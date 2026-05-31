use std::path::PathBuf;
use std::sync::Arc;

use chrono::{Duration, Utc};
use pie_agent_core::{
    BeforeTriggerContext, BeforeTriggerDecision, BeforeTriggerHook, OnTriggerPromptHook, Session,
    Trigger, TriggerPromptDecision, TriggerPromptRequest, TriggerSource,
};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TriggerPromptDriverDecision {
    AcceptOnce,
    Always,
    Block,
    Skip,
}

impl TriggerPromptDriverDecision {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "accept" => Ok(Self::AcceptOnce),
            "always" => Ok(Self::Always),
            "block" => Ok(Self::Block),
            "skip" => Ok(Self::Skip),
            _ => Err("expected accept, always, block, or skip".into()),
        }
    }

    pub(crate) fn resolution(self) -> UiTriggerPromptResolution {
        match self {
            Self::AcceptOnce => UiTriggerPromptResolution {
                decision: TriggerPromptDecision::Allow,
                trust_decision: None,
            },
            Self::Always => UiTriggerPromptResolution {
                decision: TriggerPromptDecision::Allow,
                trust_decision: Some(TriggerTrustDecision::Always),
            },
            Self::Block => UiTriggerPromptResolution {
                decision: TriggerPromptDecision::Deny {
                    reason: Some("blocked by user".into()),
                },
                trust_decision: Some(TriggerTrustDecision::Block),
            },
            Self::Skip => UiTriggerPromptResolution {
                decision: TriggerPromptDecision::Timeout {
                    reason: Some("deferred_by_user".into()),
                },
                trust_decision: None,
            },
        }
    }
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

pub(crate) fn decision_driver_hook(
    session: Session,
    decision: TriggerPromptDriverDecision,
) -> OnTriggerPromptHook {
    decision_driver_hook_at(
        crate::config::base_dir().join("hub-trust.json"),
        session,
        decision,
    )
}

fn decision_driver_hook_at(
    trust_path: PathBuf,
    session: Session,
    decision: TriggerPromptDriverDecision,
) -> OnTriggerPromptHook {
    Arc::new(
        move |request: TriggerPromptRequest, cancel: CancellationToken| {
            let session = session.clone();
            let trust_path = trust_path.clone();
            Box::pin(async move {
                let resolution = if cancel.is_cancelled() {
                    UiTriggerPromptResolution {
                        decision: TriggerPromptDecision::Timeout {
                            reason: Some("trigger prompt cancelled".into()),
                        },
                        trust_decision: None,
                    }
                } else {
                    decision.resolution()
                };
                if let Some(trust_decision) = resolution.trust_decision
                    && let Err(err) =
                        persist_trust_decision(&trust_path, &session, &request, trust_decision)
                            .await
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
        },
    )
}

pub(crate) fn hub_trust_gate_hook() -> BeforeTriggerHook {
    hub_trust_gate_hook_at(crate::config::base_dir().join("hub-trust.json"))
}

fn hub_trust_gate_hook_at(path: PathBuf) -> BeforeTriggerHook {
    Arc::new(move |ctx: BeforeTriggerContext, _cancel| {
        let path = path.clone();
        Box::pin(async move { hub_trust_gate_decision(&path, &ctx.trigger).await })
    })
}

async fn hub_trust_gate_decision(path: &PathBuf, trigger: &Trigger) -> BeforeTriggerDecision {
    let binding = match HubTriggerBinding::from_trigger(trigger) {
        Ok(Some(binding)) => binding,
        Ok(None) => return BeforeTriggerDecision::Allow,
        Err(reason) => {
            return BeforeTriggerDecision::Deny {
                reason: reason.to_string(),
            };
        }
    };

    let store = match load_trust_store(path).await {
        Ok(store) => store,
        Err(err) => {
            return BeforeTriggerDecision::Deny {
                reason: format!(
                    "could not read hub trust store: {}",
                    crate::bug_report::redact(&err)
                ),
            };
        }
    };
    let key = HubTrustKey {
        local_receiver_instance_id: store.local_receiver_instance_id.clone(),
        source_scope: trigger_source_scope(&trigger.source_label),
        receiver_agent_id: binding.receiver_agent_id,
        sender_agent_id: binding.sender_agent_id,
        action_class: binding.action_class,
    };
    let now = Utc::now();
    if let Some(entry) = store.entries.iter().find(|entry| entry.key == key) {
        match entry.decision.as_str() {
            "block" => {
                return BeforeTriggerDecision::Deny {
                    reason: "hub sender is blocked by local trust policy".into(),
                };
            }
            "always" if !entry_is_expired(entry, now) => {
                return BeforeTriggerDecision::Allow;
            }
            _ => {}
        }
    }

    BeforeTriggerDecision::Prompt {
        reason: "new hub sender requires first-contact approval".into(),
    }
}

#[derive(Debug)]
struct HubTriggerBinding {
    receiver_agent_id: String,
    sender_agent_id: String,
    action_class: String,
}

impl HubTriggerBinding {
    fn from_trigger(trigger: &Trigger) -> Result<Option<Self>, &'static str> {
        if !is_hub_agent_message_trigger(trigger) {
            return Ok(None);
        }
        Ok(Some(Self {
            receiver_agent_id: trigger_payload_uuid(trigger, &["_meta", "receiver_agent_id"])
                .or_else(|| trigger_payload_uuid(trigger, &["receiver_agent_id"]))
                .ok_or("hub notification is missing a valid receiver binding")?,
            sender_agent_id: trigger_payload_uuid(trigger, &["_meta", "sender_agent_id"])
                .or_else(|| trigger_payload_uuid(trigger, &["sender_agent_id"]))
                .ok_or("hub notification is missing a valid sender binding")?,
            action_class: trigger_payload_action_class(trigger, &["_meta", "action_class"])
                .or_else(|| trigger_payload_action_class(trigger, &["action_class"]))
                .ok_or("hub notification is missing a valid action binding")?,
        }))
    }
}

fn is_hub_agent_message_trigger(trigger: &Trigger) -> bool {
    matches!(
        &trigger.source,
        TriggerSource::Mcp { method, .. } if method == "notifications/agent_message"
    )
}

fn trigger_payload_uuid(trigger: &Trigger, path: &[&str]) -> Option<String> {
    uuid_string(trigger_payload_string(trigger, path)?)
}

fn uuid_string(value: &str) -> Option<String> {
    uuid::Uuid::parse_str(value).ok()?;
    Some(value.to_string())
}

fn trigger_payload_action_class(trigger: &Trigger, path: &[&str]) -> Option<String> {
    let value = trigger_payload_string(trigger, path)?;
    (value == "notification").then_some(value.to_string())
}

fn trigger_payload_string<'a>(trigger: &'a Trigger, path: &[&str]) -> Option<&'a str> {
    let mut value = trigger.payload.as_ref()?;
    for key in path {
        value = value.get(*key)?;
    }
    value.as_str()
}

fn entry_is_expired(entry: &HubTrustEntry, now: chrono::DateTime<Utc>) -> bool {
    let Some(expires_at) = entry.expires_at.as_deref() else {
        return false;
    };
    chrono::DateTime::parse_from_rfc3339(expires_at)
        .map(|expires_at| expires_at.with_timezone(&Utc) <= now)
        .unwrap_or(true)
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
        if let Some(mention) = crate::hub_client::display_mention(&redacted) {
            return mention;
        }
    }
    "unknown@hub".into()
}

#[cfg(test)]
mod tests {
    use pie_agent_core::{
        AgentHarness, AgentHarnessOptions, BeforeTriggerContext, BeforeTriggerDecision,
        BeforeTriggerHook, CredentialScope, MemorySessionStorage, PayloadVisibility, PromoteAction,
        ReplacementPolicy, Session, SessionStorage, SourceKind, Trigger, TriggerAction,
        TriggerAuthority, TriggerDelivery, TriggerPromptRequest, TriggerSource,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
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

    #[tokio::test]
    async fn decision_driver_hook_maps_block_and_skip_without_exposing_payload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hub-trust.json");
        let storage = Arc::new(MemorySessionStorage::new());
        let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
        let request = TriggerPromptRequest {
            trigger_prompt_id: "prompt_driver".into(),
            trace_id: "trace_driver".into(),
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

        let hook = decision_driver_hook_at(
            path.clone(),
            session.clone(),
            TriggerPromptDriverDecision::Block,
        );
        let decision = hook(request.clone(), CancellationToken::new()).await;
        assert!(matches!(
            decision,
            TriggerPromptDecision::Deny { reason }
                if reason.as_deref() == Some("blocked by user")
        ));

        let text = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(text.contains("\"decision\": \"block\""), "{text}");
        assert!(
            !text.contains("hub_agent_secret_should_not_persist"),
            "{text}"
        );
        let audit_text = serde_json::to_string(&session.entries().await.unwrap()).unwrap();
        assert!(
            audit_text.contains("\"decision\":\"block\""),
            "{audit_text}"
        );
        assert!(
            !audit_text.contains("hub_agent_secret_should_not_persist"),
            "{audit_text}"
        );

        let hook = decision_driver_hook_at(path, session, TriggerPromptDriverDecision::Skip);
        let decision = hook(request, CancellationToken::new()).await;
        assert!(matches!(
            decision,
            TriggerPromptDecision::Timeout { reason }
                if reason.as_deref() == Some("deferred_by_user")
        ));
    }

    #[tokio::test]
    async fn decision_driver_block_stops_before_trigger_action() {
        let dir = tempdir().unwrap();
        let storage = Arc::new(MemorySessionStorage::new());
        let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
        let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
        let prompt_hook: BeforeTriggerHook = Arc::new(|_ctx: BeforeTriggerContext, _cancel| {
            Box::pin(async move {
                BeforeTriggerDecision::Prompt {
                    reason: "new hub sender requires first-contact approval".into(),
                }
            })
        });
        opts.before_trigger = Some(prompt_hook);
        opts.on_trigger_prompt = Some(decision_driver_hook_at(
            dir.path().join("hub-trust.json"),
            session.clone(),
            TriggerPromptDriverDecision::Block,
        ));
        let action_calls = Arc::new(AtomicUsize::new(0));
        let action_calls_sink = action_calls.clone();
        opts.before_trigger_action = Some(Arc::new(move |_ctx, _cancel| {
            action_calls_sink.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                TriggerAction {
                    prompt: "should not run".into(),
                    promote: PromoteAction::None,
                    promote_requires_approval: false,
                    delivery: TriggerDelivery::InjectSummary,
                }
            })
        }));

        let harness = AgentHarness::new(opts);
        let outcome = harness.handle_trigger(hub_message_trigger()).await;
        assert!(matches!(outcome, pie_agent_core::EvaluationOutcome::Accept));
        assert_eq!(
            action_calls.load(Ordering::SeqCst),
            0,
            "blocked first-contact driver must not reach trigger execution"
        );

        let entries = session.entries().await.unwrap();
        let trigger_record = entries
            .iter()
            .find_map(|entry| match entry {
                pie_agent_core::SessionTreeEntry::Custom {
                    custom_type, data, ..
                } if custom_type == pie_agent_core::TriggerRecord::CUSTOM_TYPE => data.clone(),
                _ => None,
            })
            .expect("trigger audit should be written");
        assert_eq!(trigger_record["state"].as_str(), Some("needs_approval"));
        assert_eq!(
            trigger_record["evaluator_decision"]["prompt_decision"].as_str(),
            Some("deny")
        );
    }

    #[tokio::test]
    async fn hub_trust_gate_prompts_before_untrusted_hub_notification() {
        let dir = tempdir().unwrap();
        let trigger = hub_message_trigger();

        let decision = hub_trust_gate_decision(&dir.path().join("hub-trust.json"), &trigger).await;

        assert!(matches!(
            decision,
            BeforeTriggerDecision::Prompt { reason }
                if reason == "new hub sender requires first-contact approval"
        ));
    }

    #[tokio::test]
    async fn hub_trust_gate_denies_malformed_hub_notification_binding() {
        let dir = tempdir().unwrap();
        let mut trigger = hub_message_trigger();
        trigger.payload = Some(serde_json::json!({
            "_meta": {
                "receiver_agent_id": "hub_agent_not_a_uuid",
                "sender_agent_id": "22222222-2222-4222-8222-222222222222",
                "action_class": "notification",
            },
            "payload": { "note": "raw Local payload must not matter" },
        }));

        let decision = hub_trust_gate_decision(&dir.path().join("hub-trust.json"), &trigger).await;

        assert!(matches!(
            decision,
            BeforeTriggerDecision::Deny { reason }
                if reason == "hub notification is missing a valid receiver binding"
        ));
    }

    #[tokio::test]
    async fn hub_trust_gate_allows_or_denies_existing_trust_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hub-trust.json");
        let trigger = hub_message_trigger();
        let key = HubTrustKey {
            local_receiver_instance_id: "local-instance".into(),
            source_scope: "mcp:pie-hub".into(),
            receiver_agent_id: "11111111-1111-4111-8111-111111111111".into(),
            sender_agent_id: "22222222-2222-4222-8222-222222222222".into(),
            action_class: "notification".into(),
        };
        write_trust_store(
            &path,
            &HubTrustStore {
                version: 1,
                local_receiver_instance_id: "local-instance".into(),
                entries: vec![HubTrustEntry {
                    key: key.clone(),
                    decision: "always".into(),
                    scope: HubTrustScope {
                        action_class: "notification".into(),
                    },
                    granted_at: Utc::now().to_rfc3339(),
                    expires_at: Some((Utc::now() + Duration::days(1)).to_rfc3339()),
                }],
            },
        )
        .await
        .unwrap();

        let decision = hub_trust_gate_decision(&path, &trigger).await;
        assert!(matches!(decision, BeforeTriggerDecision::Allow));

        write_trust_store(
            &path,
            &HubTrustStore {
                version: 1,
                local_receiver_instance_id: "local-instance".into(),
                entries: vec![HubTrustEntry {
                    key,
                    decision: "block".into(),
                    scope: HubTrustScope {
                        action_class: "notification".into(),
                    },
                    granted_at: Utc::now().to_rfc3339(),
                    expires_at: None,
                }],
            },
        )
        .await
        .unwrap();

        let decision = hub_trust_gate_decision(&path, &trigger).await;
        assert!(matches!(
            decision,
            BeforeTriggerDecision::Deny { reason }
                if reason == "hub sender is blocked by local trust policy"
        ));
    }

    fn hub_message_trigger() -> Trigger {
        Trigger {
            source: TriggerSource::Mcp {
                server_name: "pie-hub".into(),
                method: "notifications/agent_message".into(),
            },
            source_kind: SourceKind::Mcp,
            source_label: "mcp:pie-hub".into(),
            event_label: "notifications/agent_message".into(),
            payload_visibility: PayloadVisibility::Local,
            payload_summary: Some("hello".into()),
            payload: Some(serde_json::json!({
                "_meta": {
                    "receiver_agent_id": "11111111-1111-4111-8111-111111111111",
                    "sender_agent_id": "22222222-2222-4222-8222-222222222222",
                    "action_class": "notification",
                },
                "sender": { "mention": "@alice@dongxu" },
                "payload": { "note": "hub_agent_secret_should_not_be_used" },
            })),
            idempotency_key: "mcp:pie-hub:custom:notification-1".into(),
            replacement_policy: ReplacementPolicy::Drop,
            trace_id: "trace-hub".into(),
            authority: TriggerAuthority {
                principal_id: "mcp:pie-hub".into(),
                principal_label: "pie-hub".into(),
                credential_scope: CredentialScope::User,
                allowed_source_actions: Vec::new(),
                expires_at: None,
            },
            received_at: Utc::now(),
        }
    }

    fn faux_model() -> pie_ai::Model {
        pie_ai::Model {
            id: "faux".into(),
            name: "Faux".into(),
            api: pie_ai::Api::from("faux"),
            provider: pie_ai::Provider::from("faux"),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: pie_ai::ModelCost::default(),
            context_window: 0,
            max_tokens: 0,
            headers: None,
            compat: None,
        }
    }
}
