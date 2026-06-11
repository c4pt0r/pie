//! Append-only jsonl `SessionStorage`. 1:1 port of
//! `packages/agent/src/harness/session/jsonl-storage.ts` (~293 lines).
//!
//! Layout: line 1 is the JSON-encoded `JsonlSessionMetadata` header. Subsequent lines are
//! `SessionTreeEntry` rows in append order. No in-place edits; the leaf pointer is derived from
//! the latest `leaf` entry (or, if absent, the last appended row).

use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

use super::super::types::{SessionError, SessionErrorCode};
use super::session::{JsonlSessionMetadata, SessionMetadata, SessionStorage, SessionTreeEntry};
use super::uuid::uuidv7;

pub struct JsonlSessionStorage {
    path: PathBuf,
    metadata: JsonlSessionMetadata,
    // In-process cache. Reads parse the file lazily on first use; subsequent calls hit this.
    cache: Mutex<Option<Vec<SessionTreeEntry>>>,
}

impl JsonlSessionStorage {
    /// Create a fresh session file at `path`, writing the header. Errors if the file exists.
    pub async fn create(
        path: impl Into<PathBuf>,
        cwd: impl Into<String>,
    ) -> Result<Self, SessionError> {
        let path = path.into();
        if path.exists() {
            return Err(SessionError {
                code: SessionErrorCode::AlreadyExists,
                message: format!("{} already exists", path.display()),
            });
        }
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(uuidv7);
        let metadata = JsonlSessionMetadata {
            base: SessionMetadata {
                id,
                created_at: chrono::Utc::now().to_rfc3339(),
            },
            cwd: cwd.into(),
            path: path.to_string_lossy().to_string(),
            parent_session_path: None,
            imported_from: None,
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.map_err(io_err)?;
        }
        let header = serde_json::to_string(&metadata).map_err(json_err)? + "\n";
        fs::write(&path, header).await.map_err(io_err)?;
        Ok(Self {
            path,
            metadata,
            cache: Mutex::new(Some(Vec::new())),
        })
    }

    /// Open an existing session file. Parses the header to recover metadata.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let raw = fs::read_to_string(&path).await.map_err(io_err)?;
        let mut lines = raw.split('\n');
        let header_line = lines.next().ok_or_else(|| SessionError {
            code: SessionErrorCode::Corrupted,
            message: format!("{} is empty", path.display()),
        })?;
        let metadata: JsonlSessionMetadata =
            serde_json::from_str(header_line).map_err(|e| SessionError {
                code: SessionErrorCode::Corrupted,
                message: format!("invalid header in {}: {e}", path.display()),
            })?;
        Ok(Self {
            path,
            metadata,
            cache: Mutex::new(None),
        })
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn metadata(&self) -> &JsonlSessionMetadata {
        &self.metadata
    }

    async fn load_entries(&self) -> Result<Vec<SessionTreeEntry>, SessionError> {
        if let Some(cached) = self.cache.lock().unwrap().clone() {
            return Ok(cached);
        }
        let raw = fs::read_to_string(&self.path).await.map_err(io_err)?;
        let mut iter = raw.split('\n');
        // Skip header.
        iter.next();
        let mut out: Vec<SessionTreeEntry> = Vec::new();
        for line in iter {
            if line.trim().is_empty() {
                continue;
            }
            let entry: SessionTreeEntry = serde_json::from_str(line).map_err(|e| SessionError {
                code: SessionErrorCode::Corrupted,
                message: format!("invalid entry: {e}"),
            })?;
            out.push(entry);
        }
        *self.cache.lock().unwrap() = Some(out.clone());
        Ok(out)
    }

    fn invalidate_cache(&self) {
        *self.cache.lock().unwrap() = None;
    }

    async fn current_leaf(&self) -> Result<Option<String>, SessionError> {
        // Replay the append-only log. A `leaf` entry explicitly moves the pointer, while any
        // normal appended entry becomes the new leaf for subsequent writes.
        let entries = self.load_entries().await?;
        let mut leaf: Option<String> = None;
        for entry in &entries {
            match entry {
                SessionTreeEntry::Leaf { target_id, .. } => {
                    leaf = target_id.clone();
                }
                _ => {
                    leaf = Some(entry.id().to_string());
                }
            }
        }
        Ok(leaf)
    }
}

fn io_err(e: std::io::Error) -> SessionError {
    SessionError {
        code: SessionErrorCode::StorageFailure,
        message: e.to_string(),
    }
}

fn json_err(e: serde_json::Error) -> SessionError {
    SessionError {
        code: SessionErrorCode::Corrupted,
        message: e.to_string(),
    }
}

#[async_trait]
impl SessionStorage for JsonlSessionStorage {
    async fn get_metadata_json(&self) -> Result<Value, SessionError> {
        Ok(serde_json::to_value(&self.metadata).unwrap())
    }

    async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
        self.current_leaf().await
    }

    async fn set_leaf_id(&self, id: Option<String>) -> Result<(), SessionError> {
        // Record as an explicit `leaf` entry — append-only by design.
        let entry = SessionTreeEntry::Leaf {
            id: uuidv7(),
            parent_id: self.current_leaf().await?,
            timestamp: chrono::Utc::now().to_rfc3339(),
            target_id: id,
        };
        self.append_entry(entry).await
    }

    async fn create_entry_id(&self) -> Result<String, SessionError> {
        Ok(uuidv7())
    }

    async fn append_entry(&self, entry: SessionTreeEntry) -> Result<(), SessionError> {
        let line = serde_json::to_string(&entry).map_err(json_err)? + "\n";
        let mut f = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .await
            .map_err(io_err)?;
        f.write_all(line.as_bytes()).await.map_err(io_err)?;
        f.flush().await.map_err(io_err)?;
        self.invalidate_cache();
        Ok(())
    }

    async fn get_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, SessionError> {
        Ok(self
            .load_entries()
            .await?
            .into_iter()
            .find(|e| e.id() == id))
    }

    async fn get_entries(&self) -> Result<Vec<SessionTreeEntry>, SessionError> {
        self.load_entries().await
    }

    async fn get_path_to_root(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Vec<SessionTreeEntry>, SessionError> {
        let Some(start) = leaf_id else {
            return Ok(Vec::new());
        };
        let entries = self.load_entries().await?;
        let mut chain: Vec<SessionTreeEntry> = Vec::new();
        let mut current = Some(start.to_string());
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = current {
            if !seen.insert(id.clone()) {
                return Err(SessionError {
                    code: SessionErrorCode::Corrupted,
                    message: format!("cycle in parent chain at {id}"),
                });
            }
            let Some(entry) = entries.iter().find(|e| e.id() == id).cloned() else {
                return Err(SessionError {
                    code: SessionErrorCode::Corrupted,
                    message: format!("parent {id} not found"),
                });
            };
            current = entry.parent_id().map(String::from);
            chain.push(entry);
        }
        chain.reverse();
        Ok(chain)
    }

    async fn find_entries(&self, entry_type: &str) -> Result<Vec<SessionTreeEntry>, SessionError> {
        Ok(self
            .load_entries()
            .await?
            .into_iter()
            .filter(|e| e.type_str() == entry_type)
            .collect())
    }

    async fn get_label(&self, id: &str) -> Result<Option<String>, SessionError> {
        let mut latest: Option<String> = None;
        for entry in self.load_entries().await? {
            if let SessionTreeEntry::Label {
                target_id, label, ..
            } = entry
            {
                if target_id == id {
                    latest = label;
                }
            }
        }
        Ok(latest)
    }
}
