//! Triage inbox (issue #23 — docs/issues/23-loops-inbox.md).
//!
//! Where loop findings land instead of interrupting the main chat or sinking into the
//! audit log. Global JSONL at `~/.pie/inbox.jsonl` (override root for tests): loops run
//! per-session, but the inbox is what you open in the morning, wherever they ran.
//!
//! Concurrency: writes within this process serialize through a mutex. Cross-process
//! appends interleave fine (line-oriented); status rewrites are last-writer-wins —
//! acceptable for v1. Unparseable lines are skipped on read, never deleted.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Cap on a single finding's text.
pub const MAX_ENTRY_TEXT_CHARS: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxStatus {
    New,
    Claimed,
    Dismissed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InboxEntry {
    pub id: String,
    pub created_at: String,
    /// Bounded origin label, e.g. `cron:<job-id-prefix>`.
    pub source: String,
    pub text: String,
    pub trace_id: String,
    pub session_id: String,
    pub status: InboxStatus,
}

static WRITE_LOCK: Mutex<()> = Mutex::new(());

pub fn default_inbox_path() -> PathBuf {
    crate::config::base_dir().join("inbox.jsonl")
}

/// Append a finding. Text is trimmed and capped at [`MAX_ENTRY_TEXT_CHARS`].
pub fn append(
    path: &Path,
    source: &str,
    text: &str,
    trace_id: &str,
    session_id: &str,
) -> Result<InboxEntry> {
    let trimmed = text.trim();
    let text = if trimmed.chars().count() > MAX_ENTRY_TEXT_CHARS {
        let mut capped: String = trimmed.chars().take(MAX_ENTRY_TEXT_CHARS).collect();
        capped.push('…');
        capped
    } else {
        trimmed.to_string()
    };
    let entry = InboxEntry {
        id: format!("inb-{}", uuid::Uuid::new_v4().simple()),
        created_at: chrono::Utc::now().to_rfc3339(),
        source: source.chars().take(80).collect(),
        text,
        trace_id: trace_id.to_string(),
        session_id: session_id.to_string(),
        status: InboxStatus::New,
    };
    let _guard = WRITE_LOCK.lock();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let line = serde_json::to_string(&entry)? + "\n";
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("append {}", path.display()))?;
    Ok(entry)
}

/// All entries, oldest first. Unparseable lines are skipped.
pub fn list(path: &Path) -> Result<Vec<InboxEntry>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str::<InboxEntry>(line).ok())
        .collect())
}

/// Entries with status `new`, oldest first.
pub fn list_new(path: &Path) -> Result<Vec<InboxEntry>> {
    Ok(list(path)?
        .into_iter()
        .filter(|entry| entry.status == InboxStatus::New)
        .collect())
}

/// Count of `new` entries; 0 when the file is missing or unreadable (badge path —
/// rendering must never fail on inbox problems).
pub fn new_count(path: &Path) -> usize {
    list_new(path).map(|entries| entries.len()).unwrap_or(0)
}

/// Set one entry's status by id. Returns the updated entry, or `None` when absent.
pub fn set_status(path: &Path, id: &str, status: InboxStatus) -> Result<Option<InboxEntry>> {
    let _guard = WRITE_LOCK.lock();
    let mut entries = list(path)?;
    let mut updated = None;
    for entry in &mut entries {
        if entry.id == id {
            entry.status = status.clone();
            updated = Some(entry.clone());
        }
    }
    if updated.is_some() {
        rewrite(path, &entries)?;
    }
    Ok(updated)
}

/// Dismiss every `new` entry; returns how many changed.
pub fn dismiss_all_new(path: &Path) -> Result<usize> {
    let _guard = WRITE_LOCK.lock();
    let mut entries = list(path)?;
    let mut changed = 0usize;
    for entry in &mut entries {
        if entry.status == InboxStatus::New {
            entry.status = InboxStatus::Dismissed;
            changed += 1;
        }
    }
    if changed > 0 {
        rewrite(path, &entries)?;
    }
    Ok(changed)
}

/// Rewrite the full file (status changes). Corrupt lines were already dropped by
/// `list`, which is acceptable on an explicit mutation.
fn rewrite(path: &Path, entries: &[InboxEntry]) -> Result<()> {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&serde_json::to_string(entry)?);
        out.push('\n');
    }
    std::fs::write(path, out).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_inbox() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.jsonl");
        (dir, path)
    }

    #[test]
    fn append_list_claim_dismiss_round_trip() {
        let (_dir, path) = temp_inbox();
        assert_eq!(new_count(&path), 0, "missing file counts as zero");

        let a = append(
            &path,
            "cron:job-1",
            "found a flaky test",
            "trace-a",
            "sess-1",
        )
        .unwrap();
        let b = append(
            &path,
            "cron:job-2",
            "  PR #9 needs rebase  ",
            "trace-b",
            "sess-1",
        )
        .unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(b.text, "PR #9 needs rebase", "text must be trimmed");

        let entries = list_new(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, a.id, "oldest first");
        assert_eq!(new_count(&path), 2);

        let claimed = set_status(&path, &a.id, InboxStatus::Claimed)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.status, InboxStatus::Claimed);
        assert_eq!(new_count(&path), 1);
        assert!(
            set_status(&path, "inb-missing", InboxStatus::Claimed)
                .unwrap()
                .is_none()
        );

        assert_eq!(dismiss_all_new(&path).unwrap(), 1);
        assert_eq!(new_count(&path), 0);
        // History preserved: claimed + dismissed entries still listed.
        assert_eq!(list(&path).unwrap().len(), 2);
    }

    #[test]
    fn oversized_text_is_capped_and_corrupt_lines_skipped() {
        let (_dir, path) = temp_inbox();
        let long = "x".repeat(2000);
        let entry = append(&path, "cron:j", &long, "t", "s").unwrap();
        assert!(
            entry.text.chars().count() <= MAX_ENTRY_TEXT_CHARS + 1,
            "capped (plus ellipsis)"
        );

        // A corrupt line in the middle must not break reads or later appends.
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "{{not json").unwrap();
        }
        append(&path, "cron:j", "after corruption", "t2", "s").unwrap();
        let entries = list(&path).unwrap();
        assert_eq!(
            entries.len(),
            2,
            "corrupt line skipped, both real entries read"
        );
        assert_eq!(entries[1].text, "after corruption");
    }
}
