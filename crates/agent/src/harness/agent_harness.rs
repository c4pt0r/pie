//! `AgentHarness` — opinionated assembly around the bare `Agent`. 1:1 port of
//! `packages/agent/src/harness/agent-harness.ts` (~995 lines).
//!
//! Implemented:
//! - Compose `Agent` + `Session` + skills catalog + compaction settings
//! - `prompt(text)` / `prompt_with_images` / `continue_()`
//! - Auto-compaction trigger before each LLM call (when `compaction.enabled` is true)
//! - `set_model` / `set_thinking_level` mirror state mutations onto the session log
//! - `fork()` / `move_to()` branch operations (with optional branch summary)
//! - `prompt_from_template(name, vars)` — picks a `PromptTemplate`, interpolates, prompts
//! - `replace_tools` / `replace_skills` runtime mutations
//! - `enqueue_steering` / `enqueue_follow_up` queue passthrough
//! - `subscribe` to lifecycle events

use std::sync::Arc;

use parking_lot::Mutex;
use pie_ai::{ImageContent, Message as PiMessage, Model};

use super::super::agent::{Agent, AgentListener, AgentOptions, AgentRunError};
use super::super::types::*;
// AfterToolCallHook is re-exported under types::* via `pub use` in the module; if it isn't
// directly visible here, fall back to the absolute path inside Agent::new.
#[allow(unused_imports)]
use crate::types::AfterToolCallHook;

/// Harness-level lifecycle events. These are emitted in addition to the per-turn `AgentEvent`s
/// the inner `Agent` already publishes — they cover the cross-turn lifecycle decisions the
/// harness is responsible for (compaction, branching, session boundaries).
///
/// Subscribers run synchronously in delivery order on the calling tokio task. Panicking
/// subscribers are isolated via `catch_unwind` so one bad observer cannot break the harness;
/// the offending listener is dropped from the registry.
#[derive(Clone, Debug)]
pub enum HarnessEvent {
    /// First call to `prompt`/`continue_`/`prompt_from_template` after `AgentHarness::new`
    /// fires this once. `messages_replayed` reflects how many session messages were already on
    /// the active branch (e.g. a `--resume` start vs a fresh session).
    SessionStart { messages_replayed: usize },
    /// Auto- or manual compaction ran. `from_hook = true` currently means it came from
    /// `force_compact` (the CLI `/compact` path); `false` means the internal threshold check
    /// triggered it before a prompt.
    Compaction {
        from_hook: bool,
        summary: String,
        tokens_before: u64,
    },
    /// A branch operation (`move_to` / `fork`) landed. `from_entry_id` is `None` for moves to
    /// the root; `to_entry_id` is the new active leaf id (or `None` for root).
    Branch {
        from_entry_id: Option<String>,
        to_entry_id: Option<String>,
        summary_entry_id: Option<String>,
    },
    /// The harness has admitted a [`Trigger`] for processing — fires immediately at the
    /// start of [`AgentHarness::handle_trigger`] before evaluation. Carries the source
    /// identification needed to render a "processing X" banner. RFC 1 §2.7.
    TriggerHandlingStart {
        idempotency_key: String,
        source_kind: super::trigger::SourceKind,
        source_label: String,
        event_label: String,
        trace_id: String,
    },
    /// Terminal: the trigger reached an end state. `state` is one of the terminal variants
    /// (`Accepted` / `Deduped` / `CycleSuppressed` / etc. — `Accepted` is terminal for this
    /// sub-PR slice, the running/completed transition lands with the agent-loop wiring in
    /// sub-PR 3). `audit_entry_id` is the `SessionTreeEntry::Custom` id when persistence
    /// succeeded, `None` if persistence failed (a parallel `PersistenceError` event will
    /// describe the failure).
    TriggerHandled {
        idempotency_key: String,
        trace_id: String,
        state: super::trigger::TriggerState,
        audit_entry_id: Option<String>,
    },
    /// Best-effort persistence error reflux. Currently fires only when the trigger audit
    /// `Custom` entry write failed in `handle_trigger`. The trigger itself still produced
    /// a `TriggerHandled` event with `audit_entry_id = None`; this event explains why so
    /// that observability (TUI banner, `/triggers`, JSONL logs) can mark the audit as
    /// best-effort lost rather than dropping it silently.
    PersistenceError {
        /// Free-form context — currently always `"trigger_audit"`. New write sites that
        /// surface through this event must pin themselves to a stable string.
        context: String,
        /// Short, secret-free message. The original `SessionError` is *not* exposed because
        /// some implementations include filesystem paths or storage backend details that
        /// belong in trace logs, not user-facing event surfaces.
        message: String,
    },
}

/// Listener for [`HarnessEvent`]. Shape mirrors `crate::agent::AgentListener` so the same Fn
/// helpers translate.
pub type HarnessListener = Arc<dyn Fn(HarnessEvent) + Send + Sync>;

use super::compaction::compaction::{
    CompactionSettings, DEFAULT_COMPACTION_SETTINGS, SummarizeError, compact,
    estimate_context_tokens, should_compact,
};
use super::cost::{CostSnapshot, CostTracker};
use super::messages::compaction_summary;
use super::notification_hook::NotificationHookStatus;
use super::prompt_templates::PromptTemplateRegistry;
use super::session::session::{BranchSummaryInput, Session};
use super::skills::format_skill_invocation;
use super::system_prompt::format_skills_for_system_prompt;
use super::trigger::{Trigger, TriggerRecord, TriggerState};
use super::trigger_runtime::{
    EvaluationOutcome, TriggerRuntime, TriggerRuntimeConfig, TriggerRuntimeSnapshot,
};
use super::types::{PromptTemplate, Skill};

/// Aggregated, copy-friendly snapshot returned by
/// [`AgentHarness::notification_status_snapshot`]. The TUI / `/triggers hooks` command
/// renders this directly; `hooks` is intentionally a snapshot `Vec` (not a live view) so the
/// caller cannot pin the hook registry against new registrations.
///
/// Hook registry is currently empty in v1 of this PR — `register_notification_hook` lands
/// alongside the hook supervisor in a follow-up. The shape is here so CLI/TUI can render
/// `notifications: local mode (no hooks) — 0 waiting` against the real API today and not
/// have to refactor when hooks become non-empty.
#[derive(Clone, Debug)]
pub struct NotificationStatusSnapshot {
    pub hooks: Vec<NotificationHookStatus>,
    pub runtime: TriggerRuntimeSnapshot,
}

pub struct AgentHarnessOptions {
    /// Base system prompt prepended to the rendered skill catalog.
    pub system_prompt: String,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub skills: Vec<Skill>,
    pub prompt_templates: Vec<PromptTemplate>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub session: Session,
    pub stream_fn: Option<StreamFn>,
    /// Auto-compaction thresholds. Defaults to [`DEFAULT_COMPACTION_SETTINGS`].
    pub compaction: CompactionSettings,
    /// Optional `before_tool_call` hook. Wire a `PermissionPolicy::as_before_tool_call()` here
    /// to apply danger-detection to tool calls before the loop runs them.
    pub before_tool_call: Option<BeforeToolCallHook>,
    /// Optional `after_tool_call` hook. Used by the LSP supervisor (issue #12) to attach
    /// diagnostics to write/edit tool results.
    pub after_tool_call: Option<AfterToolCallHook>,
    /// Per-session USD cap. When set, the harness refuses to start a new prompt once the
    /// running cost exceeds the cap. `None` disables the check.
    pub budget_cap_usd: Option<f64>,
    /// Optional trigger runtime config override. Defaults to
    /// [`TriggerRuntimeConfig::default`] (5-minute dedup, 5-hop cycle limit).
    pub trigger_runtime: TriggerRuntimeConfig,
}

impl AgentHarnessOptions {
    pub fn new(model: Model, session: Session) -> Self {
        Self {
            system_prompt: String::new(),
            model,
            thinking_level: ThinkingLevel::Off,
            skills: Vec::new(),
            prompt_templates: Vec::new(),
            tools: Vec::new(),
            session,
            stream_fn: None,
            compaction: DEFAULT_COMPACTION_SETTINGS.clone(),
            before_tool_call: None,
            after_tool_call: None,
            budget_cap_usd: None,
            trigger_runtime: TriggerRuntimeConfig::default(),
        }
    }
}

pub struct AgentHarness {
    agent: Arc<Agent>,
    session: Session,
    skills: Mutex<Vec<Skill>>,
    base_system_prompt: String,
    templates: Mutex<PromptTemplateRegistry>,
    compaction_settings: Mutex<CompactionSettings>,
    /// Used by auto-compaction to call the LLM for summarization.
    stream_fn: Option<StreamFn>,
    /// Harness-level lifecycle listeners. Separate from `Agent::listeners` — those cover
    /// per-turn events; this covers cross-turn / session-level decisions. Held behind an
    /// `Arc` so an unsubscriber closure can drop its captured handle independently of the
    /// `AgentHarness` lifetime.
    harness_listeners: Arc<Mutex<Vec<HarnessListener>>>,
    session_start_emitted: Mutex<bool>,
    /// Running token / cost totals for this harness lifetime. Updated automatically by an
    /// internal listener subscribed to `Agent::MessageEnd`. Snapshot via [`Self::cost`].
    cost: CostTracker,
    budget_cap_usd: Option<f64>,
    /// In-memory dedup + cycle evaluator shared with [`Self::handle_trigger`]. Exposed via
    /// [`Self::notification_status_snapshot`] for observability.
    trigger_runtime: TriggerRuntime,
}

impl AgentHarness {
    pub fn new(options: AgentHarnessOptions) -> Self {
        let mut state = AgentState::default();
        state.model = Some(options.model);
        state.thinking_level = Some(options.thinking_level);
        state.tools = options.tools;
        state.system_prompt = build_system_prompt(&options.system_prompt, &options.skills);

        let agent = Agent::new(AgentOptions {
            initial_state: Some(state),
            stream_fn: options.stream_fn.clone(),
            before_tool_call: options.before_tool_call.clone(),
            after_tool_call: options.after_tool_call.clone(),
            ..Default::default()
        });

        let cost = CostTracker::new();
        // Subscribe the cost tracker to assistant MessageEnd events. Listener is wired against
        // the inner Agent so the harness has no per-prompt setup cost.
        let _ = agent.subscribe(cost.as_listener());

        Self {
            agent: Arc::new(agent),
            session: options.session,
            skills: Mutex::new(options.skills),
            base_system_prompt: options.system_prompt,
            templates: Mutex::new(PromptTemplateRegistry::new(options.prompt_templates)),
            compaction_settings: Mutex::new(options.compaction),
            stream_fn: options.stream_fn,
            harness_listeners: Arc::new(Mutex::new(Vec::new())),
            session_start_emitted: Mutex::new(false),
            cost,
            budget_cap_usd: options.budget_cap_usd,
            trigger_runtime: TriggerRuntime::with_config(options.trigger_runtime),
        }
    }

    /// Snapshot of running token + cost totals.
    pub fn cost(&self) -> CostSnapshot {
        self.cost.snapshot()
    }

    /// Reset the cost tracker — `/cost reset` and on session-switch.
    pub fn reset_cost(&self) {
        self.cost.reset();
    }

    /// Register a harness-level lifecycle listener. Returns an unsubscriber closure.
    ///
    /// Listener panics are caught — see [`HarnessEvent`] for the isolation contract. The
    /// returned closure removes the listener; calling it twice is a no-op.
    pub fn subscribe_harness(&self, listener: HarnessListener) -> Box<dyn FnOnce() + Send> {
        self.harness_listeners.lock().push(listener.clone());
        // Identity-match the listener for removal. Capture the data-pointer address as a
        // `usize` (Send) so the unsubscriber doesn't carry a raw pointer across threads.
        let target = Arc::as_ptr(&listener) as *const () as usize;
        let listeners = Arc::clone(&self.harness_listeners);
        Box::new(move || {
            let mut g = listeners.lock();
            if let Some(i) = g
                .iter()
                .position(|l| (Arc::as_ptr(l) as *const () as usize) == target)
            {
                g.remove(i);
            }
        })
    }

    fn emit_harness_event(&self, event: HarnessEvent) {
        let listeners = self.harness_listeners.lock().clone();
        for l in listeners {
            // Each listener runs isolated so one panic doesn't poison the rest.
            let l = l.clone();
            let ev = event.clone();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || l(ev)));
        }
    }

    fn ensure_session_start_emitted(&self) {
        let mut g = self.session_start_emitted.lock();
        if *g {
            return;
        }
        *g = true;
        let count = self.agent.state().messages.len();
        drop(g);
        self.emit_harness_event(HarnessEvent::SessionStart {
            messages_replayed: count,
        });
    }

    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// Accept an incoming [`Trigger`] from a notification adapter. Evaluates it against the
    /// runtime's dedup + cycle bookkeeping, persists a
    /// `SessionTreeEntry::Custom { custom_type: "trigger" }` audit entry summarizing the
    /// decision, and emits [`HarnessEvent::TriggerHandlingStart`] / [`HarnessEvent::TriggerHandled`].
    ///
    /// Returns the [`EvaluationOutcome`] so adapters that synchronously dispatched the
    /// trigger know whether downstream rule evaluation should proceed. In this PR `Accept`
    /// is terminal — actually invoking the agent loop on an accepted trigger lands with the
    /// permission evaluator extension and the running-state machine in sub-PR 3.
    ///
    /// Persistence is best-effort: if the audit write fails, this method still returns the
    /// evaluator outcome and emits a [`HarnessEvent::PersistenceError`] alongside the
    /// `TriggerHandled` event (with `audit_entry_id = None`). The trigger evaluation is
    /// authoritative; the audit record is observability.
    pub async fn handle_trigger(&self, trigger: Trigger) -> EvaluationOutcome {
        self.emit_harness_event(HarnessEvent::TriggerHandlingStart {
            idempotency_key: trigger.idempotency_key.clone(),
            source_kind: trigger.source_kind,
            source_label: trigger.source_label.clone(),
            event_label: trigger.event_label.clone(),
            trace_id: trigger.trace_id.clone(),
        });

        let outcome = self.trigger_runtime.evaluate(&trigger);

        let (state, evaluator_decision) = match &outcome {
            EvaluationOutcome::Accept => (
                TriggerState::Accepted,
                Some(serde_json::json!({ "outcome": "accept" })),
            ),
            EvaluationOutcome::Deduped {
                replacement_policy,
                previous_trace_id,
            } => (
                TriggerState::Deduped,
                Some(serde_json::json!({
                    "outcome": "deduped",
                    "replacement_policy": replacement_policy,
                    "previous_trace_id": previous_trace_id,
                })),
            ),
            EvaluationOutcome::CycleSuppressed { hop_count } => (
                TriggerState::CycleSuppressed,
                Some(serde_json::json!({
                    "outcome": "cycle_suppressed",
                    "hop_count": hop_count,
                })),
            ),
        };

        let mut record = TriggerRecord::received_from(&trigger);
        record.state = state;
        record.evaluator_decision = evaluator_decision;

        let audit_payload = match serde_json::to_value(&record) {
            Ok(v) => Some(v),
            Err(e) => {
                // Audit serialization failure is a programming error (the type derives
                // Serialize over wholly-owned fields), but we don't want to panic on it
                // from a user-driven path. Surface as PersistenceError and proceed.
                self.emit_harness_event(HarnessEvent::PersistenceError {
                    context: "trigger_audit".into(),
                    message: format!("trigger record serialization failed: {e}"),
                });
                None
            }
        };

        let audit_entry_id = match audit_payload {
            Some(payload) => match self
                .session
                .append_custom(TriggerRecord::CUSTOM_TYPE, Some(payload))
                .await
            {
                Ok(id) => Some(id),
                Err(e) => {
                    self.emit_harness_event(HarnessEvent::PersistenceError {
                        context: "trigger_audit".into(),
                        message: format!("trigger audit append failed: {:?}", e.code),
                    });
                    None
                }
            },
            None => None,
        };

        self.emit_harness_event(HarnessEvent::TriggerHandled {
            idempotency_key: trigger.idempotency_key,
            trace_id: trigger.trace_id,
            state,
            audit_entry_id,
        });

        outcome
    }

    /// Point-in-time view of the harness's notification surface — currently the
    /// [`TriggerRuntimeSnapshot`] plus an empty hook list. Hook registration + supervisor
    /// land in a follow-up PR; the shape is here so CLI/TUI can render banners against the
    /// real API today and not have to refactor when the registry becomes non-empty.
    pub fn notification_status_snapshot(&self) -> NotificationStatusSnapshot {
        NotificationStatusSnapshot {
            hooks: Vec::new(),
            runtime: self.trigger_runtime.snapshot(),
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn skills(&self) -> Vec<Skill> {
        self.skills.lock().clone()
    }

    /// Snapshot of the loaded prompt templates. Listing-only — callers run them via
    /// [`Self::prompt_from_template`].
    pub fn templates(&self) -> Vec<PromptTemplate> {
        self.templates.lock().list().to_vec()
    }

    pub fn system_prompt(&self) -> String {
        self.agent.state().system_prompt.clone()
    }

    /// Replace the skill catalog. Rebuilds the system prompt so the in-flight Agent state has
    /// the new `<skills>` block on its next LLM call.
    pub fn replace_skills(&self, skills: Vec<Skill>) {
        *self.skills.lock() = skills;
        let prompt = build_system_prompt(&self.base_system_prompt, &self.skills.lock());
        self.agent.state().system_prompt = prompt;
    }

    /// Replace the prompt-template registry.
    pub fn replace_prompt_templates(&self, templates: Vec<PromptTemplate>) {
        *self.templates.lock() = PromptTemplateRegistry::new(templates);
    }

    /// Replace the tool set. UI consumers calling this mid-run will see the new tools on the
    /// next turn.
    pub fn replace_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
        self.agent.state().tools = tools;
    }

    /// Update auto-compaction thresholds.
    pub fn set_compaction_settings(&self, settings: CompactionSettings) {
        *self.compaction_settings.lock() = settings;
    }

    pub fn abort(&self) {
        self.agent.abort();
    }

    pub fn enqueue_steering(&self, message: AgentMessage) {
        self.agent.enqueue_steering(message);
    }

    pub fn enqueue_follow_up(&self, message: AgentMessage) {
        self.agent.enqueue_follow_up(message);
    }

    pub fn subscribe(&self, listener: AgentListener) -> impl FnOnce() {
        self.agent.subscribe(listener)
    }

    /// Switch model. Persists a `ModelChange` session entry so resume sees the right one.
    pub async fn set_model(&self, model: Model) -> Result<String, super::types::SessionError> {
        let provider = model.provider.0.clone();
        let model_id = model.id.clone();
        let id = self.session.append_model_change(provider, model_id).await?;
        self.agent.state().model = Some(model);
        Ok(id)
    }

    pub async fn set_thinking_level(
        &self,
        level: ThinkingLevel,
    ) -> Result<String, super::types::SessionError> {
        let id = self
            .session
            .append_thinking_level_change(level.as_str())
            .await?;
        self.agent.state().thinking_level = Some(level);
        Ok(id)
    }

    /// Move the session leaf to a specific entry id (or root). When `summary` is provided,
    /// records a branch_summary entry so siblings see the fork's contribution. Replays the new
    /// branch into agent state via [`Self::rehydrate_from_session`].
    pub async fn move_to(
        &self,
        entry_id: Option<&str>,
        summary: Option<BranchSummaryInput>,
    ) -> Result<Option<String>, super::types::SessionError> {
        let from = self.session.leaf_id().await.ok().flatten();
        let result = self.session.move_to(entry_id, summary).await?;
        self.rehydrate_from_session().await?;
        self.emit_harness_event(HarnessEvent::Branch {
            from_entry_id: from,
            to_entry_id: entry_id.map(|s| s.to_string()),
            summary_entry_id: result.clone(),
        });
        Ok(result)
    }

    /// Replace the agent's in-memory state with the session's active branch. Messages, model,
    /// and thinking level are restored from `Session::build_context()`. Returns the rebuilt
    /// `SessionContext` for callers that want to render the transcript or inspect the recovered
    /// model.
    ///
    /// CLI startup (`--resume`) and post-branch-switch flows both go through this — keeps the
    /// "how do we rehydrate?" decision in one place.
    pub async fn rehydrate_from_session(
        &self,
    ) -> Result<super::session::session::SessionContext, super::types::SessionError> {
        let ctx = self.session.build_context().await?;
        let mut s = self.agent.state();
        s.messages = ctx.messages.clone();
        if let Some(model) = &ctx.model {
            // Restore the previously-active model when it's still in the catalog. Unknown
            // models keep whatever the caller set up — the resume banner reflects that fact.
            if let Some(m) = pie_ai::get_model(
                &pie_ai::Provider::from(model.provider.clone()),
                &model.model_id,
            ) {
                s.model = Some(m);
            }
        }
        if let Ok(level) = ctx.thinking_level.parse::<ThinkingLevel>() {
            s.thinking_level = Some(level);
        }
        Ok(ctx)
    }

    /// Pick a template by name, interpolate, and prompt the agent.
    pub async fn prompt_from_template(
        &self,
        name: &str,
        vars: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), AgentRunError> {
        let template = {
            let g = self.templates.lock();
            g.get(name).cloned()
        };
        let template = match template {
            Some(t) => t,
            None => {
                return Err(AgentRunError::Other(format!(
                    "unknown prompt template: {name}"
                )));
            }
        };
        let rendered = PromptTemplateRegistry::interpolate(&template, &vars);
        self.prompt(rendered).await
    }

    /// Prompt the agent with text. Runs auto-compaction first, persists results to session.
    pub async fn prompt(&self, text: impl Into<String>) -> Result<(), AgentRunError> {
        let text = text.into();
        let user_message = AgentMessage::Llm(PiMessage::User(pie_ai::UserMessage {
            role: pie_ai::UserRole::User,
            content: pie_ai::UserContent::Text(text),
            timestamp: chrono::Utc::now().timestamp_millis(),
        }));
        self.prompt_with_message(user_message).await
    }

    /// Prompt with text + images (multimodal users).
    pub async fn prompt_with_images(
        &self,
        text: impl Into<String>,
        images: Vec<ImageContent>,
    ) -> Result<(), AgentRunError> {
        let mut blocks: Vec<pie_ai::UserContentBlock> = images
            .into_iter()
            .map(pie_ai::UserContentBlock::Image)
            .collect();
        let text = text.into();
        if !text.is_empty() {
            blocks.insert(0, pie_ai::UserContentBlock::text(text));
        }
        let user_message = AgentMessage::Llm(PiMessage::User(pie_ai::UserMessage {
            role: pie_ai::UserRole::User,
            content: pie_ai::UserContent::Blocks(blocks),
            timestamp: chrono::Utc::now().timestamp_millis(),
        }));
        self.prompt_with_message(user_message).await
    }

    async fn prompt_with_message(&self, msg: AgentMessage) -> Result<(), AgentRunError> {
        self.ensure_session_start_emitted();
        if let Some(cap) = self.budget_cap_usd {
            let total = self.cost.snapshot().tokens.cost.total;
            if total >= cap {
                return Err(AgentRunError::Other(format!(
                    "budget cap reached: ${total:.4} >= ${cap:.4}. Reset with /cost reset or raise budget_cap_usd.",
                )));
            }
        }
        // Run compaction if we've crossed the threshold. This must happen before the user
        // message is appended so the cut point doesn't risk splitting the current turn.
        self.run_auto_compaction().await?;

        let (listener, persist_errors) = make_session_listener(self.session.clone());
        let unsub = self.agent.subscribe(listener);
        let result = self.agent.prompt(msg).await;
        unsub();
        finish_persisted_run(result, persist_errors)
    }

    pub async fn continue_(&self) -> Result<(), AgentRunError> {
        self.ensure_session_start_emitted();
        self.run_auto_compaction().await?;
        let (listener, persist_errors) = make_session_listener(self.session.clone());
        let unsub = self.agent.subscribe(listener);
        let result = self.agent.continue_().await;
        unsub();
        finish_persisted_run(result, persist_errors)
    }

    /// Force a compaction immediately, regardless of token thresholds. Useful for `/compact`-
    /// style slash commands.
    pub async fn force_compact(
        &self,
        custom_instructions: Option<String>,
    ) -> Result<bool, AgentRunError> {
        self.do_compact(true, custom_instructions).await
    }

    async fn run_auto_compaction(&self) -> Result<(), AgentRunError> {
        let settings = self.compaction_settings.lock().clone();
        if !settings.enabled {
            return Ok(());
        }
        let (context_tokens, context_window) = {
            let s = self.agent.state();
            let model = match &s.model {
                Some(m) => m,
                None => return Ok(()),
            };
            let estimate = estimate_context_tokens(&s.messages);
            (estimate.tokens, model.context_window)
        };
        if !should_compact(context_tokens, context_window, &settings) {
            return Ok(());
        }
        let _ = self.do_compact(false, None).await?;
        Ok(())
    }

    /// Shared implementation behind auto + manual compaction. Returns `true` when compaction
    /// actually ran.
    ///
    /// Operates on the real session entries (via `self.session.branch(None)`) so the
    /// `first_kept_entry_id` we persist on the `Compaction` record is reachable in the session
    /// jsonl. The previous implementation synthesized fake `Message` entries from in-memory
    /// `state.messages` with fresh uuidv7s — those ids were never written to the session, so
    /// `--resume` could not locate them in `build_session_context` and silently dropped all
    /// pre-compaction tail. See issue #19.
    async fn do_compact(
        &self,
        from_hook: bool,
        custom_instructions: Option<String>,
    ) -> Result<bool, AgentRunError> {
        let model = match self.agent.state().model.clone() {
            Some(m) => m,
            None => return Ok(false),
        };

        // Source of truth: real session entries with their real ids.
        let entries = match self.session.branch(None).await {
            Ok(es) => es,
            Err(e) => {
                // Read failure is non-fatal: skip this compaction attempt; the loop will try
                // again next time. We do not append a `Compaction` record and do not mutate
                // agent state.
                self.emit_harness_event(HarnessEvent::Compaction {
                    from_hook,
                    summary: format!("compaction skipped: session branch read failed: {e}"),
                    tokens_before: 0,
                });
                return Ok(false);
            }
        };

        let settings = self.compaction_settings.lock().clone();
        let result = compact(
            model,
            &entries,
            &settings,
            custom_instructions,
            self.stream_fn.clone(),
            self.agent.active_token().unwrap_or_default(),
        )
        .await;

        let result = match result {
            Ok(r) if !r.summary.is_empty() => r,
            Ok(_) => return Ok(false),
            Err(SummarizeError::Aborted) => return Ok(false),
            Err(e) => return Err(AgentRunError::Other(format!("compaction failed: {e}"))),
        };

        let first_kept_entry_id = result.first_kept_entry_id.clone().unwrap_or_default();

        // Persist a compaction entry to the session.
        let _ = self
            .session
            .append_compaction(
                result.summary.clone(),
                first_kept_entry_id.clone(),
                result.tokens_before,
                None,
                from_hook,
            )
            .await
            .map_err(|e| AgentRunError::Other(format!("session append compaction: {e}")))?;

        self.emit_harness_event(HarnessEvent::Compaction {
            from_hook,
            summary: result.summary.clone(),
            tokens_before: result.tokens_before,
        });

        // Replace agent state's prefix with a single compaction-summary message followed by
        // the in-memory tail that corresponds to the kept session entries.
        //
        // `state.messages` is the in-memory mirror of session `Message` entries (the agent loop
        // only appends `AgentMessage::Llm` variants there, and `make_session_listener`
        // persists each one). So the in-memory index for the first kept entry equals the
        // count of `Message` entries strictly before `first_kept_entry_id` in `entries`.
        // Non-Message entries (ModelChange, ThinkingLevelChange, Custom{custom_type=trigger},
        // BranchSummary, etc.) are not in `state.messages` and are skipped naturally.
        {
            let mut s = self.agent.state();
            let mut new_msgs: Vec<AgentMessage> = vec![compaction_summary(result.summary.clone())];

            if !first_kept_entry_id.is_empty() {
                if let Some(real_idx) = entries.iter().position(|e| e.id() == first_kept_entry_id) {
                    let kept_in_memory_start = entries[..real_idx]
                        .iter()
                        .filter(|e| {
                            matches!(e, super::session::session::SessionTreeEntry::Message { .. })
                        })
                        .count();
                    if kept_in_memory_start <= s.messages.len() {
                        new_msgs.extend(s.messages[kept_in_memory_start..].iter().cloned());
                    }
                    // If `kept_in_memory_start` is out of range, the in-memory state has
                    // diverged from the session (race or external mutation). We keep just the
                    // summary; the next prompt rehydrates the rest if needed.
                }
                // If `first_kept_entry_id` is non-empty but not found in `entries`, treat as a
                // legacy (pre-fix) bad record: keep just the summary, do not crash. Documented
                // in CHANGELOG `### Fixed`.
            }
            // Empty `first_kept_entry_id` means `entries` was empty pre-compaction — only the
            // summary is needed.

            s.messages = new_msgs;
        }
        Ok(true)
    }

    /// Format a single skill invocation block for ad-hoc UI surfaces.
    pub fn format_skill(skill: &Skill, extra: Option<&str>) -> String {
        format_skill_invocation(skill, extra)
    }
}

fn build_system_prompt(base: &str, skills: &[Skill]) -> String {
    let skills_block = format_skills_for_system_prompt(skills);
    if base.is_empty() {
        return skills_block;
    }
    if skills_block.is_empty() {
        return base.to_string();
    }
    format!("{base}\n\n{skills_block}")
}

/// Build an `AgentListener` that persists every emitted `MessageEnd` to the session log.
fn make_session_listener(
    session: Session,
) -> (
    crate::agent::AgentListener,
    Arc<Mutex<Vec<super::types::SessionError>>>,
) {
    let errors = Arc::new(Mutex::new(Vec::new()));
    let listener_errors = errors.clone();
    let listener: crate::agent::AgentListener = Arc::new(move |event, _cancel| {
        let session = session.clone();
        let listener_errors = listener_errors.clone();
        Box::pin(async move {
            if let AgentEvent::MessageEnd { message } = event {
                if let Err(e) = session.append_message(message).await {
                    listener_errors.lock().push(e);
                }
            }
        })
    });
    (listener, errors)
}

fn finish_persisted_run(
    result: Result<(), AgentRunError>,
    persist_errors: Arc<Mutex<Vec<super::types::SessionError>>>,
) -> Result<(), AgentRunError> {
    result?;
    if let Some(e) = persist_errors.lock().first() {
        return Err(AgentRunError::Other(format!("session append message: {e}")));
    }
    Ok(())
}
