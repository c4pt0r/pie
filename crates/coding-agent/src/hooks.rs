//! User-configured CLI hooks.
//!
//! Hooks are intentionally a coding-agent concern, not an agent-core behavior modifier:
//! they observe `AgentEvent`s and run best-effort side effects (shell commands and/or HTTP
//! webhooks). They never mutate agent state and failures are surfaced as diagnostics/logs,
//! not prompt failures.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use pie_agent_core::{
    AgentEvent, AgentListener, AgentMessage, HarnessEvent, HarnessListener, ThinkingLevel,
};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::config::base_dir;

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_SUMMARY_CHARS: usize = 2_000;

#[derive(Debug)]
pub struct LoadedHooks {
    pub runner: Arc<HookRunner>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug)]
pub struct HookRunner {
    rules: Vec<HookRule>,
    session_id: String,
    cwd: PathBuf,
    model_provider: String,
    model_id: String,
    thinking_level: String,
    client: reqwest::Client,
}

#[derive(Clone, Debug)]
struct HookRule {
    event: HookEvent,
    command: Option<String>,
    webhook: Option<String>,
    headers: BTreeMap<String, String>,
    timeout_ms: u64,
    cwd: HookCwd,
    on_failure: OnFailure,
    tool: Option<String>,
    source: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HookEvent {
    AgentStart,
    AgentEnd,
    TurnStart,
    TurnEnd,
    MessageStart,
    MessageUpdate,
    MessageEnd,
    ToolStart,
    ToolUpdate,
    ToolEnd,
    Compaction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum HookCwd {
    Project,
    Pie,
    Home,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OnFailure {
    Warn,
    Ignore,
}

#[derive(Debug, Deserialize)]
struct HooksFile {
    #[serde(default)]
    allow_project_hooks: bool,
    #[serde(default, rename = "hook")]
    hooks: Vec<HookRuleConfig>,
}

#[derive(Debug, Deserialize)]
struct HookRuleConfig {
    event: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    webhook: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    cwd: Option<HookCwd>,
    #[serde(default)]
    on_failure: Option<OnFailure>,
    #[serde(default)]
    tool: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HookPayload {
    event: String,
    session_id: String,
    cwd: String,
    model_provider: String,
    model_id: String,
    thinking_level: String,
    source: Option<String>,
    message_kind: Option<String>,
    message_summary: Option<String>,
    assistant_event: Option<String>,
    tool_call_id: Option<String>,
    tool_name: Option<String>,
    tool_is_error: Option<bool>,
    tool_args: Option<serde_json::Value>,
    tool_result_summary: Option<String>,
    compaction_trigger: Option<String>,
    compaction_tokens_before: Option<u64>,
    compaction_summary: Option<String>,
}

struct EventData {
    event: HookEvent,
    message_kind: Option<String>,
    message_summary: Option<String>,
    assistant_event: Option<String>,
    tool_call_id: Option<String>,
    tool_name: Option<String>,
    tool_is_error: Option<bool>,
    tool_args: Option<serde_json::Value>,
    tool_result_summary: Option<String>,
    compaction_trigger: Option<String>,
    compaction_tokens_before: Option<u64>,
    compaction_summary: Option<String>,
}

pub async fn load(
    cwd: &Path,
    session_id: impl Into<String>,
    model: Option<&pie_ai::Model>,
    thinking_level: Option<ThinkingLevel>,
) -> LoadedHooks {
    let session_id = session_id.into();
    let (model_provider, model_id) = model
        .map(|m| (m.provider.0.clone(), m.id.clone()))
        .unwrap_or_else(|| ("".into(), "".into()));
    let thinking_level = thinking_level
        .map(|t| t.as_str().to_string())
        .unwrap_or_else(|| "off".into());

    let user_path = base_dir().join("hooks.toml");
    let project_path = cwd.join(".pie").join("hooks.toml");
    let mut diagnostics = Vec::new();
    let mut rules = Vec::new();

    let user_file = read_file(&user_path, "user", &mut diagnostics).await;
    let allow_project = std::env::var("PIE_ALLOW_PROJECT_HOOKS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
        || user_file
            .as_ref()
            .map(|f| f.allow_project_hooks)
            .unwrap_or(false);

    if let Some(file) = user_file {
        push_rules(file, "user", &mut rules, &mut diagnostics);
    }

    if project_path.exists() {
        if allow_project {
            if let Some(file) = read_file(&project_path, "project", &mut diagnostics).await {
                push_rules(file, "project", &mut rules, &mut diagnostics);
            }
        } else {
            diagnostics.push(format!(
                "project hooks ignored at {}; set allow_project_hooks = true in {} or PIE_ALLOW_PROJECT_HOOKS=1",
                project_path.display(),
                user_path.display()
            ));
        }
    }

    let client = reqwest::Client::builder()
        .user_agent(format!("pie/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    LoadedHooks {
        runner: Arc::new(HookRunner {
            rules,
            session_id,
            cwd: cwd.to_path_buf(),
            model_provider,
            model_id,
            thinking_level,
            client,
        }),
        diagnostics,
    }
}

async fn read_file(path: &Path, label: &str, diagnostics: &mut Vec<String>) -> Option<HooksFile> {
    if !path.exists() {
        return None;
    }
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(e) => {
            diagnostics.push(format!(
                "hooks {label}: read {} failed: {e}",
                path.display()
            ));
            return None;
        }
    };
    match toml::from_str::<HooksFile>(&text) {
        Ok(file) => Some(file),
        Err(e) => {
            diagnostics.push(format!(
                "hooks {label}: parse {} failed: {e}",
                path.display()
            ));
            None
        }
    }
}

fn push_rules(
    file: HooksFile,
    source: &str,
    rules: &mut Vec<HookRule>,
    diagnostics: &mut Vec<String>,
) {
    for (idx, cfg) in file.hooks.into_iter().enumerate() {
        if cfg.enabled == Some(false) {
            continue;
        }
        let event = match HookEvent::parse(&cfg.event) {
            Some(event) => event,
            None => {
                diagnostics.push(format!(
                    "hooks {source}: hook #{} has unknown event {:?}",
                    idx + 1,
                    cfg.event
                ));
                continue;
            }
        };
        if cfg.command.as_deref().unwrap_or("").trim().is_empty() && cfg.webhook.is_none() {
            diagnostics.push(format!(
                "hooks {source}: hook #{} has neither command nor webhook",
                idx + 1
            ));
            continue;
        }
        rules.push(HookRule {
            event,
            command: cfg.command.filter(|s| !s.trim().is_empty()),
            webhook: cfg.webhook,
            headers: cfg.headers,
            timeout_ms: cfg.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            cwd: cfg.cwd.unwrap_or(HookCwd::Project),
            on_failure: cfg.on_failure.unwrap_or(OnFailure::Warn),
            tool: cfg.tool,
            source: source.to_string(),
        });
    }
}

impl HookRunner {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn listener(self: &Arc<Self>) -> AgentListener {
        let me = self.clone();
        Arc::new(move |event, cancel| {
            let me = me.clone();
            Box::pin(async move {
                me.handle_event(&event, cancel).await;
            })
        })
    }

    pub fn harness_listener(self: &Arc<Self>) -> HarnessListener {
        let me = self.clone();
        Arc::new(move |event| {
            let me = me.clone();
            tokio::spawn(async move {
                me.handle_harness_event(&event, CancellationToken::new())
                    .await;
            });
        })
    }

    pub async fn handle_event(&self, event: &AgentEvent, cancel: CancellationToken) {
        let Some(data) = EventData::from_agent_event(event) else {
            return;
        };
        self.handle_data(data, cancel).await;
    }

    pub async fn handle_harness_event(&self, event: &HarnessEvent, cancel: CancellationToken) {
        let Some(data) = EventData::from_harness_event(event) else {
            return;
        };
        self.handle_data(data, cancel).await;
    }

    async fn handle_data(&self, data: EventData, cancel: CancellationToken) {
        let matching = self
            .rules
            .iter()
            .filter(|rule| rule.matches(&data))
            .collect::<Vec<_>>();
        if matching.is_empty() {
            return;
        }

        for rule in matching {
            if cancel.is_cancelled() {
                return;
            }
            let payload = self.payload_for(rule, &data);
            if let Err(e) = self.run_rule(rule, &payload, cancel.clone()).await
                && matches!(rule.on_failure, OnFailure::Warn)
            {
                tracing::warn!("hook {} {} failed: {e}", rule.source, rule.event.as_str());
            }
        }
    }

    fn payload_for(&self, rule: &HookRule, data: &EventData) -> HookPayload {
        HookPayload {
            event: data.event.as_str().to_string(),
            session_id: self.session_id.clone(),
            cwd: self.cwd.display().to_string(),
            model_provider: self.model_provider.clone(),
            model_id: self.model_id.clone(),
            thinking_level: self.thinking_level.clone(),
            source: Some(rule.source.clone()),
            message_kind: data.message_kind.clone(),
            message_summary: data.message_summary.clone(),
            assistant_event: data.assistant_event.clone(),
            tool_call_id: data.tool_call_id.clone(),
            tool_name: data.tool_name.clone(),
            tool_is_error: data.tool_is_error,
            tool_args: data.tool_args.clone(),
            tool_result_summary: data.tool_result_summary.clone(),
            compaction_trigger: data.compaction_trigger.clone(),
            compaction_tokens_before: data.compaction_tokens_before,
            compaction_summary: data.compaction_summary.clone(),
        }
    }

    async fn run_rule(
        &self,
        rule: &HookRule,
        payload: &HookPayload,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let payload_json = serde_json::to_string(payload)?;
        let payload_path = write_payload_file(&payload_json).await?;

        let result = async {
            if let Some(command) = &rule.command {
                self.run_command(rule, command, payload, &payload_path, cancel.clone())
                    .await?;
            }
            if let Some(url) = &rule.webhook {
                self.run_webhook(rule, url, &payload_json, cancel.clone())
                    .await?;
            }
            anyhow::Ok(())
        }
        .await;

        let _ = tokio::fs::remove_file(&payload_path).await;
        result
    }

    async fn run_command(
        &self,
        rule: &HookRule,
        command: &str,
        payload: &HookPayload,
        payload_path: &Path,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        // Previously: `cmd.output()` raced against `tokio::time::timeout` + cancel via
        // `select!`. Either non-completion branch left `sh -c` running in the background
        // (along with anything it spawned), so a hook that ran `(slow_thing) & wait` would
        // leak descendants past its declared `timeout_ms`. Mirrors the bash-tool fix in
        // PR #41 and the `NativeEnv::exec` fix in PR #40: spawn explicitly, put the child
        // in its own process group on Unix via `setsid`, and `killpg(pgid, SIGKILL)` the
        // whole tree on timeout / cancel. `kill_on_drop(true)` is the cross-platform
        // backstop.
        let timeout = Duration::from_millis(rule.timeout_ms);
        let mut cmd = Command::new(shell_program());
        cmd.arg(shell_arg())
            .arg(command)
            .current_dir(self.cwd_for(rule))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .envs(env_for(payload, payload_path))
            .kill_on_drop(true);

        #[cfg(unix)]
        {
            // SAFETY: `setsid` is async-signal-safe per POSIX and has no Rust state to
            // invalidate. The child becomes session and process-group leader; SIGKILL to
            // `-pgid` then targets the whole tree we just spawned. `tokio::process::Command`
            // exposes `pre_exec` as an inherent method so no trait import is needed.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        let child = cmd.spawn().map_err(|e| anyhow::anyhow!("spawn: {e}"))?;
        let child_pid = child.id();

        // Race the wait against the rule's timeout and the cancel token. `biased` puts
        // cancel first so a user Ctrl-C wins same-tick ties over the timeout.
        let outcome: HookOutcome = {
            let wait = child.wait_with_output();
            tokio::pin!(wait);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => HookOutcome::Cancelled,
                res = tokio::time::timeout(timeout, &mut wait) => match res {
                    Ok(out) => HookOutcome::Completed(out),
                    Err(_) => HookOutcome::TimedOut,
                },
            }
        };

        match outcome {
            HookOutcome::Completed(Ok(output)) => {
                if !output.status.success() {
                    anyhow::bail!(
                        "command exited {}: {}",
                        output.status.code().unwrap_or(-1),
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
                if !output.stdout.is_empty() {
                    tracing::debug!(
                        "hook command stdout: {}",
                        String::from_utf8_lossy(&output.stdout).trim()
                    );
                }
                if !output.stderr.is_empty() {
                    tracing::debug!(
                        "hook command stderr: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
                Ok(())
            }
            HookOutcome::Completed(Err(e)) => Err(anyhow::anyhow!(e)),
            HookOutcome::TimedOut => {
                terminate_hook_tree(child_pid).await;
                anyhow::bail!("timed out after {}ms", rule.timeout_ms);
            }
            HookOutcome::Cancelled => {
                terminate_hook_tree(child_pid).await;
                anyhow::bail!("cancelled");
            }
        }
    }
}

/// Outcome of one `run_command` race. Lifted out of `tokio::select!` so the match below
/// can spell the kill-tree path explicitly per branch rather than mixing it into the
/// select arms.
enum HookOutcome {
    Completed(std::io::Result<std::process::Output>),
    TimedOut,
    Cancelled,
}

/// Best-effort SIGKILL of the hook child's whole process group on Unix. On non-Unix targets
/// this is a no-op (the `kill_on_drop(true)` set on the `Command` is the only kill path
/// when the wait future is dropped). The pid was snapshotted with `child.id()` before the
/// wait future consumed the handle.
async fn terminate_hook_tree(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: SIGKILL on a pid we just observed via `child.id()`. `killpg` returning
        // `ESRCH` (group already gone) is benign and we don't act on the return.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    let _ = pid;
}

impl HookRunner {
    async fn run_webhook(
        &self,
        rule: &HookRule,
        url: &str,
        payload_json: &str,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let mut req = self
            .client
            .post(url)
            .timeout(Duration::from_millis(rule.timeout_ms))
            .header("Content-Type", "application/json")
            .body(payload_json.to_string());
        for (k, v) in &rule.headers {
            req = req.header(k, v);
        }
        let resp = tokio::select! {
            r = req.send() => r?,
            _ = cancel.cancelled() => {
                anyhow::bail!("cancelled");
            }
        };
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "webhook status {status}: {}",
                text.chars().take(500).collect::<String>()
            );
        }
        Ok(())
    }

    fn cwd_for(&self, rule: &HookRule) -> PathBuf {
        match rule.cwd {
            HookCwd::Project => self.cwd.clone(),
            HookCwd::Pie => base_dir(),
            HookCwd::Home => directories::BaseDirs::new()
                .map(|d| d.home_dir().to_path_buf())
                .unwrap_or_else(|| self.cwd.clone()),
        }
    }
}

impl HookRule {
    fn matches(&self, data: &EventData) -> bool {
        if self.event != data.event {
            return false;
        }
        if let Some(tool) = &self.tool
            && data.tool_name.as_deref() != Some(tool.as_str())
        {
            return false;
        }
        true
    }
}

impl HookEvent {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "agent_start" => Some(Self::AgentStart),
            "agent_end" => Some(Self::AgentEnd),
            "turn_start" => Some(Self::TurnStart),
            "turn_end" => Some(Self::TurnEnd),
            "message_start" => Some(Self::MessageStart),
            "message_update" => Some(Self::MessageUpdate),
            "message_end" => Some(Self::MessageEnd),
            "tool_start" => Some(Self::ToolStart),
            "tool_update" => Some(Self::ToolUpdate),
            "tool_end" => Some(Self::ToolEnd),
            "compaction" => Some(Self::Compaction),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::AgentStart => "agent_start",
            Self::AgentEnd => "agent_end",
            Self::TurnStart => "turn_start",
            Self::TurnEnd => "turn_end",
            Self::MessageStart => "message_start",
            Self::MessageUpdate => "message_update",
            Self::MessageEnd => "message_end",
            Self::ToolStart => "tool_start",
            Self::ToolUpdate => "tool_update",
            Self::ToolEnd => "tool_end",
            Self::Compaction => "compaction",
        }
    }
}

impl EventData {
    fn from_agent_event(event: &AgentEvent) -> Option<Self> {
        match event {
            AgentEvent::AgentStart => Some(Self::basic(HookEvent::AgentStart)),
            AgentEvent::AgentEnd { .. } => Some(Self::basic(HookEvent::AgentEnd)),
            AgentEvent::TurnStart => Some(Self::basic(HookEvent::TurnStart)),
            AgentEvent::TurnEnd { message, .. } => {
                let mut d = Self::basic(HookEvent::TurnEnd);
                d.message_kind = Some(message_kind(message));
                d.message_summary = Some(message_summary(message));
                Some(d)
            }
            AgentEvent::MessageStart { message } => {
                let mut d = Self::basic(HookEvent::MessageStart);
                d.message_kind = Some(message_kind(message));
                d.message_summary = Some(message_summary(message));
                Some(d)
            }
            AgentEvent::MessageUpdate {
                message,
                assistant_message_event,
            } => {
                let mut d = Self::basic(HookEvent::MessageUpdate);
                d.message_kind = Some(message_kind(message));
                d.message_summary = Some(message_summary(message));
                d.assistant_event = Some(assistant_event_name(assistant_message_event).into());
                Some(d)
            }
            AgentEvent::MessageEnd { message } => {
                let mut d = Self::basic(HookEvent::MessageEnd);
                d.message_kind = Some(message_kind(message));
                d.message_summary = Some(message_summary(message));
                Some(d)
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                let mut d = Self::basic(HookEvent::ToolStart);
                d.tool_call_id = Some(tool_call_id.clone());
                d.tool_name = Some(tool_name.clone());
                d.tool_args = Some(args.clone());
                Some(d)
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => {
                let mut d = Self::basic(HookEvent::ToolUpdate);
                d.tool_call_id = Some(tool_call_id.clone());
                d.tool_name = Some(tool_name.clone());
                d.tool_args = Some(args.clone());
                d.tool_result_summary = Some(result_summary(partial_result));
                Some(d)
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                let mut d = Self::basic(HookEvent::ToolEnd);
                d.tool_call_id = Some(tool_call_id.clone());
                d.tool_name = Some(tool_name.clone());
                d.tool_is_error = Some(*is_error);
                d.tool_result_summary = Some(result_summary(result));
                Some(d)
            }
        }
    }

    fn basic(event: HookEvent) -> Self {
        Self {
            event,
            message_kind: None,
            message_summary: None,
            assistant_event: None,
            tool_call_id: None,
            tool_name: None,
            tool_is_error: None,
            tool_args: None,
            tool_result_summary: None,
            compaction_trigger: None,
            compaction_tokens_before: None,
            compaction_summary: None,
        }
    }

    fn from_harness_event(event: &HarnessEvent) -> Option<Self> {
        match event {
            HarnessEvent::Compaction {
                from_hook,
                summary,
                tokens_before,
            } => {
                let mut d = Self::basic(HookEvent::Compaction);
                d.compaction_trigger = Some(compaction_trigger(*from_hook).into());
                d.compaction_tokens_before = Some(*tokens_before);
                d.compaction_summary = Some(truncate(summary));
                Some(d)
            }
            HarnessEvent::SessionStart { .. }
            | HarnessEvent::Branch { .. }
            | HarnessEvent::TriggerHandlingStart { .. }
            | HarnessEvent::TriggerHandled { .. }
            | HarnessEvent::PersistenceError { .. }
            | HarnessEvent::TriggerExecutionStarted { .. }
            | HarnessEvent::TriggerCompleted { .. }
            | HarnessEvent::TriggerFailed { .. }
            | HarnessEvent::TriggerPromoted { .. }
            | HarnessEvent::TriggerRequestsMainRun { .. }
            | HarnessEvent::PromotionPending { .. }
            | HarnessEvent::TurnEnded { .. } => None,
        }
    }
}

fn compaction_trigger(from_hook: bool) -> &'static str {
    // In current AgentHarness call sites, true is the explicit `force_compact` path used by
    // `/compact`; false is the threshold-based automatic compaction path.
    if from_hook { "manual" } else { "auto" }
}

fn env_for(payload: &HookPayload, payload_path: &Path) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("PIE_HOOK_EVENT".into(), payload.event.clone());
    env.insert(
        "PIE_HOOK_PAYLOAD".into(),
        payload_path.display().to_string(),
    );
    env.insert("PIE_SESSION_ID".into(), payload.session_id.clone());
    env.insert("PIE_CWD".into(), payload.cwd.clone());
    env.insert("PIE_MODEL_PROVIDER".into(), payload.model_provider.clone());
    env.insert("PIE_MODEL_ID".into(), payload.model_id.clone());
    env.insert("PIE_THINKING_LEVEL".into(), payload.thinking_level.clone());
    if let Some(v) = &payload.message_kind {
        env.insert("PIE_MESSAGE_KIND".into(), v.clone());
    }
    if let Some(v) = &payload.assistant_event {
        env.insert("PIE_ASSISTANT_EVENT".into(), v.clone());
    }
    if let Some(v) = &payload.tool_call_id {
        env.insert("PIE_TOOL_CALL_ID".into(), v.clone());
    }
    if let Some(v) = &payload.tool_name {
        env.insert("PIE_TOOL_NAME".into(), v.clone());
    }
    if let Some(v) = payload.tool_is_error {
        env.insert("PIE_TOOL_IS_ERROR".into(), v.to_string());
    }
    if let Some(v) = &payload.compaction_trigger {
        env.insert("PIE_COMPACTION_TRIGGER".into(), v.clone());
    }
    if let Some(v) = payload.compaction_tokens_before {
        env.insert("PIE_COMPACTION_TOKENS_BEFORE".into(), v.to_string());
    }
    env
}

async fn write_payload_file(payload_json: &str) -> anyhow::Result<PathBuf> {
    let dir = std::env::temp_dir().join("pie-hooks");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{}.json", uuid::Uuid::new_v4()));
    tokio::fs::write(&path, payload_json).await?;
    Ok(path)
}

fn message_kind(message: &AgentMessage) -> String {
    match message {
        AgentMessage::Llm(pie_ai::Message::User(_)) => "user".into(),
        AgentMessage::Llm(pie_ai::Message::Assistant(_)) => "assistant".into(),
        AgentMessage::Llm(pie_ai::Message::ToolResult(_)) => "tool_result".into(),
        AgentMessage::Custom(c) => c.role.clone(),
    }
}

fn message_summary(message: &AgentMessage) -> String {
    let text = match message {
        AgentMessage::Llm(pie_ai::Message::User(u)) => match &u.content {
            pie_ai::UserContent::Text(t) => t.clone(),
            pie_ai::UserContent::Blocks(blocks) => blocks
                .iter()
                .map(|b| match b {
                    pie_ai::UserContentBlock::Text(t) => t.text.clone(),
                    pie_ai::UserContentBlock::Image(i) => format!("<image {}>", i.mime_type),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        },
        AgentMessage::Llm(pie_ai::Message::Assistant(a)) => a
            .content
            .iter()
            .map(|b| match b {
                pie_ai::ContentBlock::Text(t) => t.text.clone(),
                pie_ai::ContentBlock::Thinking(_) => "<thinking>".into(),
                pie_ai::ContentBlock::ToolCall(tc) => format!("<tool_call {}>", tc.name),
                pie_ai::ContentBlock::Image(i) => format!("<image {}>", i.mime_type),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        AgentMessage::Llm(pie_ai::Message::ToolResult(t)) => t
            .content
            .iter()
            .map(|b| match b {
                pie_ai::UserContentBlock::Text(t) => t.text.clone(),
                pie_ai::UserContentBlock::Image(i) => format!("<image {}>", i.mime_type),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        AgentMessage::Custom(c) => serde_json::to_string(&c.payload).unwrap_or_default(),
    };
    truncate(&text)
}

fn result_summary(result: &pie_agent_core::AgentToolResult) -> String {
    let text = result
        .content
        .iter()
        .map(|b| match b {
            pie_ai::UserContentBlock::Text(t) => t.text.clone(),
            pie_ai::UserContentBlock::Image(i) => format!("<image {}>", i.mime_type),
        })
        .collect::<Vec<_>>()
        .join("\n");
    truncate(&text)
}

fn assistant_event_name(ev: &pie_ai::AssistantMessageEvent) -> &'static str {
    match ev {
        pie_ai::AssistantMessageEvent::Start { .. } => "start",
        pie_ai::AssistantMessageEvent::TextStart { .. } => "text_start",
        pie_ai::AssistantMessageEvent::TextDelta { .. } => "text_delta",
        pie_ai::AssistantMessageEvent::TextEnd { .. } => "text_end",
        pie_ai::AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
        pie_ai::AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
        pie_ai::AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
        pie_ai::AssistantMessageEvent::ToolCallStart { .. } => "tool_call_start",
        pie_ai::AssistantMessageEvent::ToolCallDelta { .. } => "tool_call_delta",
        pie_ai::AssistantMessageEvent::ToolCallEnd { .. } => "tool_call_end",
        pie_ai::AssistantMessageEvent::Done { .. } => "done",
        pie_ai::AssistantMessageEvent::Error { .. } => "error",
    }
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_SUMMARY_CHARS {
        return s.to_string();
    }
    let mut out = s.chars().take(MAX_SUMMARY_CHARS).collect::<String>();
    out.push('…');
    out
}

#[cfg(unix)]
fn shell_program() -> &'static str {
    "sh"
}

#[cfg(unix)]
fn shell_arg() -> &'static str {
    "-c"
}

#[cfg(windows)]
fn shell_program() -> &'static str {
    "cmd"
}

#[cfg(windows)]
fn shell_arg() -> &'static str {
    "/C"
}

#[cfg(test)]
mod tests {
    use super::*;
    use pie_agent_core::{AgentToolResult, ToolExecutionMode};
    use pie_ai::{ToolResultMessage, ToolResultRole, UserContentBlock};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn runner(rules: Vec<HookRule>) -> HookRunner {
        HookRunner {
            rules,
            session_id: "session-1".into(),
            cwd: std::env::current_dir().unwrap(),
            model_provider: "faux".into(),
            model_id: "model".into(),
            thinking_level: "off".into(),
            client: reqwest::Client::new(),
        }
    }

    fn rule(event: HookEvent) -> HookRule {
        HookRule {
            event,
            command: None,
            webhook: None,
            headers: BTreeMap::new(),
            timeout_ms: 1_000,
            cwd: HookCwd::Project,
            on_failure: OnFailure::Warn,
            tool: None,
            source: "test".into(),
        }
    }

    #[test]
    fn parses_hook_rules_and_skips_bad_entries() {
        let file: HooksFile = toml::from_str(
            r#"
allow_project_hooks = true

[[hook]]
event = "tool_end"
command = "echo ok"
tool = "bash"

[[hook]]
event = "compaction"
command = "echo compacted"

[[hook]]
event = "not_real"
command = "echo nope"
            "#,
        )
        .unwrap();
        let mut rules = Vec::new();
        let mut diagnostics = Vec::new();
        push_rules(file, "test", &mut rules, &mut diagnostics);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].event, HookEvent::ToolEnd);
        assert_eq!(rules[0].tool.as_deref(), Some("bash"));
        assert_eq!(rules[1].event, HookEvent::Compaction);
        assert_eq!(diagnostics.len(), 1);
    }

    #[tokio::test]
    async fn command_hook_receives_env_and_payload() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("hook.out");
        let mut r = rule(HookEvent::ToolEnd);
        r.command = Some(format!(
            "printf '%s %s ' \"$PIE_HOOK_EVENT\" \"$PIE_TOOL_NAME\" > {}; test -s \"$PIE_HOOK_PAYLOAD\"",
            out.display()
        ));
        r.cwd = HookCwd::Project;
        let runner = runner(vec![r]);
        let ev = AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            result: AgentToolResult {
                content: vec![UserContentBlock::text("ok")],
                details: serde_json::Value::Null,
                terminate: None,
            },
            is_error: false,
        };
        runner.handle_event(&ev, CancellationToken::new()).await;
        let body = tokio::fs::read_to_string(out).await.unwrap();
        assert_eq!(body, "tool_end bash ");
    }

    #[tokio::test]
    async fn compaction_command_hook_receives_env_and_payload() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("hook.out");
        let mut r = rule(HookEvent::Compaction);
        r.command = Some(format!(
            "printf '%s %s %s ' \"$PIE_HOOK_EVENT\" \"$PIE_COMPACTION_TRIGGER\" \"$PIE_COMPACTION_TOKENS_BEFORE\" > {}; grep -q '\"compaction_summary\":\"summary text\"' \"$PIE_HOOK_PAYLOAD\"",
            out.display()
        ));
        let runner = runner(vec![r]);
        let ev = HarnessEvent::Compaction {
            from_hook: true,
            summary: "summary text".into(),
            tokens_before: 42,
        };
        runner
            .handle_harness_event(&ev, CancellationToken::new())
            .await;
        let body = tokio::fs::read_to_string(out).await.unwrap();
        assert_eq!(body, "compaction manual 42 ");
    }

    #[tokio::test]
    async fn webhook_hook_posts_json_payload() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(tokio::sync::Mutex::new(String::new()));
        let seen_task = seen.clone();
        let server = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap();
                *seen_task.lock().await = String::from_utf8_lossy(&buf[..n]).into_owned();
                let resp =
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let mut r = rule(HookEvent::TurnEnd);
        r.webhook = Some(format!("http://{addr}/hook"));
        let runner = runner(vec![r]);
        let ev = AgentEvent::TurnEnd {
            message: AgentMessage::Llm(pie_ai::Message::ToolResult(ToolResultMessage {
                role: ToolResultRole::ToolResult,
                tool_call_id: "call-1".into(),
                tool_name: "bash".into(),
                content: vec![UserContentBlock::text("ok")],
                details: None,
                is_error: false,
                timestamp: 0,
            })),
            tool_results: Vec::new(),
        };
        runner.handle_event(&ev, CancellationToken::new()).await;
        server.await.unwrap();
        let req = seen.lock().await.clone();
        assert!(req.starts_with("POST /hook "), "{req}");
        assert!(req.contains("\"event\":\"turn_end\""), "{req}");
    }

    #[tokio::test]
    async fn tool_filter_skips_non_matching_tool() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("hook.out");
        let mut r = rule(HookEvent::ToolEnd);
        r.tool = Some("bash".into());
        r.command = Some(format!("touch {}", out.display()));
        let runner = runner(vec![r]);
        let ev = AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            result: AgentToolResult::default(),
            is_error: false,
        };
        runner.handle_event(&ev, CancellationToken::new()).await;
        assert!(!out.exists());
    }

    #[allow(dead_code)]
    fn _keep_tool_execution_mode(_: ToolExecutionMode) {}

    /// Hook command that exceeds `timeout_ms` must be killed, including any descendant
    /// process the shell backgrounded. The previous implementation used `cmd.output()`
    /// inside a `select!` against `tokio::time::timeout`, so on timeout the underlying
    /// `sh -c` (and any `(child) & wait` subprocess) kept running.
    #[cfg(unix)]
    #[tokio::test]
    async fn command_hook_timeout_kills_descendant_process() {
        use std::time::Instant;

        // Unique marker so `pgrep` only finds the descendant this test spawned.
        let marker = "pie-hook-timeout-test-mkr-z2x7a1";
        let mut r = rule(HookEvent::ToolEnd);
        r.timeout_ms = 100;
        r.command = Some(format!("(sleep 30 && echo {marker}) & wait"));
        let runner = runner(vec![r]);

        let started = Instant::now();
        let ev = AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            result: AgentToolResult::default(),
            is_error: false,
        };
        runner.handle_event(&ev, CancellationToken::new()).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed.as_secs() < 5,
            "hook timeout path took {elapsed:?}; descendant kill did not happen in time"
        );

        // Give the kernel a beat to reap the killed group.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let pgrep = tokio::process::Command::new("pgrep")
            .arg("-f")
            .arg(marker)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = pgrep {
            let mut buf = String::new();
            if let Some(mut s) = child.stdout.take() {
                let _ = tokio::io::AsyncReadExt::read_to_string(&mut s, &mut buf).await;
            }
            let _ = child.wait().await;
            assert!(
                buf.trim().is_empty(),
                "found surviving descendant matching {marker:?} after hook timeout: pids={buf}"
            );
        }
    }

    /// Cancellation token tripped mid-hook must kill the whole shell tree, mirroring the
    /// timeout path. Mirrors `bash_tool::cancellation_kills_child_process` for hooks.
    #[cfg(unix)]
    #[tokio::test]
    async fn command_hook_cancellation_kills_descendant_process() {
        use std::time::Instant;

        let marker = "pie-hook-cancel-test-mkr-z2x7b2";
        let mut r = rule(HookEvent::ToolEnd);
        r.timeout_ms = 30_000;
        r.command = Some(format!("(sleep 30 && echo {marker}) & wait"));
        let runner = runner(vec![r]);

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel_clone.cancel();
        });

        let started = Instant::now();
        let ev = AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            result: AgentToolResult::default(),
            is_error: false,
        };
        runner.handle_event(&ev, cancel).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed.as_secs() < 5,
            "hook cancel path took {elapsed:?}; descendant kill did not happen in time"
        );

        tokio::time::sleep(Duration::from_millis(200)).await;

        let pgrep = tokio::process::Command::new("pgrep")
            .arg("-f")
            .arg(marker)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = pgrep {
            let mut buf = String::new();
            if let Some(mut s) = child.stdout.take() {
                let _ = tokio::io::AsyncReadExt::read_to_string(&mut s, &mut buf).await;
            }
            let _ = child.wait().await;
            assert!(
                buf.trim().is_empty(),
                "found surviving descendant matching {marker:?} after hook cancel: pids={buf}"
            );
        }
    }
}
