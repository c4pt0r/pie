//! Session entry types + `Session` facade. 1:1 port of
//! `packages/agent/src/harness/session/session.ts` plus the entry types from `harness/types.ts`.
//!
//! Append-only jsonl model: every entry is one of [`SessionTreeEntry`]'s tagged variants. The
//! `Session` struct wraps a [`SessionStorage`] trait object and adds typed `append_*` helpers
//! plus `build_context` (parent-chain replay).

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::AgentMessage;

use super::super::messages::{branch_summary, compaction_summary, custom};
use super::super::types::{SessionError, SessionErrorCode};

// ──────────────────────────────────────────────────────────────────────────────────────────
// Entry types
// ──────────────────────────────────────────────────────────────────────────────────────────

/// One row in a session jsonl. Tagged by `type`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionTreeEntry {
    Message {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        message: AgentMessage,
    },
    ThinkingLevelChange {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "thinkingLevel")]
        thinking_level: String,
    },
    ModelChange {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    Compaction {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        summary: String,
        #[serde(rename = "firstKeptEntryId")]
        first_kept_entry_id: String,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default, rename = "fromHook", skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
    },
    BranchSummary {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "fromId")]
        from_id: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default, rename = "fromHook", skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
    },
    Custom {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "customType")]
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    CustomMessage {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "customType")]
        custom_type: String,
        content: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        display: bool,
    },
    Label {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "targetId")]
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    SessionInfo {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Leaf {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: String,
        #[serde(rename = "targetId")]
        target_id: Option<String>,
    },
}

impl SessionTreeEntry {
    pub fn id(&self) -> &str {
        match self {
            Self::Message { id, .. }
            | Self::ThinkingLevelChange { id, .. }
            | Self::ModelChange { id, .. }
            | Self::Compaction { id, .. }
            | Self::BranchSummary { id, .. }
            | Self::Custom { id, .. }
            | Self::CustomMessage { id, .. }
            | Self::Label { id, .. }
            | Self::SessionInfo { id, .. }
            | Self::Leaf { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Self::Message { parent_id, .. }
            | Self::ThinkingLevelChange { parent_id, .. }
            | Self::ModelChange { parent_id, .. }
            | Self::Compaction { parent_id, .. }
            | Self::BranchSummary { parent_id, .. }
            | Self::Custom { parent_id, .. }
            | Self::CustomMessage { parent_id, .. }
            | Self::Label { parent_id, .. }
            | Self::SessionInfo { parent_id, .. }
            | Self::Leaf { parent_id, .. } => parent_id.as_deref(),
        }
    }

    pub fn type_str(&self) -> &'static str {
        match self {
            Self::Message { .. } => "message",
            Self::ThinkingLevelChange { .. } => "thinking_level_change",
            Self::ModelChange { .. } => "model_change",
            Self::Compaction { .. } => "compaction",
            Self::BranchSummary { .. } => "branch_summary",
            Self::Custom { .. } => "custom",
            Self::CustomMessage { .. } => "custom_message",
            Self::Label { .. } => "label",
            Self::SessionInfo { .. } => "session_info",
            Self::Leaf { .. } => "leaf",
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Context + metadata
// ──────────────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub model: Option<SessionContextModel>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionContextModel {
    pub provider: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub id: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JsonlSessionMetadata {
    #[serde(flatten)]
    pub base: SessionMetadata,
    pub cwd: String,
    pub path: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "parentSessionPath"
    )]
    pub parent_session_path: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "importedFrom"
    )]
    pub imported_from: Option<SessionImportOrigin>,
}

/// Provenance of a session created by importing a `.piesession` archive. The importing
/// environment assigns a fresh id; this records where the transcript originally came from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionImportOrigin {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub cwd: String,
    #[serde(rename = "exportedAt")]
    pub exported_at: String,
    #[serde(rename = "pieVersion")]
    pub pie_version: String,
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// SessionStorage trait
// ──────────────────────────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait SessionStorage: Send + Sync {
    async fn get_metadata_json(&self) -> Result<Value, SessionError>;
    async fn get_leaf_id(&self) -> Result<Option<String>, SessionError>;
    async fn set_leaf_id(&self, id: Option<String>) -> Result<(), SessionError>;
    async fn create_entry_id(&self) -> Result<String, SessionError>;
    async fn append_entry(&self, entry: SessionTreeEntry) -> Result<(), SessionError>;
    async fn get_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, SessionError>;
    async fn get_entries(&self) -> Result<Vec<SessionTreeEntry>, SessionError>;
    async fn get_path_to_root(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Vec<SessionTreeEntry>, SessionError>;
    async fn find_entries(&self, entry_type: &str) -> Result<Vec<SessionTreeEntry>, SessionError>;
    async fn get_label(&self, id: &str) -> Result<Option<String>, SessionError>;
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Replay
// ──────────────────────────────────────────────────────────────────────────────────────────

pub fn build_session_context(path_entries: &[SessionTreeEntry]) -> SessionContext {
    let mut thinking_level = String::from("off");
    let mut model: Option<SessionContextModel> = None;
    let mut compaction_idx: Option<usize> = None;

    for (i, entry) in path_entries.iter().enumerate() {
        match entry {
            SessionTreeEntry::ThinkingLevelChange {
                thinking_level: t, ..
            } => {
                thinking_level = t.clone();
            }
            SessionTreeEntry::ModelChange {
                provider, model_id, ..
            } => {
                model = Some(SessionContextModel {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                });
            }
            SessionTreeEntry::Message {
                message: AgentMessage::Llm(pie_ai::Message::Assistant(a)),
                ..
            } => {
                model = Some(SessionContextModel {
                    provider: a.provider.0.clone(),
                    model_id: a.model.clone(),
                });
            }
            SessionTreeEntry::Compaction { .. } => {
                compaction_idx = Some(i);
            }
            _ => {}
        }
    }

    let mut messages: Vec<AgentMessage> = Vec::new();
    let append = |messages: &mut Vec<AgentMessage>, entry: &SessionTreeEntry| match entry {
        SessionTreeEntry::Message { message, .. } => messages.push(message.clone()),
        SessionTreeEntry::CustomMessage {
            custom_type,
            content,
            details,
            timestamp,
            ..
        } => {
            let ts = chrono::DateTime::parse_from_rfc3339(timestamp)
                .map(|d| d.timestamp_millis())
                .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis());
            messages.push(custom(
                custom_type.clone(),
                serde_json::json!({ "content": content, "details": details, "timestamp": ts }),
            ));
        }
        SessionTreeEntry::BranchSummary { summary, .. } if !summary.is_empty() => {
            messages.push(branch_summary(summary.clone()));
        }
        _ => {}
    };

    if let Some(idx) = compaction_idx {
        let SessionTreeEntry::Compaction {
            summary,
            first_kept_entry_id,
            ..
        } = &path_entries[idx]
        else {
            unreachable!()
        };
        messages.push(compaction_summary(summary.clone()));
        let mut found_first_kept = false;
        for (i, entry) in path_entries.iter().enumerate() {
            if i >= idx {
                break;
            }
            if entry.id() == first_kept_entry_id.as_str() {
                found_first_kept = true;
            }
            if found_first_kept {
                append(&mut messages, entry);
            }
        }
        for entry in &path_entries[idx + 1..] {
            append(&mut messages, entry);
        }
    } else {
        for entry in path_entries {
            append(&mut messages, entry);
        }
    }

    SessionContext {
        messages,
        thinking_level,
        model,
    }
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Session facade
// ──────────────────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Session {
    storage: Arc<dyn SessionStorage>,
}

impl Session {
    pub fn new(storage: Arc<dyn SessionStorage>) -> Self {
        Self { storage }
    }

    pub fn storage(&self) -> &Arc<dyn SessionStorage> {
        &self.storage
    }

    fn not_found(msg: impl Into<String>) -> SessionError {
        SessionError {
            code: SessionErrorCode::NotFound,
            message: msg.into(),
        }
    }

    fn now_rfc3339() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    pub async fn leaf_id(&self) -> Result<Option<String>, SessionError> {
        self.storage.get_leaf_id().await
    }

    pub async fn get_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, SessionError> {
        self.storage.get_entry(id).await
    }

    pub async fn entries(&self) -> Result<Vec<SessionTreeEntry>, SessionError> {
        self.storage.get_entries().await
    }

    pub async fn branch(
        &self,
        from_id: Option<&str>,
    ) -> Result<Vec<SessionTreeEntry>, SessionError> {
        let leaf = match from_id {
            Some(id) => Some(id.to_string()),
            None => self.storage.get_leaf_id().await?,
        };
        self.storage.get_path_to_root(leaf.as_deref()).await
    }

    pub async fn build_context(&self) -> Result<SessionContext, SessionError> {
        let branch = self.branch(None).await?;
        Ok(build_session_context(&branch))
    }

    pub async fn label(&self, id: &str) -> Result<Option<String>, SessionError> {
        self.storage.get_label(id).await
    }

    pub async fn session_name(&self) -> Result<Option<String>, SessionError> {
        let entries = self.storage.find_entries("session_info").await?;
        for entry in entries.into_iter().rev() {
            if let SessionTreeEntry::SessionInfo {
                name: Some(name), ..
            } = entry
            {
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    return Ok(Some(trimmed.to_string()));
                }
            }
        }
        Ok(None)
    }

    async fn append_typed(&self, entry: SessionTreeEntry) -> Result<String, SessionError> {
        let id = entry.id().to_string();
        self.storage.append_entry(entry).await?;
        Ok(id)
    }

    pub async fn append_message(&self, message: AgentMessage) -> Result<String, SessionError> {
        let id = self.storage.create_entry_id().await?;
        let parent = self.storage.get_leaf_id().await?;
        self.append_typed(SessionTreeEntry::Message {
            id,
            parent_id: parent,
            timestamp: Self::now_rfc3339(),
            message,
        })
        .await
    }

    pub async fn append_thinking_level_change(
        &self,
        thinking_level: impl Into<String>,
    ) -> Result<String, SessionError> {
        let id = self.storage.create_entry_id().await?;
        let parent = self.storage.get_leaf_id().await?;
        self.append_typed(SessionTreeEntry::ThinkingLevelChange {
            id,
            parent_id: parent,
            timestamp: Self::now_rfc3339(),
            thinking_level: thinking_level.into(),
        })
        .await
    }

    pub async fn append_model_change(
        &self,
        provider: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Result<String, SessionError> {
        let id = self.storage.create_entry_id().await?;
        let parent = self.storage.get_leaf_id().await?;
        self.append_typed(SessionTreeEntry::ModelChange {
            id,
            parent_id: parent,
            timestamp: Self::now_rfc3339(),
            provider: provider.into(),
            model_id: model_id.into(),
        })
        .await
    }

    pub async fn append_compaction(
        &self,
        summary: impl Into<String>,
        first_kept_entry_id: impl Into<String>,
        tokens_before: u64,
        details: Option<Value>,
        from_hook: bool,
    ) -> Result<String, SessionError> {
        let id = self.storage.create_entry_id().await?;
        let parent = self.storage.get_leaf_id().await?;
        self.append_typed(SessionTreeEntry::Compaction {
            id,
            parent_id: parent,
            timestamp: Self::now_rfc3339(),
            summary: summary.into(),
            first_kept_entry_id: first_kept_entry_id.into(),
            tokens_before,
            details,
            from_hook: if from_hook { Some(true) } else { None },
        })
        .await
    }

    pub async fn append_custom(
        &self,
        custom_type: impl Into<String>,
        data: Option<Value>,
    ) -> Result<String, SessionError> {
        let id = self.storage.create_entry_id().await?;
        let parent = self.storage.get_leaf_id().await?;
        self.append_typed(SessionTreeEntry::Custom {
            id,
            parent_id: parent,
            timestamp: Self::now_rfc3339(),
            custom_type: custom_type.into(),
            data,
        })
        .await
    }

    pub async fn append_label(
        &self,
        target_id: impl Into<String>,
        label: Option<String>,
    ) -> Result<String, SessionError> {
        let target = target_id.into();
        if self.storage.get_entry(&target).await?.is_none() {
            return Err(Self::not_found(format!("Entry {target} not found")));
        }
        let id = self.storage.create_entry_id().await?;
        let parent = self.storage.get_leaf_id().await?;
        self.append_typed(SessionTreeEntry::Label {
            id,
            parent_id: parent,
            timestamp: Self::now_rfc3339(),
            target_id: target,
            label,
        })
        .await
    }

    pub async fn append_session_name(
        &self,
        name: impl Into<String>,
    ) -> Result<String, SessionError> {
        let id = self.storage.create_entry_id().await?;
        let parent = self.storage.get_leaf_id().await?;
        let n = name.into().trim().to_string();
        self.append_typed(SessionTreeEntry::SessionInfo {
            id,
            parent_id: parent,
            timestamp: Self::now_rfc3339(),
            name: Some(n),
        })
        .await
    }

    pub async fn move_to(
        &self,
        entry_id: Option<&str>,
        summary: Option<BranchSummaryInput>,
    ) -> Result<Option<String>, SessionError> {
        if let Some(id) = entry_id {
            if self.storage.get_entry(id).await?.is_none() {
                return Err(Self::not_found(format!("Entry {id} not found")));
            }
        }
        self.storage.set_leaf_id(entry_id.map(String::from)).await?;
        let Some(summary) = summary else {
            return Ok(None);
        };
        let id = self.storage.create_entry_id().await?;
        let from_id = entry_id.map(String::from).unwrap_or_else(|| "root".into());
        let entry = SessionTreeEntry::BranchSummary {
            id,
            parent_id: entry_id.map(String::from),
            timestamp: Self::now_rfc3339(),
            from_id,
            summary: summary.summary,
            details: summary.details,
            from_hook: if summary.from_hook { Some(true) } else { None },
        };
        Ok(Some(self.append_typed(entry).await?))
    }
}

#[derive(Clone, Debug, Default)]
pub struct BranchSummaryInput {
    pub summary: String,
    pub details: Option<Value>,
    pub from_hook: bool,
}
