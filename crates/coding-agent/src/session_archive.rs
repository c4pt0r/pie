//! `.piesession` export/import support.
//!
//! The archive is intentionally small and inspectable: a tar file with a manifest, one
//! session JSONL transcript, and optional session-scoped automation sidecars. It preserves
//! transcript/tool history, so callers must render a sensitivity warning.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use pie_agent_core::{JsonlSessionMetadata, JsonlSessionRepo, SessionTreeEntry, uuidv7};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::triggers::cron::CronJob;
use crate::triggers::dynamic::DynamicTriggerRule;

const SCHEMA: &str = "pie.session_export.v1";
const MANIFEST_PATH: &str = "manifest.json";
const SESSION_PATH: &str = "session.jsonl";
const TRIGGERS_PATH: &str = "sidecars/triggers.json";
const CRON_PATH: &str = "sidecars/cron.toml";
const MAX_MANIFEST_BYTES: usize = 128 * 1024;
const MAX_SESSION_BYTES: usize = 50 * 1024 * 1024;
const MAX_SIDECAR_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivateTriggers {
    Off,
    Ask,
    On,
}

#[derive(Debug)]
pub struct ExportSummary {
    pub output_path: PathBuf,
    pub session_id: String,
    pub entry_count: usize,
    pub has_triggers: bool,
    pub has_cron: bool,
}

#[derive(Debug)]
pub struct ImportSummary {
    pub session_id: String,
    pub session_path: PathBuf,
    pub entry_count: usize,
    pub triggers_imported: usize,
    pub cron_imported: usize,
    pub automation_enabled: bool,
    /// Ids that were enabled in the source archive. A disabled-by-default import keeps
    /// these so an interactive "activate now?" answer can restore exactly the source
    /// state (same semantics as `--activate-triggers=on`).
    pub originally_enabled_triggers: Vec<String>,
    pub originally_enabled_cron: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    schema: String,
    created_at: String,
    pie_version: String,
    source: ManifestSource,
    content: ManifestContent,
    sensitivity: ManifestSensitivity,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestSource {
    session_id: String,
    cwd: String,
    session_path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestContent {
    session_jsonl_sha256: String,
    entry_count: usize,
    active_leaf_id: Option<String>,
    has_triggers: bool,
    has_cron: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestSensitivity {
    session_transcript_preserved: bool,
    separate_auth_stores_included: bool,
    provider_credentials_included: bool,
    mcp_config_included: bool,
}

#[derive(Debug)]
struct ParsedSession {
    metadata: JsonlSessionMetadata,
    entries: Vec<SessionTreeEntry>,
    original_entry_lines: Vec<String>,
    active_leaf_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DynamicTriggerFile {
    version: u32,
    rules: Vec<DynamicTriggerRule>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CronJobsFile {
    #[serde(default)]
    jobs: Vec<CronJob>,
}

pub async fn export_session(
    session_path: &Path,
    output_path: &Path,
    exclude_triggers: bool,
) -> Result<ExportSummary> {
    let session_jsonl = tokio::fs::read_to_string(session_path)
        .await
        .with_context(|| format!("read session {}", session_path.display()))?;
    if session_jsonl.len() > MAX_SESSION_BYTES {
        bail!("session transcript is too large to export");
    }
    let parsed = parse_session_jsonl(&session_jsonl)?;
    let session_id = parsed.metadata.base.id.clone();
    let session_hash = sha256_hex(session_jsonl.as_bytes());

    let trigger_path = crate::session::trigger_sidecar_path(session_path);
    let cron_path = crate::session::cron_sidecar_path(session_path);
    let trigger_bytes = if !exclude_triggers {
        read_optional_sidecar(&trigger_path).await?
    } else {
        None
    };
    let cron_bytes = if !exclude_triggers {
        read_optional_sidecar(&cron_path).await?
    } else {
        None
    };

    let manifest = Manifest {
        schema: SCHEMA.into(),
        created_at: Utc::now().to_rfc3339(),
        pie_version: env!("CARGO_PKG_VERSION").into(),
        source: ManifestSource {
            session_id: session_id.clone(),
            cwd: parsed.metadata.cwd.clone(),
            session_path: parsed.metadata.path.clone(),
        },
        content: ManifestContent {
            session_jsonl_sha256: session_hash,
            entry_count: parsed.entries.len(),
            active_leaf_id: parsed.active_leaf_id.clone(),
            has_triggers: trigger_bytes.is_some(),
            has_cron: cron_bytes.is_some(),
        },
        sensitivity: ManifestSensitivity {
            session_transcript_preserved: true,
            separate_auth_stores_included: false,
            provider_credentials_included: false,
            mcp_config_included: false,
        },
    };

    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let output = output_path.to_path_buf();
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let session_bytes = session_jsonl.into_bytes();
    let trigger_for_tar = trigger_bytes.clone();
    let cron_for_tar = cron_bytes.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let file = create_archive_file(&output)?;
        let mut tar = tar::Builder::new(file);
        append_bytes(&mut tar, MANIFEST_PATH, &manifest_bytes)?;
        append_bytes(&mut tar, SESSION_PATH, &session_bytes)?;
        if let Some(bytes) = trigger_for_tar.as_deref() {
            append_bytes(&mut tar, TRIGGERS_PATH, bytes)?;
        }
        if let Some(bytes) = cron_for_tar.as_deref() {
            append_bytes(&mut tar, CRON_PATH, bytes)?;
        }
        tar.finish().context("finish session archive")?;
        Ok(())
    })
    .await??;

    Ok(ExportSummary {
        output_path: output_path.to_path_buf(),
        session_id,
        entry_count: parsed.entries.len(),
        has_triggers: trigger_bytes.is_some(),
        has_cron: cron_bytes.is_some(),
    })
}

pub async fn import_session(
    repo: &JsonlSessionRepo,
    archive_path: &Path,
    cwd: &Path,
    activate_triggers: ActivateTriggers,
) -> Result<ImportSummary> {
    if activate_triggers == ActivateTriggers::Ask {
        bail!(
            "activate-triggers=ask requires interactive confirmation and is not implemented yet; use off or on"
        );
    }
    let archive_path = archive_path.to_path_buf();
    let files = tokio::task::spawn_blocking(move || read_archive(&archive_path)).await??;
    let manifest_bytes = files
        .get(MANIFEST_PATH)
        .ok_or_else(|| anyhow!("session archive is missing manifest.json"))?;
    let session_bytes = files
        .get(SESSION_PATH)
        .ok_or_else(|| anyhow!("session archive is missing session.jsonl"))?;

    let manifest: Manifest =
        serde_json::from_slice(manifest_bytes).context("parse session archive manifest")?;
    if manifest.schema != SCHEMA {
        bail!("unsupported session archive schema");
    }
    let actual_hash = sha256_hex(session_bytes);
    if actual_hash != manifest.content.session_jsonl_sha256 {
        bail!("session archive checksum mismatch");
    }
    let session_text = std::str::from_utf8(session_bytes).context("session.jsonl is not UTF-8")?;
    let parsed = parse_session_jsonl(session_text)?;
    if parsed.entries.len() != manifest.content.entry_count {
        bail!("session archive entry count mismatch");
    }
    if parsed.active_leaf_id != manifest.content.active_leaf_id {
        bail!("session archive active leaf mismatch");
    }
    let automation_enabled = activate_triggers == ActivateTriggers::On;
    let trigger_sidecar = files
        .get(TRIGGERS_PATH)
        .map(|bytes| rewrite_trigger_sidecar(bytes, automation_enabled))
        .transpose()?;
    let cron_sidecar = files
        .get(CRON_PATH)
        .map(|bytes| rewrite_cron_sidecar(bytes, automation_enabled))
        .transpose()?;
    let originally_enabled_triggers = files
        .get(TRIGGERS_PATH)
        .and_then(|bytes| serde_json::from_slice::<DynamicTriggerFile>(bytes).ok())
        .map(|file| {
            file.rules
                .iter()
                .filter(|rule| rule.enabled)
                .map(|rule| rule.id.clone())
                .collect()
        })
        .unwrap_or_default();
    let originally_enabled_cron = files
        .get(CRON_PATH)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|text| toml::from_str::<CronJobsFile>(text).ok())
        .map(|file| {
            file.jobs
                .iter()
                .filter(|job| job.enabled)
                .map(|job| job.id.clone())
                .collect()
        })
        .unwrap_or_default();

    tokio::fs::create_dir_all(repo.root())
        .await
        .with_context(|| format!("create {}", repo.root().display()))?;
    let new_id = uuidv7();
    let session_path = repo.root().join(format!("{new_id}.jsonl"));
    if tokio::fs::try_exists(&session_path).await? {
        bail!("import destination already exists");
    }
    let rewritten = rewrite_session_jsonl(&parsed, &manifest, &new_id, cwd, &session_path)?;
    let temp_path = repo.root().join(format!("{new_id}.jsonl.tmp"));

    let mut sidecars: Vec<(PathBuf, String)> = Vec::new();
    let triggers_imported = match &trigger_sidecar {
        Some(rules) => {
            sidecars.push((
                crate::session::trigger_sidecar_path(&session_path),
                serde_json::to_string_pretty(rules)?,
            ));
            rules.rules.len()
        }
        None => 0,
    };
    let cron_imported = match &cron_sidecar {
        Some(jobs) => {
            sidecars.push((
                crate::session::cron_sidecar_path(&session_path),
                toml::to_string_pretty(jobs)?,
            ));
            jobs.jobs.len()
        }
        None => 0,
    };
    commit_import(repo, &session_path, &temp_path, &rewritten, &sidecars).await?;

    Ok(ImportSummary {
        session_id: new_id,
        session_path,
        entry_count: parsed.entries.len(),
        triggers_imported,
        cron_imported,
        automation_enabled,
        originally_enabled_triggers,
        originally_enabled_cron,
    })
}

/// Re-enable the given trigger/cron ids on an imported session's sidecars — the second
/// half of the interactive "activate imported automation now?" flow. Sync IO: callers run
/// from UI resolution paths; the sidecars are small.
pub fn activate_imported(
    session_path: &Path,
    trigger_ids: &[String],
    cron_ids: &[String],
) -> Result<(usize, usize)> {
    let mut triggers_enabled = 0usize;
    if !trigger_ids.is_empty() {
        let path = crate::session::trigger_sidecar_path(session_path);
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let mut file: DynamicTriggerFile =
            serde_json::from_str(&text).context("parse trigger sidecar")?;
        for rule in &mut file.rules {
            if trigger_ids.contains(&rule.id) && !rule.enabled {
                rule.enabled = true;
                triggers_enabled += 1;
            }
        }
        std::fs::write(&path, serde_json::to_string_pretty(&file)?)
            .with_context(|| format!("write {}", path.display()))?;
    }
    let mut cron_enabled = 0usize;
    if !cron_ids.is_empty() {
        let path = crate::session::cron_sidecar_path(session_path);
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let mut file: CronJobsFile = toml::from_str(&text).context("parse cron sidecar")?;
        for job in &mut file.jobs {
            if cron_ids.contains(&job.id) && !job.enabled {
                job.enabled = true;
                cron_enabled += 1;
            }
        }
        std::fs::write(&path, toml::to_string_pretty(&file)?)
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok((triggers_enabled, cron_enabled))
}

pub fn default_export_path(cwd: &Path, session_id: &str) -> PathBuf {
    let short: String = session_id.chars().take(16).collect();
    cwd.join(format!("pie-session-{short}.piesession"))
}

fn parse_session_jsonl(text: &str) -> Result<ParsedSession> {
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| anyhow!("session transcript is empty"))?;
    let metadata: JsonlSessionMetadata =
        serde_json::from_str(header).context("parse session metadata")?;
    if metadata.base.id.trim().is_empty() {
        bail!("session metadata is missing id");
    }
    let mut entries = Vec::new();
    let mut original_entry_lines = Vec::new();
    let mut seen = HashSet::new();
    let mut active_leaf_id = None;
    for (idx, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: SessionTreeEntry = serde_json::from_str(line)
            .with_context(|| format!("parse session entry line {}", idx + 2))?;
        let id = entry.id().to_string();
        if !seen.insert(id.clone()) {
            bail!("session transcript contains duplicate entry id");
        }
        if let Some(parent) = entry.parent_id()
            && !seen.contains(parent)
        {
            bail!("session transcript contains dangling parent reference");
        }
        if let SessionTreeEntry::Leaf { target_id, .. } = &entry
            && let Some(target) = target_id
            && !seen.contains(target)
        {
            bail!("session transcript contains dangling leaf target");
        }
        active_leaf_id = match &entry {
            SessionTreeEntry::Leaf { target_id, .. } => target_id.clone(),
            _ => Some(id),
        };
        entries.push(entry);
        original_entry_lines.push(line.to_string());
    }
    Ok(ParsedSession {
        metadata,
        entries,
        original_entry_lines,
        active_leaf_id,
    })
}

fn rewrite_session_jsonl(
    parsed: &ParsedSession,
    manifest: &Manifest,
    new_id: &str,
    cwd: &Path,
    path: &Path,
) -> Result<String> {
    let metadata = JsonlSessionMetadata {
        base: pie_agent_core::SessionMetadata {
            id: new_id.to_string(),
            created_at: Utc::now().to_rfc3339(),
        },
        cwd: cwd.to_string_lossy().to_string(),
        path: path.to_string_lossy().to_string(),
        parent_session_path: None,
        imported_from: Some(pie_agent_core::SessionImportOrigin {
            session_id: manifest.source.session_id.clone(),
            cwd: manifest.source.cwd.clone(),
            exported_at: manifest.created_at.clone(),
            pie_version: manifest.pie_version.clone(),
        }),
    };
    let mut out = serde_json::to_string(&metadata)?;
    out.push('\n');
    for line in &parsed.original_entry_lines {
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

/// Write all imported files with the session rename as the commit point. The session is
/// staged at `temp_path` (a non-`.jsonl` name, invisible to repo listings), replay-validated
/// there, and only renamed into place after every sidecar landed. Any failure removes
/// everything written so a failed import leaves no orphan or partial session behind.
async fn commit_import(
    repo: &JsonlSessionRepo,
    session_path: &Path,
    temp_path: &Path,
    session_content: &str,
    sidecars: &[(PathBuf, String)],
) -> Result<()> {
    let result = async {
        tokio::fs::write(temp_path, session_content)
            .await
            .with_context(|| format!("write {}", temp_path.display()))?;
        let staged = repo.open(temp_path).await?;
        staged
            .build_context()
            .await
            .context("validate imported session")?;
        for (path, content) in sidecars {
            tokio::fs::write(path, content)
                .await
                .with_context(|| format!("write {}", path.display()))?;
        }
        tokio::fs::rename(temp_path, session_path)
            .await
            .with_context(|| format!("rename into {}", session_path.display()))
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(temp_path).await;
        for (path, _) in sidecars {
            let _ = tokio::fs::remove_file(path).await;
        }
    }
    result
}

/// The archive carries the full transcript, so it is created owner-only and never
/// truncates an existing file.
fn create_archive_file(path: &Path) -> Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow!(
                "output already exists: {} (remove it or pass a different path)",
                path.display()
            )
        } else {
            anyhow::Error::new(err).context(format!("create {}", path.display()))
        }
    })
}

async fn read_optional_sidecar(path: &Path) -> Result<Option<Vec<u8>>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            if bytes.len() > MAX_SIDECAR_BYTES {
                bail!("session sidecar is too large to export");
            }
            Ok(Some(bytes))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn append_bytes<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    path: &str,
    bytes: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    tar.append_data(&mut header, path, Cursor::new(bytes))
        .with_context(|| format!("append {path}"))?;
    Ok(())
}

fn read_archive(path: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut archive = tar::Archive::new(file);
    let mut files = BTreeMap::new();
    for entry in archive.entries().context("read session archive")? {
        let entry = entry.context("read archive entry")?;
        if !entry.header().entry_type().is_file() {
            bail!("session archive contains a non-file entry");
        }
        let path = entry.path().context("read archive entry path")?;
        validate_archive_path(&path)?;
        let rel = path
            .to_str()
            .ok_or_else(|| anyhow!("session archive contains non-UTF-8 path"))?
            .to_string();
        let limit = match rel.as_str() {
            MANIFEST_PATH => MAX_MANIFEST_BYTES,
            SESSION_PATH => MAX_SESSION_BYTES,
            TRIGGERS_PATH | CRON_PATH => MAX_SIDECAR_BYTES,
            _ => bail!("session archive contains an unexpected file"),
        };
        let mut bytes = Vec::new();
        entry
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .context("read archive file")?;
        if bytes.len() > limit {
            bail!("session archive file is too large");
        }
        if files.insert(rel, bytes).is_some() {
            bail!("session archive contains duplicate file paths");
        }
    }
    Ok(files)
}

fn validate_archive_path(path: &Path) -> Result<()> {
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => bail!("session archive contains an unsafe path"),
        }
    }
    Ok(())
}

/// Activation never widens what the source had: `activate` ANDs with each rule's own
/// `enabled` flag, and `fired_at` history is preserved so fire-once rules don't re-fire.
fn rewrite_trigger_sidecar(bytes: &[u8], activate: bool) -> Result<DynamicTriggerFile> {
    let mut file: DynamicTriggerFile =
        serde_json::from_slice(bytes).context("parse trigger sidecar")?;
    for rule in &mut file.rules {
        rule.enabled = rule.enabled && activate;
    }
    Ok(file)
}

fn rewrite_cron_sidecar(bytes: &[u8], activate: bool) -> Result<CronJobsFile> {
    let text = std::str::from_utf8(bytes).context("cron sidecar is not UTF-8")?;
    let mut file: CronJobsFile = toml::from_str(text).context("parse cron sidecar")?;
    for job in &mut file.jobs {
        job.enabled = job.enabled && activate;
        job.running_trace_id = None;
        job.last_due_at = None;
        job.last_error = None;
        job.skipped_overlap_count = 0;
    }
    Ok(file)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use pie_agent_core::JsonlSessionRepo;

    #[tokio::test]
    async fn export_import_rewrites_metadata_and_disables_automation() {
        let temp = tempfile::tempdir().unwrap();
        let source_cwd = temp.path().join("source");
        let dest_cwd = temp.path().join("dest");
        tokio::fs::create_dir_all(&source_cwd).await.unwrap();
        tokio::fs::create_dir_all(&dest_cwd).await.unwrap();
        let source_repo = JsonlSessionRepo::new(temp.path().join("source-sessions"));
        let source = source_repo
            .create(source_cwd.to_string_lossy().to_string())
            .await
            .unwrap();
        source
            .append_custom(
                "test_event",
                Some(serde_json::json!({"transcript": "preserved"})),
            )
            .await
            .unwrap();
        let source_meta = source.storage().get_metadata_json().await.unwrap();
        let source_path = PathBuf::from(source_meta["path"].as_str().unwrap());

        let trigger_path = crate::session::trigger_sidecar_path(&source_path);
        let trigger_file = DynamicTriggerFile {
            version: 1,
            rules: vec![DynamicTriggerRule {
                id: "trigger-1".into(),
                condition: "when something happens".into(),
                action: "do work".into(),
                enabled: true,
                fire_once: true,
                fired_at: Some(Utc::now()),
                promote_to_chat: false,
                created_at: Utc::now(),
            }],
        };
        tokio::fs::write(
            &trigger_path,
            serde_json::to_string_pretty(&trigger_file).unwrap(),
        )
        .await
        .unwrap();

        let cron_path = crate::session::cron_sidecar_path(&source_path);
        let cron_file = CronJobsFile {
            jobs: vec![CronJob {
                id: "cron-1".into(),
                schedule: "0 * * * *".into(),
                action: "hourly work".into(),
                enabled: true,
                running_trace_id: Some("trace-secret".into()),
                last_due_at: Some(Utc::now()),
                last_fired_at: Some(Utc::now()),
                last_completed_at: None,
                last_error: Some("old error".into()),
                skipped_overlap_count: 2,
                stateful: false,
                created_at: Utc::now(),
            }],
        };
        tokio::fs::write(&cron_path, toml::to_string_pretty(&cron_file).unwrap())
            .await
            .unwrap();

        let archive = temp.path().join("backup.piesession");
        let export = export_session(&source_path, &archive, false).await.unwrap();
        assert_eq!(export.entry_count, 1);
        assert!(export.has_triggers);
        assert!(export.has_cron);

        let dest_repo = JsonlSessionRepo::new(temp.path().join("dest-sessions"));
        let imported = import_session(&dest_repo, &archive, &dest_cwd, ActivateTriggers::Off)
            .await
            .unwrap();
        assert_eq!(imported.entry_count, 1);
        assert_eq!(imported.triggers_imported, 1);
        assert_eq!(imported.cron_imported, 1);
        assert_ne!(imported.session_id, export.session_id);

        let imported_session = dest_repo.open(&imported.session_path).await.unwrap();
        let meta = imported_session
            .storage()
            .get_metadata_json()
            .await
            .unwrap();
        assert_eq!(meta["id"].as_str().unwrap(), imported.session_id);
        assert_eq!(meta["cwd"].as_str().unwrap(), dest_cwd.to_string_lossy());
        assert_eq!(
            meta["path"].as_str().unwrap(),
            imported.session_path.to_string_lossy()
        );

        let imported_triggers =
            tokio::fs::read_to_string(crate::session::trigger_sidecar_path(&imported.session_path))
                .await
                .unwrap();
        let imported_trigger_file: DynamicTriggerFile =
            serde_json::from_str(&imported_triggers).unwrap();
        assert!(!imported_trigger_file.rules[0].enabled);
        // fired_at is history: a fire-once rule that already fired must not re-fire after a
        // later manual enable, so import preserves it in every activation mode.
        assert!(imported_trigger_file.rules[0].fired_at.is_some());

        let imported_cron =
            tokio::fs::read_to_string(crate::session::cron_sidecar_path(&imported.session_path))
                .await
                .unwrap();
        let imported_cron_file: CronJobsFile = toml::from_str(&imported_cron).unwrap();
        let job = &imported_cron_file.jobs[0];
        assert!(!job.enabled);
        assert!(job.running_trace_id.is_none());
        assert!(job.last_due_at.is_none());
        assert!(job.last_error.is_none());
        assert_eq!(job.skipped_overlap_count, 0);

        let excluded_archive = temp.path().join("backup-no-automation.piesession");
        let export_without_automation = export_session(&source_path, &excluded_archive, true)
            .await
            .unwrap();
        assert!(!export_without_automation.has_triggers);
        assert!(!export_without_automation.has_cron);
        let archive_files = read_archive(&excluded_archive).unwrap();
        assert!(!archive_files.contains_key(TRIGGERS_PATH));
        assert!(!archive_files.contains_key(CRON_PATH));
        let imported_without_automation = import_session(
            &dest_repo,
            &excluded_archive,
            &dest_cwd,
            ActivateTriggers::Off,
        )
        .await
        .unwrap();
        assert_eq!(imported_without_automation.triggers_imported, 0);
        assert_eq!(imported_without_automation.cron_imported, 0);
    }

    #[test]
    fn rejects_unsafe_archive_paths() {
        assert!(validate_archive_path(Path::new("manifest.json")).is_ok());
        assert!(validate_archive_path(Path::new("sidecars/triggers.json")).is_ok());
        assert!(validate_archive_path(Path::new("../session.jsonl")).is_err());
        assert!(validate_archive_path(Path::new("/tmp/session.jsonl")).is_err());
    }

    #[tokio::test]
    async fn ask_activation_is_explicitly_rejected_until_interactive_confirm_exists() {
        let temp = tempfile::tempdir().unwrap();
        let repo = JsonlSessionRepo::new(temp.path().join("sessions"));
        let err = import_session(
            &repo,
            &temp.path().join("missing.piesession"),
            temp.path(),
            ActivateTriggers::Ask,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("activate-triggers=ask"), "{err}");
        assert!(err.contains("not implemented"), "{err}");
    }

    #[tokio::test]
    async fn export_manifest_uses_last_entry_as_leaf_without_explicit_leaf_row() {
        let temp = tempfile::tempdir().unwrap();
        let source_cwd = temp.path().join("source");
        tokio::fs::create_dir_all(&source_cwd).await.unwrap();
        let source_repo = JsonlSessionRepo::new(temp.path().join("source-sessions"));
        let source = source_repo
            .create(source_cwd.to_string_lossy().to_string())
            .await
            .unwrap();
        source
            .append_custom("first", Some(serde_json::json!({"n": 1})))
            .await
            .unwrap();
        let last_id = source
            .append_custom("second", Some(serde_json::json!({"n": 2})))
            .await
            .unwrap();
        let source_meta = source.storage().get_metadata_json().await.unwrap();
        let source_path = PathBuf::from(source_meta["path"].as_str().unwrap());

        let archive = temp.path().join("backup.piesession");
        export_session(&source_path, &archive, false).await.unwrap();

        let manifest = manifest_from_archive(&archive);
        assert_eq!(
            manifest.content.active_leaf_id.as_deref(),
            Some(last_id.as_str())
        );
    }

    #[tokio::test]
    async fn export_manifest_uses_explicit_leaf_target_not_leaf_row_id() {
        let temp = tempfile::tempdir().unwrap();
        let source_cwd = temp.path().join("source");
        tokio::fs::create_dir_all(&source_cwd).await.unwrap();
        let source_repo = JsonlSessionRepo::new(temp.path().join("source-sessions"));
        let source = source_repo
            .create(source_cwd.to_string_lossy().to_string())
            .await
            .unwrap();
        let first_id = source
            .append_custom("first", Some(serde_json::json!({"n": 1})))
            .await
            .unwrap();
        source
            .append_custom("second", Some(serde_json::json!({"n": 2})))
            .await
            .unwrap();
        source.move_to(Some(&first_id), None).await.unwrap();
        let source_meta = source.storage().get_metadata_json().await.unwrap();
        let source_path = PathBuf::from(source_meta["path"].as_str().unwrap());

        let archive = temp.path().join("backup.piesession");
        export_session(&source_path, &archive, false).await.unwrap();

        let manifest = manifest_from_archive(&archive);
        assert_eq!(
            manifest.content.active_leaf_id.as_deref(),
            Some(first_id.as_str())
        );
        let session_text = tokio::fs::read_to_string(&source_path).await.unwrap();
        let last_entry: SessionTreeEntry = serde_json::from_str(
            session_text
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap(),
        )
        .unwrap();
        assert_ne!(
            manifest.content.active_leaf_id.as_deref(),
            Some(last_entry.id())
        );
    }

    #[tokio::test]
    async fn import_rejects_manifest_active_leaf_that_does_not_match_session_jsonl() {
        let temp = tempfile::tempdir().unwrap();
        let source_cwd = temp.path().join("source");
        let dest_cwd = temp.path().join("dest");
        tokio::fs::create_dir_all(&source_cwd).await.unwrap();
        tokio::fs::create_dir_all(&dest_cwd).await.unwrap();
        let source_repo = JsonlSessionRepo::new(temp.path().join("source-sessions"));
        let source = source_repo
            .create(source_cwd.to_string_lossy().to_string())
            .await
            .unwrap();
        source
            .append_custom("entry", Some(serde_json::json!({"n": 1})))
            .await
            .unwrap();
        let source_meta = source.storage().get_metadata_json().await.unwrap();
        let source_path = PathBuf::from(source_meta["path"].as_str().unwrap());

        let archive = temp.path().join("backup.piesession");
        export_session(&source_path, &archive, false).await.unwrap();
        let mut files = read_archive(&archive).unwrap();
        let mut manifest: Manifest =
            serde_json::from_slice(files.get(MANIFEST_PATH).unwrap()).unwrap();
        manifest.content.active_leaf_id = Some("stale-leaf-id".into());
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        files.insert(MANIFEST_PATH.into(), manifest_bytes);
        let tampered_archive = temp.path().join("tampered.piesession");
        write_test_archive(&tampered_archive, &files);

        let dest_repo = JsonlSessionRepo::new(temp.path().join("dest-sessions"));
        let err = import_session(
            &dest_repo,
            &tampered_archive,
            &dest_cwd,
            ActivateTriggers::Off,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("active leaf"), "{err}");
    }

    async fn make_exported_session(
        temp: &Path,
        rules: Vec<DynamicTriggerRule>,
        jobs: Vec<CronJob>,
    ) -> (String, PathBuf) {
        let source_cwd = temp.join("source");
        tokio::fs::create_dir_all(&source_cwd).await.unwrap();
        let source_repo = JsonlSessionRepo::new(temp.join("source-sessions"));
        let source = source_repo
            .create(source_cwd.to_string_lossy().to_string())
            .await
            .unwrap();
        source
            .append_custom("entry", Some(serde_json::json!({"n": 1})))
            .await
            .unwrap();
        let source_meta = source.storage().get_metadata_json().await.unwrap();
        let source_path = PathBuf::from(source_meta["path"].as_str().unwrap());
        if !rules.is_empty() {
            let trigger_file = DynamicTriggerFile { version: 1, rules };
            tokio::fs::write(
                crate::session::trigger_sidecar_path(&source_path),
                serde_json::to_string_pretty(&trigger_file).unwrap(),
            )
            .await
            .unwrap();
        }
        if !jobs.is_empty() {
            let cron_file = CronJobsFile { jobs };
            tokio::fs::write(
                crate::session::cron_sidecar_path(&source_path),
                toml::to_string_pretty(&cron_file).unwrap(),
            )
            .await
            .unwrap();
        }
        let archive = temp.join("backup.piesession");
        let export = export_session(&source_path, &archive, false).await.unwrap();
        (export.session_id, archive)
    }

    fn test_trigger_rule(id: &str, enabled: bool, fired: bool) -> DynamicTriggerRule {
        DynamicTriggerRule {
            id: id.into(),
            condition: "when something happens".into(),
            action: "do work".into(),
            enabled,
            fire_once: true,
            fired_at: fired.then(Utc::now),
            promote_to_chat: false,
            created_at: Utc::now(),
        }
    }

    fn test_cron_job(id: &str, enabled: bool) -> CronJob {
        CronJob {
            id: id.into(),
            schedule: "0 * * * *".into(),
            action: "hourly work".into(),
            enabled,
            running_trace_id: None,
            last_due_at: None,
            last_fired_at: None,
            last_completed_at: None,
            last_error: None,
            skipped_overlap_count: 0,
            stateful: false,
            created_at: Utc::now(),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn export_archive_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let (_, archive) = make_exported_session(temp.path(), vec![], vec![]).await;
        let mode = std::fs::metadata(&archive).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "archive mode {mode:o}");
    }

    #[tokio::test]
    async fn export_refuses_to_overwrite_existing_output() {
        let temp = tempfile::tempdir().unwrap();
        let (_, archive) = make_exported_session(temp.path(), vec![], vec![]).await;
        let source_path = {
            let source_repo = JsonlSessionRepo::new(temp.path().join("source-sessions"));
            source_repo.list().await.unwrap().pop().unwrap()
        };
        let err = export_session(&source_path, &archive, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exists"), "{err}");
        let original = tokio::fs::read(&archive).await.unwrap();
        assert!(
            !original.is_empty(),
            "existing archive must not be truncated"
        );
    }

    #[tokio::test]
    async fn import_records_source_provenance_in_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let (source_id, archive) = make_exported_session(temp.path(), vec![], vec![]).await;
        let dest_cwd = temp.path().join("dest");
        tokio::fs::create_dir_all(&dest_cwd).await.unwrap();
        let dest_repo = JsonlSessionRepo::new(temp.path().join("dest-sessions"));
        let imported = import_session(&dest_repo, &archive, &dest_cwd, ActivateTriggers::Off)
            .await
            .unwrap();

        let session = dest_repo.open(&imported.session_path).await.unwrap();
        let meta = session.storage().get_metadata_json().await.unwrap();
        let origin = &meta["importedFrom"];
        assert_eq!(origin["sessionId"].as_str(), Some(source_id.as_str()));
        assert_eq!(
            origin["cwd"].as_str().map(PathBuf::from),
            Some(temp.path().join("source"))
        );
        assert!(origin["exportedAt"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(origin["pieVersion"].as_str().is_some_and(|s| !s.is_empty()));
    }

    #[tokio::test]
    async fn activation_on_preserves_source_disabled_automation() {
        let temp = tempfile::tempdir().unwrap();
        let rules = vec![
            test_trigger_rule("was-enabled", true, true),
            test_trigger_rule("was-disabled", false, false),
        ];
        let jobs = vec![
            test_cron_job("job-on", true),
            test_cron_job("job-off", false),
        ];
        let (_, archive) = make_exported_session(temp.path(), rules, jobs).await;
        let dest_cwd = temp.path().join("dest");
        tokio::fs::create_dir_all(&dest_cwd).await.unwrap();
        let dest_repo = JsonlSessionRepo::new(temp.path().join("dest-sessions"));
        let imported = import_session(&dest_repo, &archive, &dest_cwd, ActivateTriggers::On)
            .await
            .unwrap();

        let triggers =
            tokio::fs::read_to_string(crate::session::trigger_sidecar_path(&imported.session_path))
                .await
                .unwrap();
        let trigger_file: DynamicTriggerFile = serde_json::from_str(&triggers).unwrap();
        let enabled_rule = trigger_file
            .rules
            .iter()
            .find(|r| r.id == "was-enabled")
            .unwrap();
        let disabled_rule = trigger_file
            .rules
            .iter()
            .find(|r| r.id == "was-disabled")
            .unwrap();
        assert!(enabled_rule.enabled);
        assert!(
            enabled_rule.fired_at.is_some(),
            "fire-once history must survive activation"
        );
        assert!(
            !disabled_rule.enabled,
            "a rule the user disabled at the source must stay disabled"
        );

        let cron =
            tokio::fs::read_to_string(crate::session::cron_sidecar_path(&imported.session_path))
                .await
                .unwrap();
        let cron_file: CronJobsFile = toml::from_str(&cron).unwrap();
        let job_on = cron_file.jobs.iter().find(|j| j.id == "job-on").unwrap();
        let job_off = cron_file.jobs.iter().find(|j| j.id == "job-off").unwrap();
        assert!(job_on.enabled);
        assert!(
            !job_off.enabled,
            "a job the user disabled at the source must stay disabled"
        );
    }

    #[tokio::test]
    async fn failed_sidecar_write_cleans_up_partial_import() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        tokio::fs::create_dir_all(&root).await.unwrap();
        let repo = JsonlSessionRepo::new(&root);

        // Valid one-line session content for the destination.
        let staging = JsonlSessionRepo::new(temp.path().join("staging"));
        let session = staging.create("/tmp").await.unwrap();
        let meta = session.storage().get_metadata_json().await.unwrap();
        let content = tokio::fs::read_to_string(meta["path"].as_str().unwrap())
            .await
            .unwrap();

        let session_path = root.join("imported.jsonl");
        let temp_path = root.join("imported.jsonl.tmp");
        let good_sidecar = root.join("imported.triggers.json");
        // A directory at the cron sidecar path makes its write fail mid-commit.
        let bad_sidecar = root.join("imported.cron.toml");
        tokio::fs::create_dir_all(&bad_sidecar).await.unwrap();

        let sidecars = vec![
            (
                good_sidecar.clone(),
                "{\"version\":1,\"rules\":[]}".to_string(),
            ),
            (bad_sidecar.clone(), "jobs = []".to_string()),
        ];
        let err = commit_import(&repo, &session_path, &temp_path, &content, &sidecars)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("imported.cron.toml"), "{err}");

        assert!(
            !tokio::fs::try_exists(&session_path).await.unwrap(),
            "no orphan session may remain after a failed import"
        );
        assert!(!tokio::fs::try_exists(&temp_path).await.unwrap());
        assert!(
            !tokio::fs::try_exists(&good_sidecar).await.unwrap(),
            "sidecars written before the failure must be removed"
        );
    }

    #[tokio::test]
    async fn import_summary_records_originally_enabled_automation_and_activates_it() {
        let temp = tempfile::tempdir().unwrap();
        let rules = vec![
            test_trigger_rule("was-enabled", true, false),
            test_trigger_rule("was-disabled", false, false),
        ];
        let jobs = vec![
            test_cron_job("job-on", true),
            test_cron_job("job-off", false),
        ];
        let (_, archive) = make_exported_session(temp.path(), rules, jobs).await;
        let dest_cwd = temp.path().join("dest");
        tokio::fs::create_dir_all(&dest_cwd).await.unwrap();
        let dest_repo = JsonlSessionRepo::new(temp.path().join("dest-sessions"));
        let imported = import_session(&dest_repo, &archive, &dest_cwd, ActivateTriggers::Off)
            .await
            .unwrap();

        assert_eq!(imported.originally_enabled_triggers, vec!["was-enabled"]);
        assert_eq!(imported.originally_enabled_cron, vec!["job-on"]);

        let (t, c) = activate_imported(
            &imported.session_path,
            &imported.originally_enabled_triggers,
            &imported.originally_enabled_cron,
        )
        .expect("activation rewrites sidecars");
        assert_eq!((t, c), (1, 1));

        let triggers =
            tokio::fs::read_to_string(crate::session::trigger_sidecar_path(&imported.session_path))
                .await
                .unwrap();
        let trigger_file: DynamicTriggerFile = serde_json::from_str(&triggers).unwrap();
        let on = trigger_file
            .rules
            .iter()
            .find(|r| r.id == "was-enabled")
            .unwrap();
        let off = trigger_file
            .rules
            .iter()
            .find(|r| r.id == "was-disabled")
            .unwrap();
        assert!(on.enabled, "originally-enabled rule must be re-enabled");
        assert!(!off.enabled, "originally-disabled rule must stay disabled");

        let cron =
            tokio::fs::read_to_string(crate::session::cron_sidecar_path(&imported.session_path))
                .await
                .unwrap();
        let cron_file: CronJobsFile = toml::from_str(&cron).unwrap();
        assert!(
            cron_file
                .jobs
                .iter()
                .find(|j| j.id == "job-on")
                .unwrap()
                .enabled
        );
        assert!(
            !cron_file
                .jobs
                .iter()
                .find(|j| j.id == "job-off")
                .unwrap()
                .enabled
        );
    }

    #[tokio::test]
    async fn successful_import_leaves_no_temp_files() {
        let temp = tempfile::tempdir().unwrap();
        let (_, archive) = make_exported_session(temp.path(), vec![], vec![]).await;
        let dest_cwd = temp.path().join("dest");
        tokio::fs::create_dir_all(&dest_cwd).await.unwrap();
        let dest_repo = JsonlSessionRepo::new(temp.path().join("dest-sessions"));
        import_session(&dest_repo, &archive, &dest_cwd, ActivateTriggers::Off)
            .await
            .unwrap();
        let mut dir = tokio::fs::read_dir(dest_repo.root()).await.unwrap();
        while let Some(entry) = dir.next_entry().await.unwrap() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(!name.ends_with(".tmp"), "leftover temp file {name}");
        }
    }

    fn manifest_from_archive(path: &Path) -> Manifest {
        let files = read_archive(path).unwrap();
        serde_json::from_slice(files.get(MANIFEST_PATH).unwrap()).unwrap()
    }

    fn write_test_archive(path: &Path, files: &BTreeMap<String, Vec<u8>>) {
        let file = std::fs::File::create(path).unwrap();
        let mut tar = tar::Builder::new(file);
        for (archive_path, bytes) in files {
            append_bytes(&mut tar, archive_path, bytes).unwrap();
        }
        tar.finish().unwrap();
    }
}
