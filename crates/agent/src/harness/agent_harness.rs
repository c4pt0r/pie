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
    /// (`Accepted` / `Deduped` / `CycleSuppressed` / `PermissionDenied` / `NeedsApproval`
    /// — `Accepted` is terminal for this sub-PR slice; the `Running`/`Completed`/`Failed`
    /// transitions land with the agent-loop wiring in a follow-up).
    ///
    /// `audit_entry_id` is the `SessionTreeEntry::Custom` id when persistence succeeded,
    /// `None` if persistence failed (a parallel `PersistenceError` event will describe
    /// the failure).
    ///
    /// `evaluator_decision` mirrors what was persisted in the audit record (same JSON
    /// shape) so live subscribers (TUI banner, `/triggers`, JSONL logs) can render *why*
    /// the trigger reached its state without a secondary session lookup. Shape:
    /// - Accept (Allow): `{ "outcome": "accept", "permission": "allow" }`
    /// - Accept (Deny):  `{ "outcome": "accept", "permission": "deny",   "reason": ... }`
    /// - Accept (Prompt):`{ "outcome": "accept", "permission": "prompt", "reason": ... }`
    /// - Deduped:        `{ "outcome": "deduped", "replacement_policy": ..., "previous_trace_id": ... }`
    /// - CycleSuppressed:`{ "outcome": "cycle_suppressed", "hop_count": N }`
    ///
    /// `None` only when audit serialization failed (a `PersistenceError` will accompany).
    TriggerHandled {
        idempotency_key: String,
        trace_id: String,
        state: super::trigger::TriggerState,
        audit_entry_id: Option<String>,
        evaluator_decision: Option<serde_json::Value>,
    },
    /// Best-effort persistence error reflux. Currently fires only when the trigger audit
    /// `Custom` entry write failed in `handle_trigger`. The trigger itself still produced
    /// a `TriggerHandled` event with `audit_entry_id = None`; this event explains why so
    /// that observability (TUI banner, `/triggers`, JSONL logs) can mark the audit as
    /// best-effort lost rather than dropping it silently.
    PersistenceError {
        /// Free-form context — pinned strings: `"trigger_audit"`, `"trigger_result"`. New
        /// write sites that surface through this event must pin themselves to a stable
        /// string.
        context: String,
        /// Short, secret-free message. The original `SessionError` is *not* exposed because
        /// some implementations include filesystem paths or storage backend details that
        /// belong in trace logs, not user-facing event surfaces.
        message: String,
    },
    /// A sub-agent execution started for an accepted trigger. Emitted by the spawned task
    /// just before the sub-agent's first turn runs. `prompt_preview` is the first ~80
    /// characters of the resolved action prompt, preview-safe for banners.
    ///
    /// Causality (pinned by RFC 1 §5.F + tests): `TriggerHandled { state: Accepted }`
    /// always precedes `TriggerExecutionStarted` for the same `trace_id`.
    TriggerExecutionStarted {
        trace_id: String,
        prompt_preview: String,
    },
    /// A sub-agent execution finished successfully and the parent `trigger_result` audit
    /// entry has been written. `summary` is the sub-agent's self-summary (size-capped at
    /// 4 KiB). `cost_usd` is `None` in sub-PR 5a because the bare sub-`Agent` has no
    /// `CostTracker` wrapper — the value mirrors the audit's `cost_usd: null`. Sub-PR 5b
    /// or 5c wraps the sub-agent in a mini-`CostTracker` and `cost_usd` will be `Some(f)`.
    TriggerCompleted {
        trace_id: String,
        summary: Option<String>,
        cost_usd: Option<f64>,
    },
    /// A sub-agent execution failed (agent loop error, panic-via-spawn-error, or aborted by
    /// [`AgentHarness::abort_trigger`] / [`AgentHarness::abort_all_triggers`]). `reason` is
    /// sanitized — never contains raw payload, provider response bodies, or credential
    /// material. The parent `trigger_result` audit entry has been written with
    /// `success: false`.
    TriggerFailed { trace_id: String, reason: String },
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
use super::notification_hook::{DynNotificationHook, NotificationHookStatus};
use super::prompt_templates::PromptTemplateRegistry;
use super::session::session::{BranchSummaryInput, Session};
use super::skills::format_skill_invocation;
use super::system_prompt::format_skills_for_system_prompt;
use super::trigger::{Trigger, TriggerRecord, TriggerState};
use super::trigger_runtime::{
    EvaluationOutcome, TriggerRuntime, TriggerRuntimeConfig, TriggerRuntimeSnapshot,
};
use super::types::{PromptTemplate, Skill};

/// Decision returned from [`BeforeTriggerHook`]. Maps directly to terminal
/// [`TriggerState`] variants when [`AgentHarness::handle_trigger`] resolves the trigger.
///
/// - `Allow` keeps the trigger on the `Accepted` path (default if no hook is configured).
/// - `Deny { reason }` is a hard refusal; the trigger is recorded as `PermissionDenied`
///   and the reason is captured in the audit record's `evaluator_decision`.
/// - `Prompt { reason }` is a soft refusal; the trigger is recorded as `NeedsApproval`,
///   and a future UI surface can offer the user replay. Today this is functionally a
///   block — sub-PR 5 (running state machine) is where the prompt UI is wired in.
///
/// Token material **never** belongs in `reason`. Reasons surface in the audit
/// record's `evaluator_decision` and in [`HarnessEvent::TriggerHandled`].
#[derive(Clone, Debug, Default)]
pub enum BeforeTriggerDecision {
    #[default]
    Allow,
    Deny {
        reason: String,
    },
    Prompt {
        reason: String,
    },
}

/// Snapshot passed into [`BeforeTriggerHook`]. Owned so the hook future can be `'static`.
/// The hook sees the full trigger (including authority + payload summary) plus a
/// point-in-time runtime snapshot so policy can reason about burst rates ("more than 10
/// triggers from this source in the last window → require approval").
#[derive(Clone, Debug)]
pub struct BeforeTriggerContext {
    pub trigger: super::trigger::Trigger,
    pub runtime: super::trigger_runtime::TriggerRuntimeSnapshot,
}

/// Hook called by [`AgentHarness::handle_trigger`] after dedup + cycle evaluation
/// returned `Accept`, but before the audit record is persisted. The hook returns a
/// [`BeforeTriggerDecision`] mapping to a terminal [`TriggerState`]. If no hook is
/// configured, the harness behaves as if the hook returned [`BeforeTriggerDecision::Allow`].
///
/// The hook runs after evaluator Accept on purpose: dedup / cycle decisions are
/// pure-runtime concerns (no policy involvement); permission is a policy concern that
/// applies only to triggers the runtime would otherwise process.
pub type BeforeTriggerHook = Arc<
    dyn Fn(
            BeforeTriggerContext,
            tokio_util::sync::CancellationToken,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = BeforeTriggerDecision> + Send>>
        + Send
        + Sync,
>;

/// Aggregated, copy-friendly snapshot returned by
/// [`AgentHarness::notification_status_snapshot`]. The TUI / `/triggers hooks` command
/// renders this directly; `hooks` and `running` are snapshots, not live views, so the caller
/// cannot pin the underlying registries against concurrent registrations / completions.
///
/// `hooks` is filled from `hook.status()` of every hook registered via
/// [`AgentHarness::register_notification_hook`]. Unregistered / hook-ended cases stay in the
/// snapshot until the next registration cycle; consumers should treat `NotificationHookStatus.state`
/// as the source of truth for whether a hook is currently usable.
///
/// `running` is the set of accepted triggers whose sub-agent execution has started and not
/// yet finished. Each entry holds bounded preview-safe fields only (no raw payload, no
/// template vars, no credentials). RFC 1 §5.G acceptance pins this.
#[derive(Clone, Debug)]
pub struct NotificationStatusSnapshot {
    pub hooks: Vec<NotificationHookStatus>,
    pub runtime: TriggerRuntimeSnapshot,
    pub running: Vec<RunningTriggerState>,
}

/// Bounded preview-safe view of a single in-flight trigger action. Fields are intentionally
/// minimal so the TUI banner / `/triggers` view cannot accidentally leak raw payload or
/// credential material. RFC 1 §5.G.
#[derive(Clone, Debug)]
pub struct RunningTriggerState {
    pub trace_id: String,
    pub source_label: String,
    pub event_label: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// First ~80 chars of the resolved action prompt.
    pub prompt_preview: String,
}

/// Action the harness should take on an accepted trigger. Returned by
/// [`BeforeTriggerActionHook`]; default (no hook) maps every trigger to
/// `TriggerAction { prompt: format!("{source_label} fired: {event_label}"),
/// promote: PromoteAction::None, promote_requires_approval: false }`.
///
/// `promote` and `promote_requires_approval` are accepted in this sub-PR but only the
/// `PromoteAction::None` variant has an effect; the promotion pipeline lands in sub-PR 5b
/// per the issue #20 amendment. Fields are reserved here so adapters can be written against
/// the final shape today.
#[derive(Clone, Debug)]
pub struct TriggerAction {
    pub prompt: String,
    pub promote: PromoteAction,
    pub promote_requires_approval: bool,
}

impl TriggerAction {
    /// The default `Prompt` form used when no [`BeforeTriggerActionHook`] is configured.
    /// `format!("{source_label} fired: {event_label}")` is the RFC 1 §5.C stable fallback —
    /// always non-empty and carries enough context that the sub-agent can react.
    pub fn default_for(trigger: &Trigger) -> Self {
        Self {
            prompt: format!("{} fired: {}", trigger.source_label, trigger.event_label),
            promote: PromoteAction::None,
            promote_requires_approval: false,
        }
    }
}

/// How a completed sub-agent's `trigger_result` should affect the parent session. Lands as a
/// no-op variant in sub-PR 5a; sub-PR 5b will honor `PromoteSummaryNow` (and follow-ups will
/// add `InjectNextTurn` per the issue #20 amendment).
#[derive(Clone, Debug, Default)]
pub enum PromoteAction {
    #[default]
    None,
    PromoteSummaryNow {
        /// Named [`PromptTemplate`]; `None` uses the runtime's built-in safe default.
        template: Option<String>,
    },
}

/// Snapshot context passed into [`BeforeTriggerActionHook`]. Hook returns the
/// [`TriggerAction`] for the accepted trigger.
#[derive(Clone, Debug)]
pub struct BeforeTriggerActionContext {
    pub trigger: super::trigger::Trigger,
    pub runtime: super::trigger_runtime::TriggerRuntimeSnapshot,
}

/// Hook called by [`AgentHarness::handle_trigger`] *after* the optional
/// [`BeforeTriggerHook`] returned `Allow`, to decide the action the sub-agent should run.
/// `None` falls back to [`TriggerAction::default_for`].
pub type BeforeTriggerActionHook = Arc<
    dyn Fn(
            BeforeTriggerActionContext,
            tokio_util::sync::CancellationToken,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TriggerAction> + Send>>
        + Send
        + Sync,
>;

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
    /// Optional permission hook applied to triggers admitted by the dedup + cycle evaluator.
    /// `None` is equivalent to a hook that always returns
    /// [`BeforeTriggerDecision::Allow`]. See [`BeforeTriggerHook`].
    pub before_trigger: Option<BeforeTriggerHook>,
    /// Optional action hook resolving accepted triggers to a [`TriggerAction`]. `None`
    /// falls back to [`TriggerAction::default_for`] (the stable `format!("{source_label}
    /// fired: {event_label}")` mapping).
    pub before_trigger_action: Option<BeforeTriggerActionHook>,
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
            before_trigger: None,
            before_trigger_action: None,
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
    /// Notification hooks registered via [`Self::register_notification_hook`]. Held under
    /// an `Arc<Mutex<...>>` so [`Self::notification_status_snapshot`] can read and the
    /// supervisor task can append independently of harness ownership. The hook driver +
    /// pump tasks are detached (`tokio::spawn`); they tear down naturally when the hook's
    /// `run` future completes or returns an error.
    notification_hooks: Arc<Mutex<Vec<DynNotificationHook>>>,
    /// Optional permission hook applied to accepted triggers before they advance to a
    /// terminal state. `None` defaults to [`BeforeTriggerDecision::Allow`].
    before_trigger: Option<BeforeTriggerHook>,
    /// Optional action hook resolving accepted triggers to a `TriggerAction`. `None` falls
    /// back to [`TriggerAction::default_for`].
    before_trigger_action: Option<BeforeTriggerActionHook>,
    /// Retained `before_tool_call` hook for cloning into sub-agent harnesses spawned by
    /// `spawn_trigger_action`. Mirrors the same hook handed to the inner `Agent`.
    before_tool_call: Option<BeforeToolCallHook>,
    /// Retained `after_tool_call` hook for the same purpose.
    after_tool_call: Option<AfterToolCallHook>,
    /// In-flight sub-agent executions keyed by `trace_id`. Each entry holds the cancel
    /// token (so [`Self::abort_trigger`] / [`Self::abort_all_triggers`] can interrupt the
    /// sub-agent) plus the preview-safe state surfaced by
    /// [`Self::notification_status_snapshot`]. Entries are inserted just before
    /// `TriggerExecutionStarted` and removed after the terminal `Completed`/`Failed`
    /// event so snapshots reflect what's really running.
    running_triggers: Arc<Mutex<std::collections::HashMap<String, RunningTriggerHandle>>>,
}

/// Internal record kept under `AgentHarness::running_triggers`. The public-facing snapshot
/// type is [`RunningTriggerState`]; this struct adds the cancel token used by the abort APIs.
struct RunningTriggerHandle {
    state: RunningTriggerState,
    cancel: tokio_util::sync::CancellationToken,
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
            notification_hooks: Arc::new(Mutex::new(Vec::new())),
            before_trigger: options.before_trigger,
            before_trigger_action: options.before_trigger_action,
            before_tool_call: options.before_tool_call,
            after_tool_call: options.after_tool_call,
            running_triggers: Arc::new(Mutex::new(std::collections::HashMap::new())),
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
            EvaluationOutcome::Accept => {
                // Evaluator said admit; run the permission hook to decide whether the
                // accepted trigger advances to `Accepted` or stops at one of the
                // policy-terminal states (`PermissionDenied` / `NeedsApproval`).
                let permission_decision = self.run_before_trigger_hook(&trigger).await;
                match permission_decision {
                    BeforeTriggerDecision::Allow => (
                        TriggerState::Accepted,
                        Some(serde_json::json!({
                            "outcome": "accept",
                            "permission": "allow"
                        })),
                    ),
                    BeforeTriggerDecision::Deny { reason } => (
                        TriggerState::PermissionDenied,
                        Some(serde_json::json!({
                            "outcome": "accept",
                            "permission": "deny",
                            "reason": reason,
                        })),
                    ),
                    BeforeTriggerDecision::Prompt { reason } => (
                        TriggerState::NeedsApproval,
                        Some(serde_json::json!({
                            "outcome": "accept",
                            "permission": "prompt",
                            "reason": reason,
                        })),
                    ),
                }
            }
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
        record.evaluator_decision = evaluator_decision.clone();

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

        let trace_id = trigger.trace_id.clone();
        let idempotency_key = trigger.idempotency_key.clone();

        self.emit_harness_event(HarnessEvent::TriggerHandled {
            idempotency_key,
            trace_id: trace_id.clone(),
            state,
            audit_entry_id,
            evaluator_decision,
        });

        // Sub-agent execution only fires on the policy-Allow Accepted path. Other terminal
        // states (Deduped / CycleSuppressed / PermissionDenied / NeedsApproval) leave
        // `handle_trigger` here with only the audit + `TriggerHandled` event written.
        if state == TriggerState::Accepted {
            self.spawn_trigger_action(trigger);
        }

        outcome
    }

    /// Spawn the detached sub-agent task for an accepted trigger. RFC 1 §5.A: the parent
    /// `Agent` is single-tenant, so we cannot run the action on the same `AgentHarness`;
    /// instead each accepted trigger gets its own sub-harness rooted on an in-memory
    /// session. The parent session only gets the `trigger_result` audit when the sub-agent
    /// completes (or is cancelled).
    ///
    /// **Known limitation in sub-PR 5a**: the sub-agent's session is in-memory and
    /// discarded when the task finishes. Per the issue #20 amendment, jsonl-backed retained
    /// branches (so `pie --resume <trace_id>` can replay sub-agent transcripts for
    /// archaeology) is a sub-PR 5c follow-up. `trigger_result.summary` is preserved; the
    /// full sub-agent transcript is not.
    fn spawn_trigger_action(&self, trigger: Trigger) {
        // Snapshot every input the spawned task needs so the closure can be `'static`. We
        // intentionally do not require `self: &Arc<Self>` to avoid a breaking-change to
        // existing callers of `AgentHarness::new`; instead we capture the underlying
        // shared state through individual handles.
        let trace_id = trigger.trace_id.clone();
        let source_label = trigger.source_label.clone();
        let event_label = trigger.event_label.clone();
        let listeners = Arc::clone(&self.harness_listeners);
        let parent_session = self.session.clone();
        let running_registry = Arc::clone(&self.running_triggers);
        let action_hook = self.before_trigger_action.clone();
        let runtime_snapshot = self.trigger_runtime.snapshot();
        let parent_state = self.agent.state();
        let parent_model = parent_state.model.clone();
        let parent_system_prompt = parent_state.system_prompt.clone();
        let parent_tools = parent_state.tools.clone();
        let parent_thinking = parent_state.thinking_level;
        let stream_fn = self.stream_fn.clone();
        let before_tool_call = self.before_tool_call.clone();
        let after_tool_call = self.after_tool_call.clone();

        tokio::spawn(async move {
            run_trigger_action(
                trigger,
                trace_id,
                source_label,
                event_label,
                listeners,
                parent_session,
                running_registry,
                action_hook,
                runtime_snapshot,
                parent_model,
                parent_system_prompt,
                parent_tools,
                parent_thinking,
                stream_fn,
                before_tool_call,
                after_tool_call,
            )
            .await;
        });
    }

    /// Invoke the optional permission hook on an accepted trigger. Returns
    /// [`BeforeTriggerDecision::Allow`] when no hook is configured so the default-allow
    /// policy is path-equivalent to omitting the hook entirely.
    ///
    /// The hook receives a [`CancellationToken`] that the harness does not currently
    /// cancel; sub-PR 5 will pipe the harness's active-prompt cancel through this token so
    /// a permission UI can be aborted by Ctrl-C.
    async fn run_before_trigger_hook(&self, trigger: &Trigger) -> BeforeTriggerDecision {
        let Some(hook) = self.before_trigger.clone() else {
            return BeforeTriggerDecision::Allow;
        };
        let ctx = BeforeTriggerContext {
            trigger: trigger.clone(),
            runtime: self.trigger_runtime.snapshot(),
        };
        hook(ctx, tokio_util::sync::CancellationToken::new()).await
    }

    /// Point-in-time view of the harness's notification surface — the
    /// [`TriggerRuntimeSnapshot`] plus a `Vec<NotificationHookStatus>` collected from each
    /// registered hook via [`super::notification_hook::NotificationHook::status`]. The hook
    /// vec is a snapshot, not a live view; new registrations after this call are not
    /// reflected. Hook impls that have ended naturally still appear here until the next
    /// registration cycle — consumers should treat `NotificationHookStatus.state` as the
    /// source of truth for whether a hook is currently live.
    pub fn notification_status_snapshot(&self) -> NotificationStatusSnapshot {
        // Clone the `Arc`s out of the registry first so each hook's `status()` runs without
        // the registry mutex held. A slow `status()` (e.g. one that takes its own internal
        // lock) would otherwise block concurrent `register_notification_hook` calls.
        let hook_arcs: Vec<DynNotificationHook> = self.notification_hooks.lock().clone();
        let hooks: Vec<NotificationHookStatus> = hook_arcs.iter().map(|h| h.status()).collect();
        // Running triggers: clone the public-facing state out of each handle. Drop the lock
        // before returning so consumers cannot pin the registry against concurrent inserts /
        // removes by the spawned sub-agent tasks.
        let running: Vec<RunningTriggerState> = self
            .running_triggers
            .lock()
            .values()
            .map(|h| h.state.clone())
            .collect();
        NotificationStatusSnapshot {
            hooks,
            runtime: self.trigger_runtime.snapshot(),
            running,
        }
    }

    /// Cancel the in-flight sub-agent for `trace_id`. No-op if the trigger has already
    /// completed or was never accepted. The spawned task will observe the cancel inside its
    /// `select!`, abort the agent loop, and emit `TriggerFailed` with
    /// `reason == "aborted"` plus a `trigger_result { success: false, summary:
    /// Some("aborted") }` audit entry.
    pub fn abort_trigger(&self, trace_id: &str) {
        if let Some(handle) = self.running_triggers.lock().get(trace_id) {
            handle.cancel.cancel();
        }
    }

    /// Cancel every in-flight sub-agent. Each cancelled task writes its own
    /// `trigger_result` and emits `TriggerFailed`. Convenience wrapper around
    /// [`Self::abort_trigger`] for graceful shutdown.
    pub fn abort_all_triggers(&self) {
        let cancels: Vec<_> = self
            .running_triggers
            .lock()
            .values()
            .map(|h| h.cancel.clone())
            .collect();
        for c in cancels {
            c.cancel();
        }
    }

    /// Register a [`super::notification_hook::NotificationHook`] with the harness. Spawns
    /// two detached tokio tasks:
    /// - **Driver**: calls `hook.run(sink)` and drives the hook's transport (MCP read
    ///   pump, Cloudflare hub WebSocket, etc.). Triggers the hook produces flow through
    ///   the `sink` (an `mpsc::UnboundedSender<Trigger>`).
    /// - **Pump**: reads from the sink's receiver and calls
    ///   [`Self::handle_trigger`] for each trigger. Exits naturally when the sender is
    ///   dropped (e.g. when the hook's `run` future ends).
    ///
    /// The hook is stored for [`Self::notification_status_snapshot`] to read. There is no
    /// unregister API in this PR — hooks live until the harness is dropped or the driver
    /// task ends; the pump exits naturally when the sender closes. A later sub-PR may add
    /// explicit shutdown handles if a use case requires them; for now the YAGNI surface is
    /// "register and forget".
    ///
    /// `self: &Arc<Self>` because the pump task needs to clone the harness handle so
    /// `handle_trigger` is reachable from a `'static` future. Callers already hold the
    /// harness as `Arc<AgentHarness>` in `crates/coding-agent::main` so this is not a new
    /// ergonomic ask.
    pub fn register_notification_hook(self: &Arc<Self>, hook: DynNotificationHook) {
        use super::notification_hook::TriggerSink;
        let (sink, mut rx): (TriggerSink, _) = tokio::sync::mpsc::unbounded_channel();

        // Track for status snapshot before spawning so a status read immediately after
        // returning sees the new hook.
        self.notification_hooks.lock().push(hook.clone());

        // Driver task: the hook owns transport-side work; we only care about its
        // completion to free task resources. Errors aren't surfaced to a HarnessEvent
        // here (RFC 1 §4 puts that on the next sub-PR's HookStatusChanged event); the
        // hook reflects them through its own `status()` call.
        let hook_driver = hook.clone();
        tokio::spawn(async move {
            let _ = hook_driver.run(sink).await;
        });

        // Pump task: drain triggers into handle_trigger in order. We don't bound the
        // queue here — the hook's own backpressure is the right place for that since
        // it knows the transport's per-hook semantics (MCP push has no rate, hub frames
        // have per-topic rate limits, cron has burst smoothing).
        //
        // Contract: `handle_trigger` must not panic. The pump deliberately does NOT wrap
        // the call in `catch_unwind`, because today every transition `handle_trigger` runs
        // is internal (evaluator + audit append + emit). When sub-PR 4 starts dispatching
        // accepted triggers into the agent loop (which can panic via user-provided tools /
        // hooks), this loop will gain a `catch_unwind` shell plus a `HookPumpPanicked`
        // event so the hook surface can show "pump dead" rather than silently buffering
        // triggers into a dropped channel.
        let harness = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(trigger) = rx.recv().await {
                let _ = harness.handle_trigger(trigger).await;
            }
        });
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

// ─────────────────────────────────────────────────────────────────────────────────────────
// Sub-agent execution (RFC 1 sub-PR 5a)
// ─────────────────────────────────────────────────────────────────────────────────────────

/// Emit a [`HarnessEvent`] to a snapshot of the listener registry, isolating each listener
/// with `catch_unwind` so a single panicking listener cannot poison the others. Mirrors
/// the contract of `AgentHarness::emit_harness_event` but operates on a cloned `Arc` of
/// listeners (so the spawned sub-agent task does not need an `AgentHarness` reference).
fn emit_from_listeners(listeners: &Arc<Mutex<Vec<HarnessListener>>>, event: HarnessEvent) {
    let snapshot = listeners.lock().clone();
    for listener in snapshot {
        let event = event.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || listener(event)));
    }
}

/// Top-level body of the spawned sub-agent task. Drives the lifecycle:
/// 1. Resolve the `TriggerAction` via `before_trigger_action` hook (or default).
/// 2. Register the trigger as in-flight (`running_triggers`) + emit
///    `TriggerExecutionStarted`.
/// 3. Build the sub-agent's `Agent` on an in-memory session, inheriting parent context.
/// 4. Race `agent.prompt(action.prompt)` against the cancel token via `tokio::select!`.
/// 5. Compute `(success, summary, cost_usd)` from the agent's final state.
/// 6. Write the `trigger_result` audit entry to the **parent** session.
/// 7. Emit `TriggerCompleted` or `TriggerFailed`.
/// 8. Remove the trigger from `running_triggers`.
#[allow(clippy::too_many_arguments)]
async fn run_trigger_action(
    trigger: Trigger,
    trace_id: String,
    source_label: String,
    event_label: String,
    listeners: Arc<Mutex<Vec<HarnessListener>>>,
    parent_session: Session,
    running_registry: Arc<Mutex<std::collections::HashMap<String, RunningTriggerHandle>>>,
    action_hook: Option<BeforeTriggerActionHook>,
    runtime_snapshot: super::trigger_runtime::TriggerRuntimeSnapshot,
    parent_model: Option<Model>,
    parent_system_prompt: String,
    parent_tools: Vec<Arc<dyn AgentTool>>,
    parent_thinking: Option<ThinkingLevel>,
    stream_fn: Option<StreamFn>,
    before_tool_call: Option<BeforeToolCallHook>,
    after_tool_call: Option<AfterToolCallHook>,
) {
    // 1. Resolve action. Cancel token is the same one we'll race the agent loop against —
    // the hook can listen for it to abort a long-running rule/permission UI cleanly.
    let cancel = tokio_util::sync::CancellationToken::new();
    let action = match action_hook {
        Some(hook) => {
            let ctx = BeforeTriggerActionContext {
                trigger: trigger.clone(),
                runtime: runtime_snapshot,
            };
            hook(ctx, cancel.clone()).await
        }
        None => TriggerAction::default_for(&trigger),
    };

    // 2. Register as in-flight + emit ExecutionStarted. The preview is bounded to ~80 chars
    // because TUI banners cannot render arbitrary user content safely; the full prompt
    // remains audited through the sub-agent's own jsonl when 5c lands the retained branch.
    let prompt_preview = preview_for_banner(&action.prompt, 80);
    let started_at = chrono::Utc::now();
    {
        let mut reg = running_registry.lock();
        reg.insert(
            trace_id.clone(),
            RunningTriggerHandle {
                state: RunningTriggerState {
                    trace_id: trace_id.clone(),
                    source_label: source_label.clone(),
                    event_label: event_label.clone(),
                    started_at,
                    prompt_preview: prompt_preview.clone(),
                },
                cancel: cancel.clone(),
            },
        );
    }
    emit_from_listeners(
        &listeners,
        HarnessEvent::TriggerExecutionStarted {
            trace_id: trace_id.clone(),
            prompt_preview,
        },
    );

    // 3. Build sub-agent. In sub-PR 5a we use MemorySessionStorage; the sub-agent's
    // transcript lives in memory only and is discarded when this task finishes. Per the
    // issue #20 amendment, jsonl-backed retained branches land in sub-PR 5c. The
    // `trigger_result.summary` we persist to the parent session is the only durable
    // record of what the sub-agent produced in 5a.
    let sub_storage: Arc<dyn super::session::session::SessionStorage> =
        Arc::new(super::session::memory_storage::MemorySessionStorage::new());
    let sub_session = super::session::session::Session::new(sub_storage);

    let mut sub_state = AgentState::default();
    sub_state.model = parent_model;
    sub_state.thinking_level = parent_thinking;
    sub_state.tools = parent_tools;
    sub_state.system_prompt = parent_system_prompt;

    let sub_agent = Agent::new(AgentOptions {
        initial_state: Some(sub_state),
        stream_fn,
        before_tool_call,
        after_tool_call,
        ..Default::default()
    });

    // Persist sub-agent messages into the sub-session jsonl as they finalize. Even though
    // the storage is in-memory in 5a, this keeps the message-stream → session-state link
    // intact so 5c's jsonl swap is a pure storage change with no agent-loop refactor.
    let persist_errors: Arc<Mutex<Vec<super::types::SessionError>>> =
        Arc::new(Mutex::new(Vec::new()));
    let persist_session = sub_session.clone();
    let persist_errors_listener = persist_errors.clone();
    let _persist_unsub = sub_agent.subscribe(Arc::new(move |event, _cancel| {
        let session = persist_session.clone();
        let sink = persist_errors_listener.clone();
        Box::pin(async move {
            if let AgentEvent::MessageEnd { message } = event {
                if let Err(e) = session.append_message(message).await {
                    sink.lock().push(e);
                }
            }
        })
    }));

    // 4. Race agent.prompt against cancel. The sub-agent receives the resolved action
    // prompt as a user message. On abort we propagate to the sub-agent's own
    // CancellationToken via `Agent::abort()`.
    let user_message = AgentMessage::Llm(PiMessage::User(pie_ai::UserMessage {
        role: pie_ai::UserRole::User,
        content: pie_ai::UserContent::Text(action.prompt.clone()),
        timestamp: chrono::Utc::now().timestamp_millis(),
    }));
    let run_outcome: Result<(), AgentRunError> = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            sub_agent.abort();
            Err(AgentRunError::Other("aborted".into()))
        }
        res = sub_agent.prompt(user_message) => res,
    };

    // 5. Compute summary. The sub-agent's final assistant message is our best
    // first-cut summary for 5a (no model-driven self-summary yet — that's a 5b polish).
    let (success, summary, message_count) = compute_sub_agent_outcome(&sub_agent, &run_outcome);
    // Compute failure reason once (used in both the audit and the terminal event so the
    // jsonl record carries enough context to explain `success: false` after `--resume`).
    let failure_reason: Option<String> = if success {
        None
    } else {
        Some(match &run_outcome {
            Err(AgentRunError::Other(msg)) if msg == "aborted" => "aborted".to_string(),
            Err(e) => format!("{e}"),
            Ok(_) => "unknown failure".to_string(),
        })
    };

    // 6. Persist `trigger_result` to PARENT session. Best-effort: on failure we emit a
    // `PersistenceError` reflux event (same shape as `trigger_audit` failures in sub-PR 2)
    // but still proceed to remove from registry + emit terminal event.
    //
    // `cost_usd` is omitted (Option/null) in 5a because the bare sub-`Agent` here has no
    // `CostTracker` wrapper — the parent `AgentHarness::cost` only auto-accrues for the
    // parent's own listener. Sub-PR 5b/5c will add a sub-harness wrapper or hook the
    // sub-agent's `MessageEnd` events into the parent `CostTracker`. Reporting `0.0`
    // today would lie about a real measurement; `null` honestly says "unknown".
    let result_data = serde_json::json!({
        "trace_id": trace_id,
        "branch_id": serde_json::Value::Null,
        "success": success,
        "summary": summary,
        "message_count": message_count,
        "cost_usd": serde_json::Value::Null,
        "reason": failure_reason,
    });
    let audit_write_result = parent_session
        .append_custom("trigger_result", Some(result_data))
        .await;
    if let Err(e) = audit_write_result {
        emit_from_listeners(
            &listeners,
            HarnessEvent::PersistenceError {
                context: "trigger_result".into(),
                message: format!("trigger_result append failed: {:?}", e.code),
            },
        );
    }
    // Also surface any sub-agent-side persist errors so they aren't silently swallowed.
    for e in persist_errors.lock().iter() {
        emit_from_listeners(
            &listeners,
            HarnessEvent::PersistenceError {
                context: "trigger_result".into(),
                message: format!("sub-agent session append failed: {:?}", e.code),
            },
        );
    }

    // 7. Terminal event. `reason` for Failed is sanitized: we pass the `AgentRunError`'s
    // `Display` (free-form but generally short error string from our own code paths) and
    // explicitly avoid embedding any sub-agent message bodies / provider response content.
    if success {
        // `cost_usd: None` mirrors the audit's `cost_usd: null`. Sub-agent in 5a is bare
        // (no CostTracker wrapper); reporting 0.0 here while the audit said null would
        // make event subscribers + jsonl readers disagree about the same field. 5b/5c
        // will populate this with a real measurement when the sub-agent is wrapped.
        emit_from_listeners(
            &listeners,
            HarnessEvent::TriggerCompleted {
                trace_id: trace_id.clone(),
                summary,
                cost_usd: None,
            },
        );
    } else {
        emit_from_listeners(
            &listeners,
            HarnessEvent::TriggerFailed {
                trace_id: trace_id.clone(),
                reason: failure_reason.unwrap_or_else(|| "unknown failure".to_string()),
            },
        );
    }

    // 8. Remove from registry.
    running_registry.lock().remove(&trace_id);
}

/// Inspect the sub-agent's terminal state to summarize the outcome. Returns
/// `(success, summary, message_count)`.
///
/// `summary` is the text of the sub-agent's final assistant message when one exists; this
/// is a first-cut heuristic for 5a. Sub-PR 5b can replace this with a model-driven summary
/// or a hook-supplied template-rendered summary.
fn compute_sub_agent_outcome(
    sub_agent: &Agent,
    run_outcome: &Result<(), AgentRunError>,
) -> (bool, Option<String>, usize) {
    if let Err(_e) = run_outcome {
        // Try to grab a partial last-assistant-message even on failure for context.
        let state = sub_agent.state();
        let last = last_assistant_text(&state);
        return (false, last, state.messages.len());
    }
    let state = sub_agent.state();
    let summary = last_assistant_text(&state);
    (true, summary, state.messages.len())
}

/// Extract the text of the last assistant message, if any. Returns `None` if the agent
/// produced no assistant content (e.g. aborted before the first turn). Truncated to 4 KiB
/// per RFC 1 §5.B size cap.
fn last_assistant_text(state: &AgentState) -> Option<String> {
    let last = state.messages.iter().rev().find_map(|m| match m {
        AgentMessage::Llm(pie_ai::Message::Assistant(a)) => Some(a),
        _ => None,
    })?;
    let mut text = String::new();
    for block in &last.content {
        if let pie_ai::ContentBlock::Text(t) = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&t.text);
        }
    }
    if text.is_empty() {
        return None;
    }
    const SUMMARY_CAP_BYTES: usize = 4096;
    if text.len() > SUMMARY_CAP_BYTES {
        // Walk back from the byte cap to the previous UTF-8 char boundary so `truncate`
        // never lands inside a multi-byte codepoint (which would panic). The cap is
        // generous; in practice the boundary walk shifts at most 3 bytes.
        let mut cut = SUMMARY_CAP_BYTES;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("…[truncated]");
    }
    Some(text)
}

/// Bounded preview text for status banners. Avoids panicking on multi-byte char boundaries
/// by walking char count, not byte count.
fn preview_for_banner(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}
