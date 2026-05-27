//! Session helpers — wraps `pie_agent_core::JsonlSessionRepo` with resume / list / delete
//! semantics scoped to the current cwd hash.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use pie_agent_core::{JsonlSessionRepo, Session};

use crate::config::sessions_dir_for_cwd;

pub struct SessionEntry {
    #[allow(dead_code)] // listed via the public API; not read by the CLI itself.
    pub path: PathBuf,
    pub id: String,
    pub created_at: String,
    pub preview: Option<String>,
}

pub async fn open_repo(cwd: &std::path::Path) -> JsonlSessionRepo {
    JsonlSessionRepo::new(sessions_dir_for_cwd(cwd))
}

/// Dynamic trigger rules are session-scoped sidecars next to the jsonl transcript.
pub fn trigger_sidecar_path(session_path: &std::path::Path) -> PathBuf {
    session_path.with_extension("triggers.json")
}

/// Cron jobs are session-scoped by default, parallel to dynamic trigger sidecars.
pub fn cron_sidecar_path(session_path: &std::path::Path) -> PathBuf {
    session_path.with_extension("cron.toml")
}

/// Return the dynamic-trigger sidecar for a live session.
///
/// Jsonl sessions record their absolute transcript path in metadata. Older or synthetic
/// sessions may not have that field, so keep a deterministic fallback under the repo root.
pub async fn trigger_sidecar_path_for_session(
    session: &Session,
    repo: &JsonlSessionRepo,
) -> Result<PathBuf> {
    let metadata = session.storage().get_metadata_json().await?;
    if let Some(path) = metadata.get("path").and_then(|v| v.as_str()) {
        return Ok(trigger_sidecar_path(std::path::Path::new(path)));
    }

    let session_id = metadata
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown-session");
    Ok(repo.root().join(format!("{session_id}.triggers.json")))
}

/// Return the cron sidecar for a live session.
pub async fn cron_sidecar_path_for_session(
    session: &Session,
    repo: &JsonlSessionRepo,
) -> Result<PathBuf> {
    let metadata = session.storage().get_metadata_json().await?;
    if let Some(path) = metadata.get("path").and_then(|v| v.as_str()) {
        return Ok(cron_sidecar_path(std::path::Path::new(path)));
    }

    let session_id = metadata
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown-session");
    Ok(repo.root().join(format!("{session_id}.cron.toml")))
}

/// Create a brand-new session under the current cwd's sessions dir.
pub async fn create(repo: &JsonlSessionRepo, cwd: &std::path::Path) -> Result<Session> {
    Ok(repo.create(cwd.to_string_lossy().to_string()).await?)
}

/// Resume the most recent session for this cwd, or a specific one by id when supplied.
pub async fn resume(repo: &JsonlSessionRepo, explicit_id: Option<&str>) -> Result<Session> {
    let files = repo.list().await?;
    if files.is_empty() {
        bail!("no sessions to resume in {}", repo.root().display());
    }
    let chosen = if let Some(id) = explicit_id {
        find_session_path(repo, &files, id)
            .await?
            .with_context(|| format!("no session matches id {id}"))?
    } else {
        // JsonlSessionRepo::list() sorts ascending by name (UUIDv7), so the tail is newest.
        files.last().cloned().unwrap()
    };
    Ok(repo.open(&chosen).await?)
}

/// List sessions for this cwd, oldest → newest, with a short preview from the first user
/// message when available.
pub async fn list_entries(repo: &JsonlSessionRepo) -> Result<Vec<SessionEntry>> {
    let mut out = Vec::new();
    for path in repo.list().await? {
        let session = repo.open(&path).await?;
        let meta = session.storage().get_metadata_json().await?;
        let id = meta
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let created_at = meta
            .get("createdAt")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let preview = first_user_text(&session).await;
        out.push(SessionEntry {
            path,
            id,
            created_at,
            preview,
        });
    }
    Ok(out)
}

async fn first_user_text(session: &Session) -> Option<String> {
    let entries = session.entries().await.ok()?;
    for e in entries {
        if let pie_agent_core::SessionTreeEntry::Message {
            message: pie_agent_core::AgentMessage::Llm(pie_ai::Message::User(u)),
            ..
        } = e
        {
            let text = match u.content {
                pie_ai::UserContent::Text(s) => s,
                pie_ai::UserContent::Blocks(blocks) => blocks
                    .into_iter()
                    .filter_map(|b| match b {
                        pie_ai::UserContentBlock::Text(t) => Some(t.text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            };
            // Char-bounded truncation: `String::truncate` works in bytes and panics if
            // the cutoff lands inside a multi-byte UTF-8 character (CJK / emoji).
            let text = text.replace('\n', " ");
            let preview = if text.chars().count() > 80 {
                let mut p: String = text.chars().take(80).collect();
                p.push('…');
                p
            } else {
                text
            };
            return Some(preview);
        }
    }
    None
}

/// Delete a session by id (full UUIDv7) or a unique prefix.
pub async fn delete_by_id(repo: &JsonlSessionRepo, id: &str) -> Result<PathBuf> {
    let files = repo.list().await?;
    let path = find_session_path(repo, &files, id)
        .await?
        .with_context(|| format!("no session matches id {id}"))?;
    repo.delete(&path).await?;
    let trigger_sidecar = trigger_sidecar_path(&path);
    match tokio::fs::remove_file(&trigger_sidecar).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("delete {}", trigger_sidecar.display())),
    }
    let cron_sidecar = cron_sidecar_path(&path);
    match tokio::fs::remove_file(&cron_sidecar).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("delete {}", cron_sidecar.display())),
    }
    Ok(path)
}

async fn find_session_path(
    repo: &JsonlSessionRepo,
    files: &[PathBuf],
    id: &str,
) -> Result<Option<PathBuf>> {
    for path in files {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem == id || stem.starts_with(id) {
            return Ok(Some(path.clone()));
        }

        let session = repo.open(path).await?;
        let metadata_id = session
            .storage()
            .get_metadata_json()
            .await?
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if metadata_id
            .as_deref()
            .map(|s| s == id || s.starts_with(id))
            .unwrap_or(false)
        {
            return Ok(Some(path.clone()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn resume_matches_legacy_metadata_id_when_file_stem_differs() {
        let dir = tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path());
        let path = dir.path().join("file-id.jsonl");
        std::fs::write(
            &path,
            serde_json::json!({
                "id": "metadata-id",
                "createdAt": "2026-01-01T00:00:00Z",
                "cwd": "/cwd",
                "path": path.to_string_lossy(),
            })
            .to_string()
                + "\n",
        )
        .unwrap();

        let session = resume(&repo, Some("metadata")).await.unwrap();
        let meta = session.storage().get_metadata_json().await.unwrap();
        assert_eq!(meta.get("id").and_then(|v| v.as_str()), Some("metadata-id"));
    }

    #[tokio::test]
    async fn resume_with_no_id_picks_most_recent_session() {
        // UUIDv7 is time-ordered, so the lexically-greatest filename in the sessions dir is
        // the newest one. Verify resume() picks it when called with no explicit id (which is
        // what `pie -c / --continue` ends up doing).
        let dir = tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path());

        // First, older session.
        let older = repo.create("/cwd").await.unwrap();
        let older_id = older
            .storage()
            .get_metadata_json()
            .await
            .unwrap()
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();
        // tiny sleep to ensure the UUIDv7 timestamp slot bumps for the next create
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let newer = repo.create("/cwd").await.unwrap();
        let newer_id = newer
            .storage()
            .get_metadata_json()
            .await
            .unwrap()
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();
        assert_ne!(older_id, newer_id);

        let picked = resume(&repo, None).await.unwrap();
        let picked_id = picked
            .storage()
            .get_metadata_json()
            .await
            .unwrap()
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();
        assert_eq!(
            picked_id, newer_id,
            "resume() with no id should pick the most recent session"
        );
    }

    #[tokio::test]
    async fn delete_matches_legacy_metadata_id_when_file_stem_differs() {
        let dir = tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path());
        let path = dir.path().join("file-id.jsonl");
        std::fs::write(
            &path,
            serde_json::json!({
                "id": "metadata-id",
                "createdAt": "2026-01-01T00:00:00Z",
                "cwd": "/cwd",
                "path": path.to_string_lossy(),
            })
            .to_string()
                + "\n",
        )
        .unwrap();

        let deleted = delete_by_id(&repo, "metadata").await.unwrap();
        assert_eq!(deleted, path);
        assert!(!deleted.exists());
    }

    #[test]
    fn trigger_sidecar_path_lives_next_to_session_file() {
        let path = std::path::Path::new("/tmp/session-id.jsonl");
        assert_eq!(
            trigger_sidecar_path(path),
            std::path::PathBuf::from("/tmp/session-id.triggers.json")
        );
        assert_eq!(
            cron_sidecar_path(path),
            std::path::PathBuf::from("/tmp/session-id.cron.toml")
        );
    }

    #[tokio::test]
    async fn sidecar_paths_survive_session_resume() {
        let dir = tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path());
        let created = repo.create("/cwd").await.unwrap();
        let metadata = created.storage().get_metadata_json().await.unwrap();
        let session_id = metadata.get("id").and_then(|v| v.as_str()).unwrap();
        let session_path = metadata.get("path").and_then(|v| v.as_str()).unwrap();
        let expected_trigger = trigger_sidecar_path(std::path::Path::new(session_path));
        let expected_cron = cron_sidecar_path(std::path::Path::new(session_path));

        std::fs::write(&expected_trigger, "{\"version\":1,\"rules\":[]}").unwrap();
        std::fs::write(&expected_cron, "[[jobs]]\n").unwrap();
        let resumed = resume(&repo, Some(session_id)).await.unwrap();

        assert_eq!(
            trigger_sidecar_path_for_session(&resumed, &repo)
                .await
                .unwrap(),
            expected_trigger
        );
        assert_eq!(
            cron_sidecar_path_for_session(&resumed, &repo)
                .await
                .unwrap(),
            expected_cron
        );
    }

    #[tokio::test]
    async fn cron_sidecar_is_session_specific() {
        let dir = tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path());
        let first = repo.create("/cwd").await.unwrap();
        let second = repo.create("/cwd").await.unwrap();

        let first_path = cron_sidecar_path_for_session(&first, &repo).await.unwrap();
        let second_path = cron_sidecar_path_for_session(&second, &repo).await.unwrap();

        assert_ne!(first_path, second_path);
        std::fs::write(&first_path, "[[jobs]]\n").unwrap();
        assert!(first_path.exists());
        assert!(
            !second_path.exists(),
            "a new session must not inherit another session's cron sidecar"
        );
    }

    #[tokio::test]
    async fn delete_removes_session_sidecars() {
        let dir = tempdir().unwrap();
        let repo = JsonlSessionRepo::new(dir.path());
        let session = repo.create("/cwd").await.unwrap();
        let id = session
            .storage()
            .get_metadata_json()
            .await
            .unwrap()
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();
        let session_path = repo.list().await.unwrap().pop().unwrap();
        let trigger_path = trigger_sidecar_path(&session_path);
        let cron_path = cron_sidecar_path(&session_path);
        std::fs::write(&trigger_path, "{}").unwrap();
        std::fs::write(&cron_path, "[[jobs]]\n").unwrap();

        let deleted = delete_by_id(&repo, &id).await.unwrap();

        assert_eq!(deleted, session_path);
        assert!(!deleted.exists());
        assert!(!trigger_path.exists());
        assert!(!cron_path.exists());
    }
}
