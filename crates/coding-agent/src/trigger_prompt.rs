use std::path::PathBuf;
use std::sync::Arc;

use chrono::{Duration, Utc};
use pie_agent_core::{OnTriggerPromptHook, Session, TriggerPromptDecision, TriggerPromptRequest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TriggerTrustDecision {
    Always,
    Block,
}

#[derive(Debug)]
pub(crate) struct UiTriggerPromptResolution {
    pub(crate) decision: TriggerPromptDecision,
    pub(crate) trust_decision: Option<TriggerTrustDecision>,
}

pub(crate) struct UiTriggerPrompt {
    pub(crate) request: TriggerPromptRequest,
    pub(crate) responder: oneshot::Sender<UiTriggerPromptResolution>,
}

impl UiTriggerPrompt {
    pub(crate) fn resolve(self, resolution: UiTriggerPromptResolution) {
        let _ = self.responder.send(resolution);
    }
}

pub(crate) fn interactive_hook(
    session: Session,
) -> (
    OnTriggerPromptHook,
    mpsc::UnboundedReceiver<UiTriggerPrompt>,
) {
    let (tx, rx) = mpsc::unbounded_channel::<UiTriggerPrompt>();
    let trust_path = crate::config::base_dir().join("hub-trust.json");
    let hook: OnTriggerPromptHook = Arc::new(move |request, cancel| {
        let tx = tx.clone();
        let session = session.clone();
        let trust_path = trust_path.clone();
        Box::pin(async move {
            let (decision_tx, decision_rx) = oneshot::channel();
            if tx
                .send(UiTriggerPrompt {
                    request: request.clone(),
                    responder: decision_tx,
                })
                .is_err()
            {
                return TriggerPromptDecision::Deny {
                    reason: Some("trigger prompt UI is unavailable".into()),
                };
            }
            let resolution = tokio::select! {
                resolution = decision_rx => resolution.unwrap_or(UiTriggerPromptResolution {
                    decision: TriggerPromptDecision::Deny {
                        reason: Some("trigger prompt UI closed before a decision".into()),
                    },
                    trust_decision: None,
                }),
                _ = cancel.cancelled() => UiTriggerPromptResolution {
                    decision: TriggerPromptDecision::Timeout {
                        reason: Some("trigger prompt cancelled".into()),
                    },
                    trust_decision: None,
                },
            };
            if let Some(trust_decision) = resolution.trust_decision
                && let Err(err) =
                    persist_trust_decision(&trust_path, &session, &request, trust_decision).await
            {
                return TriggerPromptDecision::Deny {
                    reason: Some(format!(
                        "could not persist hub trust decision: {}",
                        crate::bug_report::redact(&err)
                    )),
                };
            }
            resolution.decision
        })
    });
    (hook, rx)
}

pub(crate) fn deny_hook(reason: &'static str) -> OnTriggerPromptHook {
    Arc::new(
        move |_request: TriggerPromptRequest, _cancel: CancellationToken| {
            Box::pin(async move {
                TriggerPromptDecision::Deny {
                    reason: Some(reason.to_string()),
                }
            })
        },
    )
}

async fn persist_trust_decision(
    path: &PathBuf,
    session: &Session,
    request: &TriggerPromptRequest,
    decision: TriggerTrustDecision,
) -> Result<(), String> {
    let receiver_agent_id = request
        .receiver_agent_id
        .clone()
        .ok_or_else(|| "hub trust prompt is missing receiver binding".to_string())?;
    let mut store = load_trust_store(path).await?;
    let local_receiver_instance_id = store.local_receiver_instance_id.clone();
    let source_scope = trigger_source_scope(&request.source_label);
    let key = HubTrustKey {
        local_receiver_instance_id: local_receiver_instance_id.clone(),
        source_scope: source_scope.clone(),
        receiver_agent_id: receiver_agent_id.clone(),
        sender_agent_id: request.sender_agent_id.clone(),
        action_class: request.action_class.clone(),
    };
    let now = Utc::now();
    let entry_decision = match decision {
        TriggerTrustDecision::Always => "always",
        TriggerTrustDecision::Block => "block",
    };
    let expires_at = match decision {
        TriggerTrustDecision::Always => Some((now + Duration::days(90)).to_rfc3339()),
        TriggerTrustDecision::Block => None,
    };
    let entry = HubTrustEntry {
        key: key.clone(),
        decision: entry_decision.to_string(),
        scope: HubTrustScope {
            action_class: request.action_class.clone(),
        },
        granted_at: now.to_rfc3339(),
        expires_at,
    };
    if let Some(existing) = store
        .entries
        .iter_mut()
        .find(|existing| existing.key == key)
    {
        *existing = entry;
    } else {
        store.entries.push(entry);
    }
    write_trust_store(path, &store).await?;

    let audit_decision = match decision {
        TriggerTrustDecision::Always => "always",
        TriggerTrustDecision::Block => "block",
    };
    let audit = serde_json::json!({
        "schema_version": 1,
        "trace_id": request.trace_id,
        "receiver_agent_id": receiver_agent_id,
        "local_receiver_instance_id_hash": short_hash(&local_receiver_instance_id),
        "sender_agent_id": request.sender_agent_id,
        "sender_handle": sender_display_from_request(request),
        "decision": audit_decision,
        "scope": { "action_class": request.action_class },
        "trigger_source_scope": source_scope,
        "trigger_source_label": safe_trigger_source_label(&request.source_label),
        "at": now.to_rfc3339(),
    });
    session
        .append_custom("fefe_trust_decision", Some(audit))
        .await
        .map_err(|err| format!("write fefe_trust_decision audit: {:?}", err.code))?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct HubTrustStore {
    version: u8,
    local_receiver_instance_id: String,
    entries: Vec<HubTrustEntry>,
}

impl Default for HubTrustStore {
    fn default() -> Self {
        Self {
            version: 1,
            local_receiver_instance_id: uuid::Uuid::new_v4().to_string(),
            entries: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct HubTrustKey {
    local_receiver_instance_id: String,
    source_scope: String,
    receiver_agent_id: String,
    sender_agent_id: String,
    action_class: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct HubTrustEntry {
    key: HubTrustKey,
    decision: String,
    scope: HubTrustScope,
    granted_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct HubTrustScope {
    action_class: String,
}

async fn load_trust_store(path: &PathBuf) -> Result<HubTrustStore, String> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|_| "hub trust store is not valid JSON".to_string())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HubTrustStore::default()),
        Err(err) => Err(format!("read hub trust store: {err}")),
    }
}

async fn write_trust_store(path: &PathBuf, store: &HubTrustStore) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| format!("create hub trust directory: {err}"))?;
    }
    let body = serde_json::to_string_pretty(store)
        .map_err(|err| format!("encode hub trust store: {err}"))?;
    tokio::fs::write(path, body)
        .await
        .map_err(|err| format!("write hub trust store: {err}"))
}

fn short_hash(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex::encode(&hasher.finalize()[..6])
}

fn trigger_source_scope(source_label: &str) -> String {
    let redacted = crate::bug_report::redact(source_label);
    let mut parts = redacted.split(':');
    match (parts.next(), parts.next()) {
        (Some("mcp"), Some(server)) if is_safe_source_segment(server) => {
            format!("mcp:{server}")
        }
        _ => "<unknown source>".into(),
    }
}

fn safe_trigger_source_label(source_label: &str) -> String {
    let redacted = crate::bug_report::redact(source_label);
    crate::ui::feed::truncate_chars(&redacted.replace('\n', " "), 160)
}

fn is_safe_source_segment(segment: &str) -> bool {
    (1..=64).contains(&segment.len())
        && segment
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
}

fn sender_display_from_request(request: &TriggerPromptRequest) -> String {
    for candidate in [
        request.payload.pointer("/sender/mention"),
        request.payload.pointer("/sender_handle"),
        request.payload.pointer("/sender/handle"),
        request.payload.pointer("/from"),
    ]
    .into_iter()
    .flatten()
    .filter_map(|value| value.as_str())
    {
        let redacted = crate::bug_report::redact(candidate);
        if let Some(mention) = crate::hub_client::parse_mention(&redacted) {
            return mention;
        }
    }
    "@unknown@hub".into()
}

#[cfg(test)]
mod tests {
    use pie_agent_core::{MemorySessionStorage, Session, SessionStorage, TriggerPromptRequest};
    use std::sync::Arc;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn persist_trust_decision_writes_store_and_audit_without_payload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hub-trust.json");
        let storage = Arc::new(MemorySessionStorage::new());
        let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
        let request = TriggerPromptRequest {
            trigger_prompt_id: "prompt_1".into(),
            trace_id: "trace_1".into(),
            source_label: "mcp:pie-hub:custom:agent_message:secret".into(),
            receiver_agent_id: Some("11111111-1111-4111-8111-111111111111".into()),
            sender_agent_id: "22222222-2222-4222-8222-222222222222".into(),
            action_class: "notification".into(),
            trigger_summary: Some("hello".into()),
            payload: serde_json::json!({
                "sender": { "mention": "@alice@dongxu" },
                "payload": { "note": "hub_agent_secret_should_not_persist" }
            }),
            reason: "first contact".into(),
        };

        persist_trust_decision(&path, &session, &request, TriggerTrustDecision::Always)
            .await
            .unwrap();

        let text = tokio::fs::read_to_string(path).await.unwrap();
        assert!(text.contains("\"decision\": \"always\""), "{text}");
        assert!(text.contains("\"source_scope\": \"mcp:pie-hub\""), "{text}");
        assert!(
            !text.contains("hub_agent_secret_should_not_persist"),
            "{text}"
        );
        let entries = session.entries().await.unwrap();
        let audit = entries
            .iter()
            .find(|entry| {
                matches!(
                    entry,
                    pie_agent_core::SessionTreeEntry::Custom { custom_type, .. }
                        if custom_type == "fefe_trust_decision"
                )
            })
            .expect("audit should be written");
        let audit_text = serde_json::to_string(audit).unwrap();
        assert!(
            audit_text.contains("\"decision\":\"always\""),
            "{audit_text}"
        );
        assert!(
            audit_text.contains("\"trigger_source_scope\":\"mcp:pie-hub\""),
            "{audit_text}"
        );
        assert!(
            audit_text
                .contains("\"trigger_source_label\":\"mcp:pie-hub:custom:agent_message:secret\""),
            "{audit_text}"
        );
        assert!(!audit_text.contains("hub_agent_secret_should_not_persist"));
    }

    #[test]
    fn trigger_source_scope_separates_official_and_staging_hubs() {
        assert_eq!(
            trigger_source_scope("mcp:pie-hub:custom:agent_message:one"),
            "mcp:pie-hub"
        );
        assert_eq!(
            trigger_source_scope("mcp:pie-hub-staging:custom:agent_message:one"),
            "mcp:pie-hub-staging"
        );
        assert_eq!(
            trigger_source_scope("mcp:hub_agent_abcdefgh:custom:agent_message:one"),
            "<unknown source>"
        );
    }

    #[tokio::test]
    async fn persist_trust_decision_redacts_source_label_in_audit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hub-trust.json");
        let storage = Arc::new(MemorySessionStorage::new());
        let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
        let request = TriggerPromptRequest {
            trigger_prompt_id: "prompt_2".into(),
            trace_id: "trace_2".into(),
            source_label: "mcp:hub_agent_abcdefgh:custom:agent_message:one".into(),
            receiver_agent_id: Some("11111111-1111-4111-8111-111111111111".into()),
            sender_agent_id: "22222222-2222-4222-8222-222222222222".into(),
            action_class: "notification".into(),
            trigger_summary: Some("hello".into()),
            payload: serde_json::json!({
                "sender": { "mention": "@alice@dongxu" },
                "payload": { "note": "raw Local payload" }
            }),
            reason: "first contact".into(),
        };

        persist_trust_decision(&path, &session, &request, TriggerTrustDecision::Block)
            .await
            .unwrap();

        let entries = session.entries().await.unwrap();
        let audit = entries
            .iter()
            .find(|entry| {
                matches!(
                    entry,
                    pie_agent_core::SessionTreeEntry::Custom { custom_type, .. }
                        if custom_type == "fefe_trust_decision"
                )
            })
            .expect("audit should be written");
        let audit_text = serde_json::to_string(audit).unwrap();
        assert!(
            audit_text.contains("\"trigger_source_scope\":\"<unknown source>\""),
            "{audit_text}"
        );
        assert!(!audit_text.contains("hub_agent_abcdefgh"), "{audit_text}");
        assert!(!audit_text.contains("raw Local payload"), "{audit_text}");
    }
}
