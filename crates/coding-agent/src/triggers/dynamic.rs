//! Dynamic trigger rules created at runtime from natural-language user requests.
//!
//! This is intentionally source-agnostic: a rule stores the user's condition as text and
//! lets the trigger action agent evaluate that condition against whatever event envelope
//! arrived. Concrete sources (MCP, future GitHub/webhook/local watchers) only need to emit
//! normal runtime `Trigger`s.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Local, Utc};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, BeforeTriggerActionContext,
    BeforeTriggerActionHook, CredentialScope, HarnessEvent, HarnessListener, HookError, HookState,
    NotificationHook, NotificationHookStatus, PayloadVisibility, PromoteAction, ReplacementPolicy,
    SourceKind, ToolExecutionMode, Trigger, TriggerAction, TriggerAuthority, TriggerSink,
    TriggerSource,
};
use pie_ai::{Tool, UserContentBlock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{Duration, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const ZH_WHEN_PREFIX: &str = "\u{5f53}";
const ZH_IF_PREFIX: &str = "\u{5982}\u{679c}";
const ZH_TIME_SUFFIX_LONG: &str = "\u{7684}\u{65f6}\u{5019}";
const ZH_TIME_SUFFIX_SHORT: char = '\u{65f6}';
const ZH_EXECUTE_PREFIX: &str = "\u{6267}\u{884c}";
pub const DEFAULT_DYNAMIC_TRIGGER_POLL_INTERVAL_SECS: u64 = 60;
static CONFIGURED_DYNAMIC_TRIGGER_POLL_INTERVAL_SECS: AtomicU64 =
    AtomicU64::new(DEFAULT_DYNAMIC_TRIGGER_POLL_INTERVAL_SECS);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DynamicTriggerRule {
    pub id: String,
    pub condition: String,
    pub action: String,
    pub enabled: bool,
    #[serde(default = "default_fire_once")]
    pub fire_once: bool,
    #[serde(default)]
    pub fired_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub promote_to_chat: bool,
    pub created_at: DateTime<Utc>,
}

fn default_fire_once() -> bool {
    true
}

#[derive(Clone, Debug, Default)]
pub struct DynamicTriggerRegistry {
    inner: Arc<Mutex<DynamicTriggerRegistryState>>,
}

#[derive(Clone, Debug, Default)]
struct DynamicTriggerRegistryState {
    rules: Vec<DynamicTriggerRule>,
    storage_path: Option<PathBuf>,
}

impl DynamicTriggerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_from_path(
        &self,
        path: impl Into<PathBuf>,
    ) -> Result<(), DynamicTriggerStorageError> {
        let path = path.into();
        let rules = read_rules_file(&path)?;
        let mut state = self.inner.lock();
        state.rules = rules;
        state.storage_path = Some(path);
        Ok(())
    }

    pub fn storage_path(&self) -> Option<PathBuf> {
        self.inner.lock().storage_path.clone()
    }

    pub fn add_rule(
        &self,
        condition: &str,
        action: &str,
    ) -> Result<DynamicTriggerRule, AddTriggerRuleError> {
        self.add_rule_with_options(condition, action, true)
    }

    pub fn add_rule_with_options(
        &self,
        condition: &str,
        action: &str,
        fire_once: bool,
    ) -> Result<DynamicTriggerRule, AddTriggerRuleError> {
        self.add_rule_with_flags(condition, action, fire_once, false)
    }

    pub fn add_rule_with_flags(
        &self,
        condition: &str,
        action: &str,
        fire_once: bool,
        promote_to_chat: bool,
    ) -> Result<DynamicTriggerRule, AddTriggerRuleError> {
        let condition = condition.trim();
        let action = action.trim();
        if condition.is_empty() || action.is_empty() {
            return Err(ParseTriggerRuleError::EmptyPart.into());
        }
        let rule = DynamicTriggerRule {
            id: format!("dyn-{}", Uuid::new_v4().simple()),
            condition: condition.to_string(),
            action: action.to_string(),
            enabled: true,
            fire_once,
            fired_at: None,
            promote_to_chat,
            created_at: Utc::now(),
        };
        self.insert_rule(rule)
    }

    pub fn add_from_spec(&self, spec: &str) -> Result<DynamicTriggerRule, AddTriggerRuleError> {
        let parsed = parse_trigger_rule(spec)?;
        self.add_rule(&parsed.condition, &parsed.action)
    }

    fn insert_rule(
        &self,
        rule: DynamicTriggerRule,
    ) -> Result<DynamicTriggerRule, AddTriggerRuleError> {
        let mut state = self.inner.lock();
        let mut next = state.rules.clone();
        next.push(rule.clone());
        if let Some(path) = &state.storage_path {
            write_rules_file(path, &next)?;
        }
        state.rules = next;
        Ok(rule)
    }

    pub fn list(&self) -> Vec<DynamicTriggerRule> {
        self.inner.lock().rules.clone()
    }

    pub fn remove_rule(
        &self,
        id: &str,
    ) -> Result<Option<DynamicTriggerRule>, DynamicTriggerStorageError> {
        let id = id.trim();
        let mut state = self.inner.lock();
        let Some(pos) = state.rules.iter().position(|rule| rule.id == id) else {
            return Ok(None);
        };
        let mut next = state.rules.clone();
        let removed = next.remove(pos);
        if let Some(path) = &state.storage_path {
            write_rules_file(path, &next)?;
        }
        state.rules = next;
        Ok(Some(removed))
    }

    pub fn set_rule_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<Option<DynamicTriggerRule>, DynamicTriggerStorageError> {
        let id = id.trim();
        let mut state = self.inner.lock();
        let Some(pos) = state.rules.iter().position(|rule| rule.id == id) else {
            return Ok(None);
        };
        let mut next = state.rules.clone();
        next[pos].enabled = enabled;
        if enabled {
            next[pos].fired_at = None;
        }
        let updated = next[pos].clone();
        if let Some(path) = &state.storage_path {
            write_rules_file(path, &next)?;
        }
        state.rules = next;
        Ok(Some(updated))
    }

    pub fn clear_rules(&self) -> Result<usize, DynamicTriggerStorageError> {
        let mut state = self.inner.lock();
        let count = state.rules.len();
        if count == 0 {
            return Ok(0);
        }
        if let Some(path) = &state.storage_path {
            write_rules_file(path, &[])?;
        }
        state.rules.clear();
        Ok(count)
    }

    pub fn mark_rules_fired(
        &self,
        ids: &[String],
    ) -> Result<Vec<DynamicTriggerRule>, DynamicTriggerStorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut state = self.inner.lock();
        let now = Utc::now();
        let mut next = state.rules.clone();
        let mut changed = Vec::new();
        for rule in &mut next {
            if !rule.fire_once || !rule.enabled || !ids.iter().any(|id| id == &rule.id) {
                continue;
            }
            rule.enabled = false;
            rule.fired_at = Some(now);
            changed.push(rule.clone());
        }
        if changed.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(path) = &state.storage_path {
            write_rules_file(path, &next)?;
        }
        state.rules = next;
        Ok(changed)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn clear_for_tests(&self) {
        *self.inner.lock() = DynamicTriggerRegistryState::default();
    }
}

pub fn global_registry() -> &'static DynamicTriggerRegistry {
    static CELL: OnceCell<DynamicTriggerRegistry> = OnceCell::new();
    CELL.get_or_init(DynamicTriggerRegistry::new)
}

pub fn set_dynamic_trigger_poll_interval_secs(secs: u64) {
    CONFIGURED_DYNAMIC_TRIGGER_POLL_INTERVAL_SECS.store(secs.max(1), Ordering::Relaxed);
}

pub fn dynamic_trigger_poll_interval_secs() -> u64 {
    CONFIGURED_DYNAMIC_TRIGGER_POLL_INTERVAL_SECS.load(Ordering::Relaxed)
}

pub struct DynamicTriggerCheckHook {
    registry: DynamicTriggerRegistry,
    interval: Duration,
    status: Arc<Mutex<NotificationHookStatus>>,
}

impl DynamicTriggerCheckHook {
    pub fn new(registry: DynamicTriggerRegistry) -> Self {
        Self::with_interval(
            registry,
            Duration::from_secs(dynamic_trigger_poll_interval_secs()),
        )
    }

    pub fn with_interval(registry: DynamicTriggerRegistry, interval: Duration) -> Self {
        let mut status = NotificationHookStatus::pending();
        status.subscription_labels = vec!["dynamic trigger periodic check".into()];
        Self {
            registry,
            interval,
            status: Arc::new(Mutex::new(status)),
        }
    }

    fn build_trigger(&self, rule_count: usize) -> Trigger {
        let now_utc = Utc::now();
        let now_local = Local::now();
        let current_dir = std::env::current_dir()
            .ok()
            .map(|path| path.display().to_string());
        // RFC 0 §3.2.2 / RFC 1 §4.2.3: when `payload_visibility = Local`, consumers see
        // only `payload_summary`. Folding the context fields into the summary instead of
        // putting them in `payload` keeps the envelope internally consistent — the
        // sub-agent prompt renderer drops `payload` for Local sources, so anything in
        // `payload` here would never be visible to the evaluator anyway. The dynamic
        // check's needs (cwd + clock + rule count) all fit in the summary string.
        let summary = format!(
            "Periodic dynamic trigger check at local time {} / UTC {} with {} enabled rule(s); cwd: {}",
            now_local.format("%Y-%m-%d %H:%M:%S %Z"),
            now_utc.to_rfc3339(),
            rule_count,
            current_dir.as_deref().unwrap_or("<unknown>"),
        );
        Trigger {
            source: TriggerSource::Local {
                subkind: "dynamic".into(),
            },
            source_kind: SourceKind::Local,
            source_label: "local:dynamic".into(),
            event_label: "dynamic periodic check".into(),
            payload_visibility: PayloadVisibility::Local,
            payload_summary: Some(summary),
            payload: None,
            idempotency_key: format!("local:dynamic:{}", now_utc.timestamp_millis()),
            replacement_policy: ReplacementPolicy::Drop,
            trace_id: Uuid::new_v4().to_string(),
            authority: TriggerAuthority {
                principal_id: "local:dynamic".into(),
                principal_label: "dynamic trigger checker".into(),
                credential_scope: CredentialScope::User,
                allowed_source_actions: Vec::new(),
                expires_at: None,
            },
            received_at: now_utc,
        }
    }
}

#[async_trait]
impl NotificationHook for DynamicTriggerCheckHook {
    fn label(&self) -> &str {
        "local:dynamic"
    }

    async fn run(&self, sink: TriggerSink) -> Result<(), HookError> {
        self.status.lock().state = HookState::Connected;

        let mut interval = tokio::time::interval(self.interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            let enabled_count = self
                .registry
                .list()
                .into_iter()
                .filter(|rule| rule.enabled)
                .count();
            if enabled_count == 0 {
                continue;
            }

            let trigger = self.build_trigger(enabled_count);
            if sink.send(trigger).is_err() {
                self.status.lock().state = HookState::Disconnected {
                    reason: "sink closed".into(),
                };
                return Err(HookError::SinkClosed);
            }
            let mut status = self.status.lock();
            status.last_event_at = Some(Utc::now());
            status.last_error = None;
        }
    }

    fn status(&self) -> NotificationHookStatus {
        self.status.lock().clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedTriggerRule {
    pub condition: String,
    pub action: String,
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseTriggerRuleError {
    #[error("usage: /new-trigger <when condition, run action>")]
    Empty,
    #[error(
        "could not split the trigger into a condition and action. In normal chat, ask pie to create the trigger so the model can extract them, or use `/new-trigger if condition, then action`."
    )]
    MissingAction,
    #[error("condition and action must both be non-empty")]
    EmptyPart,
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum AddTriggerRuleError {
    #[error(transparent)]
    Parse(#[from] ParseTriggerRuleError),
    #[error(transparent)]
    Storage(#[from] DynamicTriggerStorageError),
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum DynamicTriggerStorageError {
    #[error("read dynamic triggers: {0}")]
    Read(String),
    #[error("parse dynamic triggers: {0}")]
    Parse(String),
    #[error("write dynamic triggers: {0}")]
    Write(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DynamicTriggerFile {
    version: u32,
    rules: Vec<DynamicTriggerRule>,
}

const DYNAMIC_TRIGGER_FILE_VERSION: u32 = 1;

fn read_rules_file(path: &Path) -> Result<Vec<DynamicTriggerRule>, DynamicTriggerStorageError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(DynamicTriggerStorageError::Read(e.to_string())),
    };
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let file: DynamicTriggerFile = serde_json::from_str(&text)
        .map_err(|e| DynamicTriggerStorageError::Parse(e.to_string()))?;
    Ok(file.rules)
}

fn write_rules_file(
    path: &Path,
    rules: &[DynamicTriggerRule],
) -> Result<(), DynamicTriggerStorageError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| DynamicTriggerStorageError::Write(e.to_string()))?;
    }
    let file = DynamicTriggerFile {
        version: DYNAMIC_TRIGGER_FILE_VERSION,
        rules: rules.to_vec(),
    };
    let text = serde_json::to_string_pretty(&file)
        .map_err(|e| DynamicTriggerStorageError::Write(e.to_string()))?;
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("dynamic-triggers.json");
    let tmp = path.with_file_name(format!("{file_name}.tmp-{}", Uuid::new_v4().simple()));
    std::fs::write(&tmp, text).map_err(|e| DynamicTriggerStorageError::Write(e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| DynamicTriggerStorageError::Write(e.to_string()))?;
    Ok(())
}

pub fn parse_trigger_rule(spec: &str) -> Result<ParsedTriggerRule, ParseTriggerRuleError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(ParseTriggerRuleError::Empty);
    }

    let markers: &[&str] = &[
        "\u{7684}\u{65f6}\u{5019}\u{ff0c}\u{6267}\u{884c}",
        "\u{7684}\u{65f6}\u{5019},\u{6267}\u{884c}",
        "\u{7684}\u{65f6}\u{5019} \u{6267}\u{884c}",
        "\u{7684}\u{65f6}\u{5019}\u{6267}\u{884c}",
        "\u{7684}\u{65f6}\u{5019}\u{ff0c}",
        "\u{7684}\u{65f6}\u{5019},",
        "\u{65f6}\u{ff0c}\u{6267}\u{884c}",
        "\u{65f6},\u{6267}\u{884c}",
        "\u{65f6} \u{6267}\u{884c}",
        "\u{65f6}\u{6267}\u{884c}",
        "\u{65f6}\u{ff0c}",
        "\u{65f6},",
        "\u{ff0c}\u{5219}",
        ", \u{5219}",
        ",\u{5219}",
        " \u{5219} ",
        "\u{5219}",
        "\u{ff0c}\u{5c31}",
        ", \u{5c31}",
        ",\u{5c31}",
        " \u{5c31} ",
        "\u{ff0c}\u{6267}\u{884c}",
        ", \u{6267}\u{884c}",
        ",\u{6267}\u{884c}",
        " \u{6267}\u{884c} ",
        " then ",
        " then run ",
        " then execute ",
        ", run ",
        ", execute ",
        ", do ",
        " run ",
        " execute ",
    ];

    let lower = spec.to_lowercase();
    let mut split: Option<(usize, &str)> = None;
    for &marker in markers {
        let haystack = if marker.is_ascii() {
            lower.as_str()
        } else {
            spec
        };
        if let Some(idx) = haystack.find(marker) {
            split = Some((idx, marker));
            break;
        }
    }

    let Some((idx, marker)) = split else {
        return Err(ParseTriggerRuleError::MissingAction);
    };

    let raw_condition = spec[..idx].trim();
    let raw_action = spec[idx + marker.len()..].trim();
    let condition = clean_condition(raw_condition);
    let action = clean_action(raw_action);
    if condition.is_empty() || action.is_empty() {
        return Err(ParseTriggerRuleError::EmptyPart);
    }

    Ok(ParsedTriggerRule { condition, action })
}

fn clean_condition(raw: &str) -> String {
    let mut s = raw.trim();
    if let Some(rest) = s.strip_prefix(ZH_WHEN_PREFIX) {
        s = rest.trim();
    }
    if let Some(rest) = s.strip_prefix(ZH_IF_PREFIX) {
        s = rest.trim();
    }
    let lower = s.to_lowercase();
    if lower.starts_with("when ") {
        s = s[5..].trim();
    } else if lower.starts_with("if ") {
        s = s[3..].trim();
    }
    s.trim_end_matches(ZH_TIME_SUFFIX_LONG)
        .trim_end_matches(ZH_TIME_SUFFIX_SHORT)
        .trim()
        .to_string()
}

fn clean_action(raw: &str) -> String {
    let mut s = raw.trim();
    if let Some(rest) = s.strip_prefix(ZH_EXECUTE_PREFIX) {
        s = rest.trim();
    }
    let lower = s.to_lowercase();
    if lower.starts_with("run ") {
        s = s[4..].trim();
    } else if lower.starts_with("execute ") {
        s = s[8..].trim();
    }
    s.to_string()
}

pub fn before_trigger_action_hook(registry: DynamicTriggerRegistry) -> BeforeTriggerActionHook {
    Arc::new(
        move |ctx: BeforeTriggerActionContext, _cancel: CancellationToken| {
            let registry = registry.clone();
            Box::pin(async move {
                let rules = registry.list();
                let enabled: Vec<_> = rules.into_iter().filter(|r| r.enabled).collect();
                if enabled.is_empty() {
                    return TriggerAction::default_for(&ctx.trigger);
                }
                let promote_rule_ids: Vec<String> = enabled
                    .iter()
                    .filter(|rule| rule.promote_to_chat)
                    .map(|rule| rule.id.clone())
                    .collect();

                TriggerAction {
                    prompt: render_dynamic_trigger_prompt(&ctx.trigger, &enabled),
                    promote: if promote_rule_ids.is_empty() {
                        PromoteAction::None
                    } else {
                        // Transitional: still uses the deprecated summary-substring path.
                        // Tools-MCP's follow-up PR migrates this to
                        // `PromoteAction::PromoteSummaryWhenResultDetailsMatch` with
                        // `PromotionCondition::AnyOf` once the `mark_dynamic_rule_matched`
                        // tool is wired into the sub-agent. Allowed locally until then.
                        #[allow(deprecated)]
                        PromoteAction::PromoteSummaryWhenSummaryContains {
                            template_body: None,
                            required_substrings: promote_rule_ids,
                        }
                    },
                    promote_requires_approval: false,
                }
            })
        },
    )
}

pub fn fire_once_harness_listener(registry: DynamicTriggerRegistry) -> HarnessListener {
    Arc::new(move |event| {
        let HarnessEvent::TriggerCompleted {
            summary: Some(summary),
            ..
        } = event
        else {
            return;
        };
        let ids = extract_dynamic_rule_ids(&summary);
        let _ = registry.mark_rules_fired(&ids);
    })
}

fn render_dynamic_trigger_prompt(trigger: &Trigger, rules: &[DynamicTriggerRule]) -> String {
    let rules_json = serde_json::to_string_pretty(rules).unwrap_or_else(|_| "[]".to_string());
    // RFC 0 §3.2.2 / RFC 1 §4.2.3 privacy contract: the full `payload` only reaches a
    // consumer when `payload_visibility = Shared`. For `Local` (default) and `Redacted`
    // sources we surface only the safe summary; the raw `payload` is null in the prompt
    // even if the adapter populated it. This prevents future hub / file-watcher / local
    // sources that legitimately attach context to `payload` from leaking that context
    // into the sub-agent (and therefore the model provider). The unconditional
    // serialization that existed before bypassed the contract.
    let payload_for_prompt = match trigger.payload_visibility {
        PayloadVisibility::Shared => trigger.payload.clone(),
        PayloadVisibility::Local | PayloadVisibility::Redacted => None,
    };
    let trigger_json = serde_json::json!({
        "source_kind": trigger.source_kind,
        "source": trigger.source.clone(),
        "source_label": trigger.source_label.clone(),
        "event_label": trigger.event_label.clone(),
        "payload_visibility": trigger.payload_visibility,
        "payload_summary": trigger.payload_summary.clone(),
        "payload": payload_for_prompt,
        "received_at": trigger.received_at,
        "idempotency_key": trigger.idempotency_key.clone(),
        "trace_id": trigger.trace_id.clone(),
        "authority": {
            "principal_id": trigger.authority.principal_id.clone(),
            "principal_label": trigger.authority.principal_label.clone(),
            "credential_scope": trigger.authority.credential_scope,
        }
    });
    let trigger_json =
        serde_json::to_string_pretty(&trigger_json).unwrap_or_else(|_| "{}".to_string());
    format!(
        "A trigger check event arrived.\n\nEvent:\n{trigger_json}\n\nDynamic trigger rules:\n{rules_json}\n\nEvaluate each rule's natural-language condition. For source-specific events, compare the rule against the event. For `local:dynamic` periodic checks, inspect current local or remote state with the available tools whenever the condition depends on filesystem state, paths, environment variables, shell expansion, command output, clock time, network/API state, or any fact not already present in the Event JSON. Do not report no match for those conditions until after the needed inspection. If no enabled rule matches after any required inspection, reply with exactly: no dynamic trigger rule matched.\n\nIf one or more rules match, execute each matching rule's action. Treat the action as an instruction from the user. If it asks to read or print a file, use the read tool or a safe shell command, then include the requested file contents in your final response. If it asks to run a local program or shell command, use the bash tool. Keep the final response concise and include the exact matched rule id(s), for example `matched dyn-...`."
    )
}

fn extract_dynamic_rule_ids(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] != b"dyn-" {
            i += 1;
            continue;
        }

        let start = i;
        i += 4;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        if i - start == 36 {
            let id = text[start..i].to_string();
            if !ids.iter().any(|existing| existing == &id) {
                ids.push(id);
            }
        }
    }
    ids
}

pub struct NewTriggerTool;

pub struct ListTriggersTool;

pub struct RemoveTriggerTool;

pub struct SetTriggerStateTool;

#[async_trait]
impl AgentTool for NewTriggerTool {
    fn definition(&self) -> &Tool {
        &NEW_TRIGGER_TOOL
    }

    fn label(&self) -> &str {
        "NewTrigger"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Parallel)
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let condition = params.get("condition").and_then(|v| v.as_str());
        let action = params.get("action").and_then(|v| v.as_str());
        let fire_once = params
            .get("fire_once")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let promote_to_chat = params
            .get("promote_to_chat")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let rule = match (condition, action) {
            (Some(condition), Some(action)) => {
                global_registry().add_rule_with_flags(condition, action, fire_once, promote_to_chat)
            }
            _ => {
                let spec = params.get("spec").and_then(|v| v.as_str()).ok_or_else(|| {
                    AgentToolError::from("missing required args: provide condition and action")
                })?;
                global_registry().add_from_spec(spec)
            }
        }
        .map_err(|e| AgentToolError::Message(e.to_string()))?;
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "created dynamic trigger {}\ncondition: {}\naction: {}\nfire_once: {}\npromote_to_chat: {}",
                rule.id, rule.condition, rule.action, rule.fire_once, rule.promote_to_chat
            ))],
            details: json!({
                "id": rule.id,
                "condition": rule.condition,
                "action": rule.action,
                "enabled": rule.enabled,
                "fire_once": rule.fire_once,
                "fired_at": rule.fired_at,
                "promote_to_chat": rule.promote_to_chat,
            }),
            terminate: None,
        })
    }
}

#[async_trait]
impl AgentTool for ListTriggersTool {
    fn definition(&self) -> &Tool {
        &LIST_TRIGGERS_TOOL
    }

    fn label(&self) -> &str {
        "ListTriggers"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Parallel)
    }

    async fn execute(
        &self,
        _id: &str,
        _params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let rules = global_registry().list();
        let storage_path = global_registry()
            .storage_path()
            .map(|path| path.display().to_string());
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(render_trigger_rules_for_tool(
                &rules,
            ))],
            details: json!({
                "count": rules.len(),
                "rules": rules,
                "storage_path": storage_path,
            }),
            terminate: None,
        })
    }
}

#[async_trait]
impl AgentTool for RemoveTriggerTool {
    fn definition(&self) -> &Tool {
        &REMOVE_TRIGGER_TOOL
    }

    fn label(&self) -> &str {
        "RemoveTrigger"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Parallel)
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        if params.get("all").and_then(|v| v.as_bool()) == Some(true) {
            let count = global_registry()
                .clear_rules()
                .map_err(|e| AgentToolError::Message(e.to_string()))?;
            return Ok(AgentToolResult {
                content: vec![UserContentBlock::text(format!(
                    "removed {count} dynamic trigger rule(s)"
                ))],
                details: json!({ "removed_count": count, "all": true }),
                terminate: None,
            });
        }

        let id = params
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing required arg: id"))?;
        let removed = global_registry()
            .remove_rule(id)
            .map_err(|e| AgentToolError::Message(e.to_string()))?;
        let Some(rule) = removed else {
            return Err(AgentToolError::Message(format!(
                "no dynamic trigger rule with id '{id}'"
            )));
        };
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "removed dynamic trigger {}\ncondition: {}\naction: {}",
                rule.id, rule.condition, rule.action
            ))],
            details: json!({
                "id": rule.id,
                "condition": rule.condition,
                "action": rule.action,
                "removed_count": 1,
            }),
            terminate: None,
        })
    }
}

#[async_trait]
impl AgentTool for SetTriggerStateTool {
    fn definition(&self) -> &Tool {
        &SET_TRIGGER_STATE_TOOL
    }

    fn label(&self) -> &str {
        "SetTriggerState"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Parallel)
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let id = params
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing required arg: id"))?;
        let enabled = params
            .get("enabled")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| AgentToolError::from("missing required arg: enabled"))?;
        let updated = global_registry()
            .set_rule_enabled(id, enabled)
            .map_err(|e| AgentToolError::Message(e.to_string()))?;
        let Some(rule) = updated else {
            return Err(AgentToolError::Message(format!(
                "no dynamic trigger rule with id '{id}'"
            )));
        };
        let state = if rule.enabled { "enabled" } else { "disabled" };
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "updated dynamic trigger {}\nstate: {}\ncondition: {}\naction: {}",
                rule.id, state, rule.condition, rule.action
            ))],
            details: json!({
                "id": rule.id,
                "condition": rule.condition,
                "action": rule.action,
                "enabled": rule.enabled,
                "fire_once": rule.fire_once,
                "fired_at": rule.fired_at,
                "promote_to_chat": rule.promote_to_chat,
            }),
            terminate: None,
        })
    }
}

fn render_trigger_rules_for_tool(rules: &[DynamicTriggerRule]) -> String {
    if rules.is_empty() {
        return "dynamic trigger rules: none".into();
    }
    let mut lines = vec![format!("dynamic trigger rules: {}", rules.len())];
    for rule in rules {
        let state = if rule.enabled { "enabled" } else { "disabled" };
        let fire_mode = if rule.fire_once {
            "fire_once"
        } else {
            "repeat"
        };
        let output_mode = if rule.promote_to_chat {
            "promote_to_chat"
        } else {
            "audit_only"
        };
        lines.push(format!(
            "- {} [{state}, {fire_mode}, {output_mode}] created_at={} condition: {} action: {}",
            rule.id,
            rule.created_at.to_rfc3339(),
            rule.condition,
            rule.action
        ));
    }
    lines.join("\n")
}

static NEW_TRIGGER_TOOL: once_cell::sync::Lazy<Tool> = once_cell::sync::Lazy::new(|| {
    Tool {
        name: "NewTrigger".into(),
        description: "Create a dynamic trigger rule. Use this when the user asks pie to create an automation or trigger. Extract the natural-language condition and the action from the user's request instead of requiring a fixed phrase.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "condition": {
                    "type": "string",
                    "description": "The natural-language condition that should be evaluated against future trigger events.",
                },
                "action": {
                    "type": "string",
                    "description": "The action to perform when the condition matches. This may be a shell command or a natural-language instruction.",
                },
                "spec": {
                    "type": "string",
                    "description": "Fallback complete trigger rule text when condition and action cannot be supplied separately.",
                },
                "fire_once": {
                    "type": "boolean",
                    "description": "Whether to disable the rule after the first successful match. Defaults to true unless the user explicitly asks for a repeating trigger.",
                },
                "promote_to_chat": {
                    "type": "boolean",
                    "description": "Whether successful trigger output should be inserted into the parent chat context so future turns can see it. Defaults to false unless the user explicitly asks for that behavior.",
                }
            },
            "required": ["condition", "action"],
            "additionalProperties": false,
        }),
    }
});

static LIST_TRIGGERS_TOOL: once_cell::sync::Lazy<Tool> = once_cell::sync::Lazy::new(|| {
    Tool {
    name: "ListTriggers".into(),
    description: "List dynamic trigger rules currently registered in pie. Use this when the user asks to view, list, show, inspect, or find trigger ids.".into(),
    parameters: json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    }),
}
});

static REMOVE_TRIGGER_TOOL: once_cell::sync::Lazy<Tool> = once_cell::sync::Lazy::new(|| {
    Tool {
        name: "RemoveTrigger".into(),
        description: "Delete dynamic trigger rules. Use this when the user asks pie to delete, remove, or clear an existing dynamic trigger.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The exact dynamic trigger rule id to remove.",
                },
                "all": {
                    "type": "boolean",
                    "description": "Set true only when the user explicitly asks to remove all dynamic trigger rules.",
                }
            },
            "additionalProperties": false,
        }),
    }
});

static SET_TRIGGER_STATE_TOOL: once_cell::sync::Lazy<Tool> = once_cell::sync::Lazy::new(|| {
    Tool {
        name: "SetTriggerState".into(),
        description: "Enable or disable an existing dynamic trigger rule without deleting it. Use this when the user asks to pause, disable, enable, or resume a trigger.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The exact dynamic trigger rule id to update.",
                },
                "enabled": {
                    "type": "boolean",
                    "description": "Set false to pause or disable the trigger; set true to enable or resume it.",
                }
            },
            "required": ["id", "enabled"],
            "additionalProperties": false,
        }),
    }
});

#[cfg(test)]
mod tests {
    use super::*;
    use pie_agent_core::{
        CredentialScope, PayloadVisibility, ReplacementPolicy, SourceKind, TriggerAuthority,
        TriggerRuntimeSnapshot, TriggerSource,
    };

    #[test]
    fn parses_chinese_trigger_rule() {
        let spec = concat!(
            "\u{5f53}",
            "\u{5728} github \u{4e0a}\u{6709}\u{65b0} issue",
            "\u{7684}\u{65f6}\u{5019}\u{ff0c}\u{6267}\u{884c} ./notify.sh"
        );
        let parsed = parse_trigger_rule(spec).expect("parse");
        assert_eq!(
            parsed.condition,
            "\u{5728} github \u{4e0a}\u{6709}\u{65b0} issue"
        );
        assert_eq!(parsed.action, "./notify.sh");
    }

    #[test]
    fn parses_english_trigger_rule() {
        let parsed = parse_trigger_rule("when a build finishes, run cargo test").expect("parse");
        assert_eq!(parsed.condition, "a build finishes");
        assert_eq!(parsed.action, "cargo test");
    }

    #[test]
    fn parses_chinese_if_then_trigger_rule() {
        let condition = "\u{73b0}\u{5728}\u{662f} 11pm";
        let action = "\u{5199}\u{4e00}\u{4e2a} tmp \u{6587}\u{4ef6}";
        let spec = format!("\u{5982}\u{679c}{condition}\u{ff0c}\u{5219}{action}");
        let parsed = parse_trigger_rule(&spec).expect("parse");
        assert_eq!(parsed.condition, condition);
        assert_eq!(parsed.action, action);
    }

    #[test]
    fn rejects_missing_action_separator() {
        let err = parse_trigger_rule(&format!("{ZH_WHEN_PREFIX}\u{6709}\u{65b0} issue"))
            .expect_err("missing action");
        assert_eq!(err, ParseTriggerRuleError::MissingAction);
    }

    #[test]
    fn persists_rules_when_storage_path_is_configured() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("triggers.json");
        let registry = DynamicTriggerRegistry::new();
        registry.load_from_path(&path).expect("load empty");
        let rule = registry
            .add_rule("the event says build finished", "echo fired")
            .expect("add");

        let reloaded = DynamicTriggerRegistry::new();
        reloaded.load_from_path(&path).expect("reload");
        assert_eq!(reloaded.list(), vec![rule]);
    }

    #[test]
    fn storage_paths_keep_session_rules_isolated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path_a = dir.path().join("session-a.triggers.json");
        let path_b = dir.path().join("session-b.triggers.json");

        let registry_a = DynamicTriggerRegistry::new();
        registry_a.load_from_path(&path_a).expect("load a");
        registry_a
            .add_rule("event for session a", "echo a")
            .expect("add");

        let registry_b = DynamicTriggerRegistry::new();
        registry_b.load_from_path(&path_b).expect("load b");
        assert!(registry_b.list().is_empty());

        let reloaded_a = DynamicTriggerRegistry::new();
        reloaded_a.load_from_path(&path_a).expect("reload a");
        assert_eq!(reloaded_a.list().len(), 1);
        assert_eq!(reloaded_a.list()[0].condition, "event for session a");
    }

    #[test]
    fn removing_rule_updates_storage_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("triggers.json");
        let registry = DynamicTriggerRegistry::new();
        registry.load_from_path(&path).expect("load empty");
        let rule = registry
            .add_rule("the event says stale", "echo stale")
            .expect("add");

        let removed = registry.remove_rule(&rule.id).expect("remove");
        assert_eq!(removed, Some(rule));

        let reloaded = DynamicTriggerRegistry::new();
        reloaded.load_from_path(&path).expect("reload");
        assert!(reloaded.list().is_empty());
    }

    #[test]
    fn fire_once_rules_can_be_marked_fired() {
        let registry = DynamicTriggerRegistry::new();
        let rule = registry
            .add_rule("event says fire once", "echo once")
            .expect("rule");

        let changed = registry
            .mark_rules_fired(std::slice::from_ref(&rule.id))
            .expect("mark fired");
        assert_eq!(changed.len(), 1);

        let rules = registry.list();
        assert_eq!(rules.len(), 1);
        assert!(!rules[0].enabled);
        assert!(rules[0].fire_once);
        assert!(rules[0].fired_at.is_some());
    }

    #[test]
    fn repeat_rules_are_not_disabled_when_marked_fired() {
        let registry = DynamicTriggerRegistry::new();
        let rule = registry
            .add_rule_with_options("event says repeat", "echo repeat", false)
            .expect("rule");

        let changed = registry
            .mark_rules_fired(std::slice::from_ref(&rule.id))
            .expect("mark fired");
        assert!(changed.is_empty());

        let rules = registry.list();
        assert_eq!(rules.len(), 1);
        assert!(rules[0].enabled);
        assert!(!rules[0].fire_once);
        assert!(rules[0].fired_at.is_none());
    }

    #[test]
    fn set_rule_enabled_reactivates_fired_fire_once_rule() {
        let registry = DynamicTriggerRegistry::new();
        let rule = registry
            .add_rule("event says reactivate", "echo again")
            .expect("rule");
        registry
            .mark_rules_fired(std::slice::from_ref(&rule.id))
            .expect("mark fired");

        let updated = registry
            .set_rule_enabled(&rule.id, true)
            .expect("enable")
            .expect("rule");
        assert!(updated.enabled);
        assert!(updated.fired_at.is_none());
    }

    #[test]
    fn extracts_dynamic_rule_ids_from_summary() {
        let text =
            "matched dyn-1234567890abcdef1234567890abcdef and dyn-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(
            extract_dynamic_rule_ids(text),
            vec![
                "dyn-1234567890abcdef1234567890abcdef".to_string(),
                "dyn-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn periodic_hook_emits_check_trigger_when_rules_exist() {
        let registry = DynamicTriggerRegistry::new();
        registry
            .add_rule("a periodic check arrives", "echo fired")
            .expect("rule");
        let hook = DynamicTriggerCheckHook::with_interval(registry, Duration::from_millis(5));
        let (sink, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let task = tokio::spawn(async move { hook.run(sink).await });
        let trigger = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("hook should emit")
            .expect("trigger");
        task.abort();

        assert_eq!(trigger.source_label, "local:dynamic");
        assert_eq!(trigger.event_label, "dynamic periodic check");
        assert!(
            trigger
                .payload_summary
                .as_deref()
                .unwrap_or_default()
                .contains("1 enabled rule")
        );
    }

    #[tokio::test]
    async fn action_hook_wraps_event_and_rules_for_agent_evaluation() {
        let registry = DynamicTriggerRegistry::new();
        let rule = registry
            .add_from_spec("when the event mentions build finished, run echo done")
            .expect("rule");
        let hook = before_trigger_action_hook(registry);
        let action = hook(
            BeforeTriggerActionContext {
                trigger: Trigger {
                    source: TriggerSource::Local {
                        subkind: "test".into(),
                    },
                    source_kind: SourceKind::Local,
                    source_label: "local:test".into(),
                    event_label: "build finished".into(),
                    payload_visibility: PayloadVisibility::Local,
                    payload_summary: Some("build finished successfully".into()),
                    payload: None,
                    idempotency_key: "test-key".into(),
                    replacement_policy: ReplacementPolicy::Drop,
                    trace_id: "trace-test".into(),
                    authority: TriggerAuthority {
                        principal_id: "test".into(),
                        principal_label: "test".into(),
                        credential_scope: CredentialScope::User,
                        allowed_source_actions: vec![],
                        expires_at: None,
                    },
                    received_at: Utc::now(),
                },
                runtime: TriggerRuntimeSnapshot {
                    dedup_entries: 0,
                    active_traces: 0,
                    accepted_total: 0,
                    deduped_total: 0,
                    cycle_suppressed_total: 0,
                },
            },
            CancellationToken::new(),
        )
        .await;

        assert!(action.prompt.contains(&rule.id));
        assert!(action.prompt.contains("build finished"));
        assert!(action.prompt.contains("echo done"));
        assert!(action.prompt.contains("with the available tools"));
        assert!(action.prompt.contains("\"payload\""));
        assert!(action.prompt.contains("environment variables"));
        assert!(
            action
                .prompt
                .contains("include the requested file contents")
        );
        assert!(matches!(action.promote, PromoteAction::None));
    }

    /// `payload_visibility = Local` (the default for most sources) must drop the raw
    /// `payload` JSON from the sub-agent prompt. The dynamic trigger evaluator runs in a
    /// sub-agent that calls a model provider, so a payload field populated by any future
    /// source (Cloudflare hub, local file-watcher with file contents, etc.) MUST NOT
    /// reach the provider context unless the source explicitly declared
    /// `PayloadVisibility::Shared`. The previous implementation serialized
    /// `trigger.payload` unconditionally, which bypassed the RFC 0 §3.2.2 / RFC 1 §4.2.3
    /// privacy contract and was flagged as a HIGH blocker by all reviewers on the
    /// `dynamic trigger workflow` commit.
    #[tokio::test]
    async fn local_payload_visibility_does_not_leak_payload_into_sub_agent_prompt() {
        let registry = DynamicTriggerRegistry::new();
        let _rule = registry
            .add_from_spec("when something happens, run echo nothing")
            .expect("rule");
        let hook = before_trigger_action_hook(registry);
        // Sentinel chosen so a substring search reliably fails if the payload leaks.
        let sentinel = "SECRET_PAYLOAD_SHOULD_NOT_REACH_MODEL_2K7";
        let action = hook(
            BeforeTriggerActionContext {
                trigger: Trigger {
                    source: TriggerSource::Local {
                        subkind: "test".into(),
                    },
                    source_kind: SourceKind::Local,
                    source_label: "local:test".into(),
                    event_label: "build finished".into(),
                    payload_visibility: PayloadVisibility::Local,
                    payload_summary: Some("safe summary".into()),
                    payload: Some(serde_json::json!({
                        "leaked_field": sentinel,
                        "nested": { "also_leaked": sentinel },
                    })),
                    idempotency_key: "test-key".into(),
                    replacement_policy: ReplacementPolicy::Drop,
                    trace_id: "trace-test".into(),
                    authority: TriggerAuthority {
                        principal_id: "test".into(),
                        principal_label: "test".into(),
                        credential_scope: CredentialScope::User,
                        allowed_source_actions: vec![],
                        expires_at: None,
                    },
                    received_at: Utc::now(),
                },
                runtime: TriggerRuntimeSnapshot {
                    dedup_entries: 0,
                    active_traces: 0,
                    accepted_total: 0,
                    deduped_total: 0,
                    cycle_suppressed_total: 0,
                },
            },
            CancellationToken::new(),
        )
        .await;

        assert!(
            !action.prompt.contains(sentinel),
            "Local payload must not leak into the sub-agent prompt — found sentinel in:\n{}",
            action.prompt
        );
        // The safe `payload_summary` field MUST still survive — we are dropping the raw
        // payload, not the entire envelope.
        assert!(
            action.prompt.contains("safe summary"),
            "payload_summary should still be visible: {}",
            action.prompt
        );
    }

    /// Counterpart: when a source explicitly opts in to `PayloadVisibility::Shared`, the
    /// full payload reaches the sub-agent prompt as before. Pins that the gate is a
    /// per-source decision, not a blanket redaction.
    #[tokio::test]
    async fn shared_payload_visibility_includes_payload_in_sub_agent_prompt() {
        let registry = DynamicTriggerRegistry::new();
        let _rule = registry
            .add_from_spec("when something happens, run echo nothing")
            .expect("rule");
        let hook = before_trigger_action_hook(registry);
        let marker = "shared-payload-marker-must-appear";
        let action = hook(
            BeforeTriggerActionContext {
                trigger: Trigger {
                    source: TriggerSource::Hub {
                        topic: "test".into(),
                    },
                    source_kind: SourceKind::Hub,
                    source_label: "hub:test".into(),
                    event_label: "explicit shared".into(),
                    payload_visibility: PayloadVisibility::Shared,
                    payload_summary: Some("shared event".into()),
                    payload: Some(serde_json::json!({ "value": marker })),
                    idempotency_key: "shared-key".into(),
                    replacement_policy: ReplacementPolicy::Drop,
                    trace_id: "trace-shared".into(),
                    authority: TriggerAuthority {
                        principal_id: "hub".into(),
                        principal_label: "hub".into(),
                        credential_scope: CredentialScope::User,
                        allowed_source_actions: vec![],
                        expires_at: None,
                    },
                    received_at: Utc::now(),
                },
                runtime: TriggerRuntimeSnapshot {
                    dedup_entries: 0,
                    active_traces: 0,
                    accepted_total: 0,
                    deduped_total: 0,
                    cycle_suppressed_total: 0,
                },
            },
            CancellationToken::new(),
        )
        .await;

        assert!(
            action.prompt.contains(marker),
            "Shared payload should reach the sub-agent prompt — marker missing from:\n{}",
            action.prompt
        );
    }

    /// `Redacted` visibility behaves like `Local` for prompt rendering: the raw payload is
    /// dropped even though the source may have attached one. The runtime contract says
    /// `Redacted` is the strongest of the three.
    #[tokio::test]
    async fn redacted_payload_visibility_does_not_leak_payload_into_sub_agent_prompt() {
        let registry = DynamicTriggerRegistry::new();
        let _rule = registry
            .add_from_spec("when something happens, run echo nothing")
            .expect("rule");
        let hook = before_trigger_action_hook(registry);
        let sentinel = "REDACTED_FIELD_MUST_BE_DROPPED_9X4";
        let action = hook(
            BeforeTriggerActionContext {
                trigger: Trigger {
                    source: TriggerSource::Local {
                        subkind: "test".into(),
                    },
                    source_kind: SourceKind::Local,
                    source_label: "local:test".into(),
                    event_label: "sensitive event".into(),
                    payload_visibility: PayloadVisibility::Redacted,
                    payload_summary: Some("redacted summary only".into()),
                    payload: Some(serde_json::json!({ "credential": sentinel })),
                    idempotency_key: "redacted-key".into(),
                    replacement_policy: ReplacementPolicy::Drop,
                    trace_id: "trace-redacted".into(),
                    authority: TriggerAuthority {
                        principal_id: "test".into(),
                        principal_label: "test".into(),
                        credential_scope: CredentialScope::User,
                        allowed_source_actions: vec![],
                        expires_at: None,
                    },
                    received_at: Utc::now(),
                },
                runtime: TriggerRuntimeSnapshot {
                    dedup_entries: 0,
                    active_traces: 0,
                    accepted_total: 0,
                    deduped_total: 0,
                    cycle_suppressed_total: 0,
                },
            },
            CancellationToken::new(),
        )
        .await;

        assert!(
            !action.prompt.contains(sentinel),
            "Redacted payload must not leak into the sub-agent prompt — found sentinel in:\n{}",
            action.prompt
        );
    }
}
