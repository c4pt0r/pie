//! In-memory `SessionStorage`. 1:1 port of
//! `packages/agent/src/harness/session/memory-storage.ts` (~131 lines). Used by tests and the
//! browser harness. Mirrors `jsonl-storage` behaviour without touching disk.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use super::super::types::{SessionError, SessionErrorCode};
use super::session::{SessionMetadata, SessionStorage, SessionTreeEntry};
use super::uuid::uuidv7;

#[derive(Default)]
struct Inner {
    entries: Vec<SessionTreeEntry>,
    leaf_id: Option<String>,
}

pub struct MemorySessionStorage {
    metadata: SessionMetadata,
    inner: Mutex<Inner>,
}

impl MemorySessionStorage {
    pub fn new() -> Self {
        Self {
            metadata: SessionMetadata {
                id: uuidv7(),
                created_at: chrono::Utc::now().to_rfc3339(),
            },
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn with_metadata(metadata: SessionMetadata) -> Self {
        Self {
            metadata,
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("storage mutex poisoned")
    }
}

impl Default for MemorySessionStorage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionStorage for MemorySessionStorage {
    async fn get_metadata_json(&self) -> Result<Value, SessionError> {
        Ok(serde_json::to_value(&self.metadata).unwrap())
    }

    async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
        Ok(self.lock().leaf_id.clone())
    }

    async fn set_leaf_id(&self, id: Option<String>) -> Result<(), SessionError> {
        let mut g = self.lock();
        let entry = SessionTreeEntry::Leaf {
            id: uuidv7(),
            parent_id: g.leaf_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            target_id: id.clone(),
        };
        g.entries.push(entry);
        g.leaf_id = id;
        Ok(())
    }

    async fn create_entry_id(&self) -> Result<String, SessionError> {
        Ok(uuidv7())
    }

    async fn append_entry(&self, entry: SessionTreeEntry) -> Result<(), SessionError> {
        let mut g = self.lock();
        g.leaf_id = match &entry {
            SessionTreeEntry::Leaf { target_id, .. } => target_id.clone(),
            _ => Some(entry.id().to_string()),
        };
        g.entries.push(entry);
        Ok(())
    }

    async fn get_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, SessionError> {
        Ok(self.lock().entries.iter().find(|e| e.id() == id).cloned())
    }

    async fn get_entries(&self) -> Result<Vec<SessionTreeEntry>, SessionError> {
        Ok(self.lock().entries.clone())
    }

    async fn get_path_to_root(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Vec<SessionTreeEntry>, SessionError> {
        let Some(start) = leaf_id else {
            return Ok(Vec::new());
        };
        let g = self.lock();
        let mut chain: Vec<SessionTreeEntry> = Vec::new();
        let mut current: Option<String> = Some(start.to_string());
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = current {
            if !seen.insert(id.clone()) {
                return Err(SessionError {
                    code: SessionErrorCode::Corrupted,
                    message: format!("cycle in parent chain at {id}"),
                });
            }
            let Some(entry) = g.entries.iter().find(|e| e.id() == id).cloned() else {
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
            .lock()
            .entries
            .iter()
            .filter(|e| e.type_str() == entry_type)
            .cloned()
            .collect())
    }

    async fn get_label(&self, id: &str) -> Result<Option<String>, SessionError> {
        // Walk Label entries in append order; latest non-None pointing at `id` wins.
        let mut latest: Option<String> = None;
        for entry in self.lock().entries.iter() {
            if let SessionTreeEntry::Label {
                target_id, label, ..
            } = entry
            {
                if target_id == id {
                    latest = label.clone();
                }
            }
        }
        Ok(latest)
    }
}
