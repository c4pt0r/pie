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
    /// A trigger admitted by the dedup / cycle evaluator reached
    /// [`BeforeTriggerDecision::Prompt`] and is awaiting an embedder-owned user decision.
    ///
    /// This is the trigger-shaped half of issue #110. The prompt is bound by
    /// `trigger_prompt_id`, not by a tool-call id / args hash, so a decision cannot be
    /// replayed onto a different trigger envelope. The runtime also writes a
    /// `trigger_prompt` Custom audit entry when the prompt resolves.
    TriggerPromptRequest { request: TriggerPromptRequest },
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
        source_label: String,
        event_label: String,
        prompt_preview: String,
    },
    /// A sub-agent execution finished successfully and the parent `trigger_result` audit
    /// entry has been written. `summary` is the sub-agent's self-summary (size-capped at
    /// 4 KiB). `cost_usd` is `None` in sub-PR 5a because the bare sub-`Agent` has no
    /// `CostTracker` wrapper — the value mirrors the audit's `cost_usd: null`. Sub-PR 5b
    /// or 5c wraps the sub-agent in a mini-`CostTracker` and `cost_usd` will be `Some(f)`.
    ///
    /// `details` is the structured sub-agent result envelope populated through marker tools
    /// (see [`TriggerResultDetailsBuilder`]). Defaults to `serde_json::Value::Null` until a
    /// sub-agent tool writes through the builder. Authorization for
    /// [`PromoteAction::PromoteSummaryWhenResultDetailsMatch`] flows exclusively through
    /// this field — `summary` is display-only and is NEVER consulted by the promotion gate.
    TriggerCompleted {
        trace_id: String,
        summary: Option<String>,
        cost_usd: Option<f64>,
        details: serde_json::Value,
    },
    /// A sub-agent execution failed (agent loop error, panic-via-spawn-error, or aborted by
    /// [`AgentHarness::abort_trigger`] / [`AgentHarness::abort_all_triggers`]). `reason` is
    /// sanitized — never contains raw payload, provider response bodies, or credential
    /// material. The parent `trigger_result` audit entry has been written with
    /// `success: false`.
    TriggerFailed { trace_id: String, reason: String },
    /// An [`TriggerDelivery::InjectAndRun`] trigger has injected its prompt into the **idle**
    /// parent conversation and is asking the embedder to run ONE model turn in the parent's
    /// full context. The runtime cannot run the single-tenant parent agent itself from the
    /// detached trigger task, so it delegates: the embedder (which owns the parent agent and
    /// its input loop) should funnel this through the same serialized path as user input and
    /// call [`AgentHarness::continue_`]. Emitted only on the idle path — when the parent is
    /// mid-turn the runtime enqueues a follow-up instead and this event is NOT emitted.
    TriggerRequestsMainRun { trace_id: String },
    /// A trigger's `PromoteAction` rendered successfully and the runtime committed to
    /// surfacing the sub-agent result to the user / LLM. pie_ai has no System role today;
    /// the inserted entry is a `Message::User` with a `[Trigger ...]` body prefix so the
    /// LLM disambiguates trigger-driven context from human input.
    ///
    /// `inserted_entry_id` semantics depend on the parent agent state at promotion time
    /// (see also `trigger_promotion.state` in the same-trace audit entry):
    ///
    /// - **Idle parent** (`apply_promotion` took the synchronous-append branch):
    ///   `inserted_entry_id` is the durable id of the appended `Message::User` and
    ///   matches `trigger_promotion.inserted_entry_id`. Audit `state: "success"`.
    /// - **Streaming parent** (queued through the loop's follow-up queue to avoid a
    ///   double-persistence / ordering race): `inserted_entry_id` is an **empty string**
    ///   because the session entry ID is only known after the loop drains the queue and
    ///   the session listener writes the entry. Consumers should correlate by `trace_id`
    ///   in this case. Audit `state: "queued"` and `inserted_entry_id: null` for the
    ///   matching `trigger_promotion` entry.
    ///
    /// Causality (RFC 1 §5.F): `TriggerCompleted | TriggerFailed` → `TriggerPromoted` for
    /// the same `trace_id` when promotion is configured AND not held for approval.
    TriggerPromoted {
        trace_id: String,
        promote_kind: String,
        /// See variant rustdoc: durable id for the idle branch; empty for the streaming
        /// queued branch (consumers join by `trace_id`).
        inserted_entry_id: String,
        template_name: Option<String>,
        redaction_status: String,
    },
    /// A trigger's `PromoteAction` was held pending approval (`promote_requires_approval =
    /// true`) and is awaiting an explicit `/triggers approve <trace_id>` (which lands in
    /// sub-PR 6). The parent transcript has NOT been modified; a `trigger_promotion`
    /// audit entry with `state: "pending"` has been written. `preview` is the rendered
    /// template body the approval UI would surface, or `None` when the render itself
    /// would have failed (in which case the audit reflects `redaction_status: "render_error"`
    /// and `state: "failed"`).
    PromotionPending {
        trace_id: String,
        promote_kind: String,
        template_name: Option<String>,
        preview: Option<String>,
    },
    /// Emitted once per prompt-cycle boundary after [`OnTurnEndHook`] returns (or after
    /// the runtime decides not to invoke it because the continuation cap was reached).
    /// Lets the TUI / UI render "evaluator says: keep going" between continuation runs
    /// without snooping the `turn_end_decision` audit entry.
    ///
    /// `decision` mirrors [`TurnEndAction::as_audit_str`] (`"stop"` / `"pause"` /
    /// `"continue"`) plus `"budget_limited"` for the cap-exceeded path. `reason` carries
    /// the pause reason (or the cap value rendered as a sentence) when applicable.
    /// `next_prompt_preview` is the first ~80 chars of the continuation prompt on
    /// `Continue` decisions; `None` otherwise.
    ///
    /// `continuation_count` is the *post*-decision counter: it reflects the iteration
    /// number after the runtime applied the decision. For `Stop` / `Pause` this equals
    /// the count of `Continue` decisions that fired earlier in the same prompt cycle.
    TurnEnded {
        decision: &'static str,
        continuation_count: u32,
        reason: Option<String>,
        next_prompt_preview: Option<String>,
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

/// Bounded, preview-safe trigger prompt request emitted when
/// [`BeforeTriggerDecision::Prompt`] asks the embedder to admit or deny a trigger.
///
/// Runtime owns only exact per-trigger resolution. "Always" / "Block future sender" trust
/// caches are embedder-owned (for fefe, `~/.pie/hub-trust.json`) and should be audited
/// separately via a domain-specific Custom entry such as `fefe_trust_decision`.
#[derive(Clone, Debug, PartialEq)]
pub struct TriggerPromptRequest {
    /// SHA-256 over the canonical binding tuple. This is the stable token the embedder
    /// echoes back through [`OnTriggerPromptHook`]'s decision path.
    pub trigger_prompt_id: String,
    pub trace_id: String,
    pub source_label: String,
    /// Receiver id is optional at the generic runtime layer because non-hub trigger
    /// sources may not have a receiver principal. Hub adapters should populate it in
    /// `_meta.receiver_agent_id` or `receiver_agent_id` so fefe first-contact prompts bind
    /// to the full `{receiver_agent_id, sender_agent_id, action_class}` scope.
    pub receiver_agent_id: Option<String>,
    pub sender_agent_id: String,
    pub action_class: String,
    pub trigger_summary: Option<String>,
    /// Embedder-rendered preview payload. Runtime constructs this from bounded envelope
    /// fields only and never includes raw `Trigger.payload`.
    pub payload: serde_json::Value,
    pub reason: String,
}

/// Decision returned by [`OnTriggerPromptHook`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TriggerPromptDecision {
    Allow,
    Deny { reason: Option<String> },
    Timeout { reason: Option<String> },
}

impl TriggerPromptDecision {
    pub fn as_audit_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny { .. } => "deny",
            Self::Timeout { .. } => "timeout",
        }
    }

    fn reason(&self) -> Option<String> {
        match self {
            Self::Deny { reason } => reason
                .as_ref()
                .map(|reason| cap_trigger_prompt_reason(reason)),
            Self::Timeout { reason } => reason
                .as_ref()
                .map(|reason| cap_trigger_prompt_reason(reason)),
            _ => None,
        }
    }
}

pub type OnTriggerPromptHook = Arc<
    dyn Fn(
            TriggerPromptRequest,
            tokio_util::sync::CancellationToken,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = TriggerPromptDecision> + Send>>
        + Send
        + Sync,
>;

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
/// [`AgentHarness::notification_status_snapshot`]. The TUI / `/triggers sources` command
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
/// `promote` controls whether the completed trigger result is only audited or also injected
/// into the parent session and parent agent context.
#[derive(Clone, Debug)]
pub struct TriggerAction {
    pub prompt: String,
    /// How a successful run is mirrored back into the parent transcript. Honored for
    /// [`TriggerDelivery::SubAgent`] (applied to the sub-agent's result) and
    /// [`TriggerDelivery::InjectSummary`] (applied to `trigger.payload_summary` as the
    /// faux result). **Ignored for [`TriggerDelivery::InjectAndRun`]**: that mode
    /// direct-injects `prompt` and asks the embedder to run one parent-loop turn, so
    /// there's no separate "result" for `promote` to act on. Set `promote = None` for
    /// `InjectAndRun` to make intent obvious.
    pub promote: PromoteAction,
    pub promote_requires_approval: bool,
    /// How the runtime delivers this action. Default [`TriggerDelivery::SubAgent`] preserves
    /// the historical behavior (run a sub-agent against `prompt`). [`TriggerDelivery::InjectSummary`]
    /// skips the sub-agent entirely — see that variant for the rationale.
    pub delivery: TriggerDelivery,
}

/// Whether an accepted trigger runs a sub-agent or is delivered straight to the parent loop.
///
/// The runtime stays domain-agnostic across both modes: it never inspects what the source
/// *is*, only moves the opaque `payload_summary` string. Which mode applies is decided
/// entirely upstream by the [`BeforeTriggerActionHook`] (e.g. a per-source config in
/// `crates/coding-agent`), never hardcoded here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TriggerDelivery {
    /// Run a fresh sub-agent against [`TriggerAction::prompt`], then apply `promote` to its
    /// result. This is the default and the only mode that involves the model.
    #[default]
    SubAgent,
    /// Skip the sub-agent. The runtime treats `trigger.payload_summary` as the result
    /// `summary` and applies `promote` directly — no model call, no tools, zero cost
    /// **for the trigger itself**. Used by sources configured as pure notification feeds.
    /// `prompt` is ignored in this mode.
    ///
    /// Note on cost attribution: when `promote` is non-`None` and the parent is mid-turn,
    /// `apply_promotion`'s streaming branch enqueues a follow-up which the parent loop
    /// drains into a real model turn. That turn's cost is attributed to the parent agent's
    /// own usage, not to this trigger's `trigger_result.cost_usd` (which stays 0.0 — an
    /// honest measurement of the direct trigger work). If you want truly zero cascade cost,
    /// pair `InjectSummary` with `PromoteAction::None`.
    InjectSummary,
    /// Skip the sub-agent, but inject [`TriggerAction::prompt`] into the **parent**
    /// conversation and arrange for ONE model turn to run in the parent's full context.
    ///
    /// The runtime never runs the single-tenant parent agent from the detached trigger task.
    /// Instead: if the parent is mid-turn it enqueues a follow-up (the running loop picks it
    /// up at the next boundary); if the parent is idle it appends the message and emits
    /// [`HarnessEvent::TriggerRequestsMainRun`] so the embedder — which owns the parent agent
    /// — can schedule the turn on its own serialized loop. The model turn itself is a normal
    /// parent-loop event, not attributed to this trigger's `trigger_result`.
    InjectAndRun,
}

/// Audit-shape note for downstream JSONL readers and `/triggers audit` consumers:
///
/// The `trigger_promotion` and `trigger_result` audit entries both carry a
/// `prefix_injected: bool` field (recording whether the engine had to prepend the
/// `[Trigger {trace_id}] ` attribution prefix), but the *placement* depends on which
/// delivery path produced the audit:
///
/// - [`TriggerDelivery::SubAgent`] + `PromoteAction::PromoteSummaryNow`/etc.: prefix lives
///   on the `trigger_promotion` audit (written by `apply_promotion`).
/// - [`TriggerDelivery::InjectSummary`]: prefix lives on the `trigger_promotion` audit
///   (`apply_promotion` is still called for the summary).
/// - [`TriggerDelivery::InjectAndRun`]: prefix lives on the `trigger_result` audit directly
///   (no `apply_promotion` call; the inject path writes its own audit).
///
/// JSONL readers that join on `trace_id` should check both audit types for the field.
const _AUDIT_SHAPE_DOC: () = ();

impl TriggerAction {
    /// The default `Prompt` form used when no [`BeforeTriggerActionHook`] is configured.
    /// `format!("{source_label} fired: {event_label}")` is the RFC 1 §5.C stable fallback —
    /// always non-empty and carries enough context that the sub-agent can react.
    pub fn default_for(trigger: &Trigger) -> Self {
        Self {
            prompt: format!("{} fired: {}", trigger.source_label, trigger.event_label),
            promote: PromoteAction::None,
            promote_requires_approval: false,
            delivery: TriggerDelivery::SubAgent,
        }
    }
}

/// How a completed sub-agent's `trigger_result` should affect the parent session. `None`
/// leaves the result in audit/TUI only. `PromoteSummaryNow` inserts a templated result into
/// the parent session immediately. `PromoteSummaryWhenResultDetailsMatch` is the
/// dynamic-rule path: promotion is gated on **structured** sub-agent result details, never
/// on free-form summary text — eliminates the prompt-injection / authorization-channel risk
/// of the older `PromoteSummaryWhenSummaryContains` variant (still present for transition).
/// `InjectNextTurn` per the issue #20 amendment is deferred to sub-PR 6 / RFC 4 work.
#[derive(Clone, Debug, Default)]
pub enum PromoteAction {
    #[default]
    None,
    PromoteSummaryNow {
        /// **Inline template body** to render against the allowlisted context. `None` uses
        /// the runtime's built-in safe default. The audit + event `template_name` field is
        /// always `None` in v1 (named-template lookup via `PromptTemplateRegistry` lands
        /// in sub-PR 6 / RFC 4 rule engine work); the body is what gets rendered but is
        /// never persisted as `template_name` because the audit contract reserves
        /// `template_name` for a registry-style identity, not the body content.
        template_body: Option<String>,
    },
    /// Deprecated: free-form `summary` substring matching cannot safely gate promotion —
    /// the sub-agent's natural-language output becomes an authorization channel a custom
    /// rule action or model paraphrase can manipulate. Prefer
    /// [`PromoteAction::PromoteSummaryWhenResultDetailsMatch`] which evaluates a
    /// `PromotionCondition` against structured `trigger_result.details` instead. Kept here
    /// during the transition; downstream PRs remove it once all callers have migrated.
    #[deprecated(
        note = "promotes on free-form summary substring; use PromoteSummaryWhenResultDetailsMatch with structured PromotionCondition::AnyOf instead"
    )]
    PromoteSummaryWhenSummaryContains {
        template_body: Option<String>,
        required_substrings: Vec<String>,
    },
    /// Promotion is gated on a [`PromotionCondition`] evaluated against the sub-agent's
    /// **structured** `trigger_result.details` (populated by the sub-agent via marker tools,
    /// not by parsing free-form output). Fail-closed: any failure to evaluate the condition
    /// (pointer missing, value not an array, empty intersection) skips promotion and emits
    /// a `trigger_promotion` audit entry with `state: "skipped"` and a `reason` field.
    PromoteSummaryWhenResultDetailsMatch {
        template_body: Option<String>,
        condition: PromotionCondition,
    },
}

/// Structured condition evaluated against `trigger_result.details` to decide whether a
/// `PromoteAction::PromoteSummaryWhenResultDetailsMatch` actually fires. Authorization
/// flows through this condition — never through the sub-agent's free-form `summary` text.
///
/// Future variants (e.g. `AllOf`, `KeyEquals`) can be added without breaking existing
/// callers; the enum is intentionally narrow today to keep the auth surface auditable.
#[derive(Clone, Debug)]
pub enum PromotionCondition {
    /// Resolve `json_pointer` against `details` (RFC 6901). Fires iff the value resolves
    /// to a JSON array AND that array shares at least one element with `any_of`. Any
    /// other state (pointer missing, value not an array, empty intersection) returns
    /// false and is recorded in the `trigger_promotion` audit with a specific `reason`.
    ///
    /// Typical use: `json_pointer = "/dynamic_trigger/matched_rule_ids"`, `any_of =
    /// <list of rule IDs that have promote_to_chat=true AND are currently enabled>`.
    AnyOf {
        json_pointer: String,
        any_of: Vec<String>,
    },
}

impl PromotionCondition {
    /// Evaluate against the sub-agent's structured `details`. Returns the intersection on
    /// match (so the caller can write `promote_eligible_rule_ids` for audit/UI), or a
    /// machine-readable skip reason on mismatch.
    pub fn evaluate(
        &self,
        details: &serde_json::Value,
    ) -> Result<Vec<String>, PromotionConditionSkipReason> {
        match self {
            Self::AnyOf {
                json_pointer,
                any_of,
            } => {
                let Some(value) = details.pointer(json_pointer) else {
                    return Err(PromotionConditionSkipReason::PointerMissing);
                };
                let Some(arr) = value.as_array() else {
                    return Err(PromotionConditionSkipReason::ValueNotArray);
                };
                let matched: Vec<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| any_of.iter().any(|needle| needle == s))
                    .map(str::to_string)
                    .collect();
                if matched.is_empty() {
                    Err(PromotionConditionSkipReason::EmptyIntersection)
                } else {
                    Ok(matched)
                }
            }
        }
    }
}

/// Machine-readable reason a [`PromotionCondition`] declined to fire. Surfaces in the
/// `trigger_promotion` audit's `reason` field as a stable string ID so downstream tools
/// (CLI `/triggers audit`, automated runbooks) can compare against an enum, not a sentence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromotionConditionSkipReason {
    /// `details.pointer(json_pointer)` returned `None`. Usually means the sub-agent did
    /// not call its marker tool — fail-closed default.
    PointerMissing,
    /// Pointer resolved to a non-array value. Sub-agent populated `details` but in the
    /// wrong shape; treat as a contract violation.
    ValueNotArray,
    /// Array exists but no element matches any entry in `any_of`. Sub-agent marked some
    /// rules but none that are allowlisted for promotion.
    EmptyIntersection,
}

impl PromotionConditionSkipReason {
    /// Stable string identifier for audit / event serialization. Avoid stringifying the
    /// `Debug` representation — these strings are part of the audit contract.
    pub fn as_audit_str(self) -> &'static str {
        match self {
            Self::PointerMissing => "result_details_missing",
            Self::ValueNotArray => "result_details_not_array",
            Self::EmptyIntersection => "no_matching_rule_id",
        }
    }
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

// ─────────────────────────────────────────────────────────────────────────────────────────
// OnTurnEnd hook (powers `/goal` and other turn-completion driven orchestrators)
// ─────────────────────────────────────────────────────────────────────────────────────────

/// Snapshot passed into [`OnTurnEndHook`] after a prompt-cycle reaches a natural stop
/// (assistant turned in a no-tool-call message, the agent's own `should_stop_after_turn`
/// returned true, etc.). The hook owns the cross-prompt decision: should the harness
/// start another prompt cycle in the same conversation (for `/goal` evaluator-driven
/// continuation), pause it, or stop normally.
///
/// `transcript` is a **clone** of `Agent::state().messages` taken at the boundary — the
/// mutex is released before the hook runs, so the hook future is `'static`. The hook is
/// responsible for bounding what it forwards downstream (e.g. last N messages, token cap)
/// when it builds an evaluator prompt; the runtime does not pre-trim because different
/// orchestrators want different windows.
///
/// `continuation_count` is the number of times this same prompt-cycle has already been
/// continued by an earlier `TurnEndAction::Continue` decision. Starts at 0 on the
/// original user/template/continue entry, increments by 1 each time the hook decides to
/// continue. The hard cap is [`AgentHarnessOptions::turn_continuation_cap`]; the runtime
/// stops calling the hook (and records `decision: "budget_limited"`) once it would be
/// exceeded — no need for the hook to enforce the cap itself.
///
/// `last_user_prompt` carries the text of the most recent `Message::User` text content,
/// when one is identifiable, so evaluators can render "the user asked for X" without
/// re-walking the transcript. `None` when no user-text message exists (e.g. `continue_`
/// from a transcript with only assistant + tool messages).
#[derive(Clone)]
pub struct OnTurnEndContext {
    pub transcript: Vec<AgentMessage>,
    pub continuation_count: u32,
    pub last_user_prompt: Option<String>,
}

/// What the runtime should do after [`OnTurnEndHook`] inspects a completed prompt cycle.
///
/// `Stop` / `Pause` / `Continue` each map to a stable `decision` string in the persisted
/// `turn_end_decision` audit entry (`"stop"` / `"pause"` / `"continue"`); a fourth
/// `"budget_limited"` value is reserved for the runtime-emitted audit when the
/// continuation cap is hit before the hook can run, so call sites never need to invent
/// that string themselves. `Noop` is intentionally not in that list — it deliberately
/// writes nothing.
#[derive(Clone, Debug)]
pub enum TurnEndAction {
    /// Hook is currently inactive and has nothing to record for this turn. Behaves
    /// identically to "no `on_turn_end` configured": **no `turn_end_decision` audit
    /// entry is written, and no [`HarnessEvent::TurnEnded`] is emitted**. Use when the
    /// hook is permanently registered but only meaningful in specific session states
    /// — e.g. `/goal` returns `Noop` when there is no active goal, when the goal is
    /// already `achieved`, or when the user has paused it externally — so untouched
    /// sessions don't accumulate noise audit entries on every prompt.
    ///
    /// `TurnEndDecision::payload` is ignored when `action == Noop`; pass `None`.
    Noop,
    /// Normal completion. Runtime returns control to the caller. Records
    /// `decision: "stop"` in the `turn_end_decision` audit and emits
    /// [`HarnessEvent::TurnEnded`].
    Stop,
    /// Soft stop with an explanatory reason (e.g. "evaluator unavailable", "user
    /// requested pause"). Persisted in `turn_end_decision.data.reason` and surfaced
    /// through [`HarnessEvent::TurnEnded`]. Runtime returns control to the caller.
    Pause { reason: String },
    /// Run another prompt cycle in the same conversation. The runtime appends `prompt`
    /// as a user `AgentMessage`, runs auto-compaction again, then drives the inner
    /// agent's loop. `continuation_count` increments by 1 before the next hook call.
    Continue { prompt: String },
}

impl TurnEndAction {
    /// Stable `decision` string for the `turn_end_decision` audit entry. `Noop` is
    /// intentionally unmapped — it returns `None` and signals to the runtime that no
    /// audit / event should be emitted for this turn. Avoid stringifying the `Debug`
    /// representation — these values are part of the audit contract and downstream
    /// JSONL readers compare against them.
    pub fn as_audit_str(&self) -> Option<&'static str> {
        match self {
            Self::Noop => None,
            Self::Stop => Some("stop"),
            Self::Pause { .. } => Some("pause"),
            Self::Continue { .. } => Some("continue"),
        }
    }
}

/// Decision envelope returned from [`OnTurnEndHook`]. Wrapping the action lets the hook
/// attach an opaque embedder-owned `payload` that gets persisted into the
/// `turn_end_decision` audit record under `data.payload` — runtime never inspects it.
/// `/goal` uses this to record evaluator JSON, evidence quotes, evaluator model id, etc.,
/// without runtime needing to know about goal-mode-specific fields.
#[derive(Clone, Debug)]
pub struct TurnEndDecision {
    pub action: TurnEndAction,
    /// Optional structured payload merged into the `turn_end_decision` audit entry as
    /// `data.payload`. `None` writes `data.payload: null`. The embedder is responsible
    /// for keeping this serializable and small — bodies should be capped before being
    /// returned, just like trigger result summaries.
    pub payload: Option<serde_json::Value>,
}

impl From<TurnEndAction> for TurnEndDecision {
    fn from(action: TurnEndAction) -> Self {
        Self {
            action,
            payload: None,
        }
    }
}

/// Hook invoked at the boundary between two prompt cycles inside
/// [`AgentHarness::prompt`] / [`AgentHarness::continue_`]. Fires exactly once after the
/// inner agent's loop returns (success or `AgentRunError` short-circuit), with the cancel
/// token wired to [`AgentHarness::abort`] so user-driven aborts interrupt the hook's own
/// awaits (e.g. an evaluator sub-agent call).
///
/// Returning [`TurnEndAction::Continue { prompt }`] starts a new prompt cycle with the
/// given text appended as a `Message::User`. Returning [`TurnEndAction::Stop`] or
/// [`TurnEndAction::Pause`] returns control to the caller with an audit/event.
/// Returning [`TurnEndAction::Noop`] returns control without audit/event, matching the
/// no-hook path. `None` (no hook configured) is equivalent to `Noop`.
///
/// The hook runs **after** the persistence listener has flushed every `MessageEnd` to
/// the session, so `transcript` matches what `--resume` would replay. It runs **before**
/// the runtime writes the `turn_end_decision` audit entry — the entry's `payload` field
/// comes from the returned [`TurnEndDecision::payload`].
pub type OnTurnEndHook = Arc<
    dyn Fn(
            OnTurnEndContext,
            tokio_util::sync::CancellationToken,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TurnEndDecision> + Send>>
        + Send
        + Sync,
>;

/// Default maximum number of [`TurnEndAction::Continue`] iterations per prompt cycle.
/// When exceeded, the runtime records a `turn_end_decision` audit with
/// `decision: "budget_limited"` and returns control to the caller without invoking the
/// hook again. Embedders override via [`AgentHarnessOptions::turn_continuation_cap`].
pub const DEFAULT_TURN_CONTINUATION_CAP: u32 = 25;

/// Result of [`AgentHarness::run_evaluator`]. The evaluator is a sub-agent with
/// `tools: []` and an in-memory session, so its only durable artifact is the assistant
/// text it produced. Callers (e.g. the `/goal` `GoalStopHook`) typically parse this as
/// JSON; the helper does not attempt to parse — that keeps the evaluator's prompt and
/// expected response shape entirely embedder-owned.
#[derive(Clone, Debug)]
pub struct EvaluatorOutput {
    /// Text content of the evaluator's last assistant message, truncated to 4 KiB on a
    /// char boundary (same cap [`compute_sub_agent_outcome`] applies to trigger
    /// summaries). `None` when the evaluator produced no assistant text (e.g. the run
    /// was cancelled before the first token).
    pub last_assistant_text: Option<String>,
}

/// Why [`AgentHarness::run_evaluator`] could not return a usable output. Distinct from
/// `AgentRunError` so callers can render policy-specific messages ("evaluator failed —
/// goal paused").
#[derive(Debug, thiserror::Error)]
pub enum EvaluatorError {
    #[error("evaluator agent failed: {0}")]
    Run(#[from] AgentRunError),
    #[error("evaluator cancelled")]
    Cancelled,
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
    /// Optional control-plane prompt resolution channel (issue #110 design v0.2 Artifact C).
    /// Routes through the bare `Agent`'s `on_control_plane_prompt` slot. `None` is
    /// fail-closed deny — any tool whose `permission_classification` returns `Prompt`
    /// (and no user `before_tool_call` hook hard-blocks) will receive a synthesized deny
    /// at runtime rather than executing. See `crates/agent/src/agent_loop.rs` for the
    /// merge semantics.
    pub on_control_plane_prompt: Option<crate::types::OnControlPlanePromptHook>,
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
    /// Optional trigger prompt resolution channel (issue #110 Artifact D). When
    /// [`BeforeTriggerHook`] returns [`BeforeTriggerDecision::Prompt`], the harness emits a
    /// [`HarnessEvent::TriggerPromptRequest`], awaits this hook, writes `trigger_prompt`
    /// audit, then either admits (`Allow`) or leaves the trigger in `NeedsApproval`
    /// (`Deny` / `Timeout`). `None` is fail-closed deny.
    pub on_trigger_prompt: Option<OnTriggerPromptHook>,
    /// Optional action hook resolving accepted triggers to a [`TriggerAction`]. `None`
    /// falls back to [`TriggerAction::default_for`] (the stable `format!("{source_label}
    /// fired: {event_label}")` mapping).
    pub before_trigger_action: Option<BeforeTriggerActionHook>,
    /// Optional async closure invoked by [`AgentHarness::reload_skills_from_disk`] to fetch
    /// the up-to-date skill catalog from whatever sources the embedder considers
    /// authoritative (filesystem dirs, registry, …). When `None`,
    /// `reload_skills_from_disk` returns [`ReloadSkillsError::NotConfigured`].
    ///
    /// The closure owns: source directory list, dedup policy (e.g. project-wins),
    /// per-skill diagnostic aggregation. Runtime stays IO-free — it never inspects the
    /// filesystem itself. This keeps `~/.pie/skills` vs project `.pie/skills` precedence
    /// and naming policy in one place (the embedder), so startup loading and runtime
    /// reload (e.g. after `InstallSkillTool` writes a new `SKILL.md`) share one source of
    /// truth.
    pub reload_skills_fn: Option<ReloadSkillsFn>,
    /// Optional hook invoked after every prompt cycle completes. Powers `/goal` and
    /// any other turn-completion driven orchestrator. See [`OnTurnEndHook`] for the
    /// contract. `None` is equivalent to a hook that always returns
    /// [`TurnEndAction::Noop`] (i.e. current behavior).
    pub on_turn_end: Option<OnTurnEndHook>,
    /// Cap on the number of [`TurnEndAction::Continue`] decisions the runtime applies
    /// to a single prompt cycle. `None` uses [`DEFAULT_TURN_CONTINUATION_CAP`]. Set
    /// `0` to disable continuation entirely (the hook still fires once for audit /
    /// observability, but `Continue` decisions are treated as `budget_limited`).
    pub turn_continuation_cap: Option<u32>,
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
            on_control_plane_prompt: None,
            budget_cap_usd: None,
            trigger_runtime: TriggerRuntimeConfig::default(),
            before_trigger: None,
            on_trigger_prompt: None,
            before_trigger_action: None,
            reload_skills_fn: None,
            on_turn_end: None,
            turn_continuation_cap: None,
        }
    }
}

/// Async loader closure invoked by [`AgentHarness::reload_skills_from_disk`]. Returns the
/// fresh skill catalog (post-dedup, per the embedder's policy) plus any per-skill
/// diagnostics from the load. See [`AgentHarnessOptions::reload_skills_fn`] for the
/// design rationale (one source-of-truth across startup load + runtime reload).
pub type ReloadSkillsFn = std::sync::Arc<
    dyn Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = super::skills::LoadSkillsOutput> + Send>,
        > + Send
        + Sync,
>;

/// Why [`AgentHarness::reload_skills_from_disk`] couldn't run.
#[derive(Debug, thiserror::Error)]
pub enum ReloadSkillsError {
    /// [`AgentHarnessOptions::reload_skills_fn`] was `None` at construction. Callers should
    /// either pass a loader at startup or use [`AgentHarness::replace_skills`] directly.
    #[error("reload_skills_fn was not configured at harness construction")]
    NotConfigured,
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
    /// Embedder-supplied skill catalog loader. See [`AgentHarnessOptions::reload_skills_fn`]
    /// for ownership of source directories + dedup policy.
    reload_skills_fn: Option<ReloadSkillsFn>,
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
    /// Optional trigger prompt decision hook. See [`AgentHarnessOptions::on_trigger_prompt`].
    on_trigger_prompt: Option<OnTriggerPromptHook>,
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
    /// Optional turn-completion hook. See [`OnTurnEndHook`]. `None` keeps the legacy
    /// "one prompt cycle per call" behavior.
    on_turn_end: Option<OnTurnEndHook>,
    /// Resolved continuation cap — defaults to [`DEFAULT_TURN_CONTINUATION_CAP`] when
    /// `AgentHarnessOptions::turn_continuation_cap` is `None`.
    turn_continuation_cap: u32,
    /// Cancellation token for the currently-running `OnTurnEndHook` future, when one
    /// is in flight. Wired so [`Self::abort`] cancels the hook (e.g. an evaluator
    /// sub-agent call) the same way it cancels the inner agent loop.
    active_hook_cancel: Mutex<Option<tokio_util::sync::CancellationToken>>,
}

/// Internal record kept under `AgentHarness::running_triggers`. The public-facing snapshot
/// type is [`RunningTriggerState`]; this struct adds the cancel token used by the abort APIs.
struct RunningTriggerHandle {
    state: RunningTriggerState,
    cancel: tokio_util::sync::CancellationToken,
}

struct ResolvedTriggerPrompt {
    request: TriggerPromptRequest,
    decision: TriggerPromptDecision,
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
            on_control_plane_prompt: options.on_control_plane_prompt.clone(),
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
            reload_skills_fn: options.reload_skills_fn,
            trigger_runtime: TriggerRuntime::with_config(options.trigger_runtime),
            notification_hooks: Arc::new(Mutex::new(Vec::new())),
            before_trigger: options.before_trigger,
            on_trigger_prompt: options.on_trigger_prompt,
            before_trigger_action: options.before_trigger_action,
            before_tool_call: options.before_tool_call,
            after_tool_call: options.after_tool_call,
            running_triggers: Arc::new(Mutex::new(std::collections::HashMap::new())),
            on_turn_end: options.on_turn_end,
            turn_continuation_cap: options
                .turn_continuation_cap
                .unwrap_or(DEFAULT_TURN_CONTINUATION_CAP),
            active_hook_cancel: Mutex::new(None),
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
                    BeforeTriggerDecision::Prompt { reason } => {
                        let resolved = self.resolve_trigger_prompt(&trigger, reason).await;
                        let state = match resolved.decision {
                            TriggerPromptDecision::Allow => TriggerState::Accepted,
                            TriggerPromptDecision::Deny { .. }
                            | TriggerPromptDecision::Timeout { .. } => TriggerState::NeedsApproval,
                        };
                        (
                            state,
                            Some(serde_json::json!({
                                "outcome": "accept",
                                "permission": "prompt",
                                "trigger_prompt_id": resolved.request.trigger_prompt_id,
                                "prompt_decision": resolved.decision.as_audit_str(),
                                "reason": resolved.request.reason,
                                "decision_reason": resolved.decision.reason(),
                            })),
                        )
                    }
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
        let parent_agent = Arc::clone(&self.agent);
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
                parent_agent,
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

    async fn resolve_trigger_prompt(
        &self,
        trigger: &Trigger,
        reason: String,
    ) -> ResolvedTriggerPrompt {
        let request = build_trigger_prompt_request(trigger, reason);

        self.emit_harness_event(HarnessEvent::TriggerPromptRequest {
            request: request.clone(),
        });

        let decision = match self.on_trigger_prompt.clone() {
            Some(hook) => {
                let cancel = tokio_util::sync::CancellationToken::new();
                *self.active_hook_cancel.lock() = Some(cancel.clone());
                let decision = hook(request.clone(), cancel).await;
                *self.active_hook_cancel.lock() = None;
                decision
            }
            None => TriggerPromptDecision::Deny {
                reason: Some(
                    "trigger prompt required but no on_trigger_prompt hook configured \
                     (fail-closed deny — see issue #110 design v0.2)"
                        .to_string(),
                ),
            },
        };

        self.write_trigger_prompt_audit(&request, &decision).await;
        ResolvedTriggerPrompt { request, decision }
    }

    async fn write_trigger_prompt_audit(
        &self,
        request: &TriggerPromptRequest,
        decision: &TriggerPromptDecision,
    ) {
        let data = serde_json::json!({
            "schema_version": 1,
            "trigger_prompt_id": request.trigger_prompt_id,
            "trace_id": request.trace_id,
            "source_label": cap_control_plane_audit_label(&request.source_label),
            "receiver_agent_id": request.receiver_agent_id,
            "sender_agent_id": request.sender_agent_id,
            "action_class": request.action_class,
            "decision": decision.as_audit_str(),
            "reason": decision.reason(),
            "at": chrono::Utc::now().to_rfc3339(),
        });

        if let Err(e) = self
            .session
            .append_custom("trigger_prompt", Some(data))
            .await
        {
            self.emit_harness_event(HarnessEvent::PersistenceError {
                context: "trigger_prompt".into(),
                message: format!("trigger prompt audit append failed: {:?}", e.code),
            });
        }
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

    /// Hot-reload the skill catalog from disk via the embedder-supplied
    /// [`AgentHarnessOptions::reload_skills_fn`] closure. Used by `InstallSkillTool`,
    /// `/skills reload`, and any future control-plane that needs to refresh the catalog
    /// after a filesystem write — they all share the same source directories + dedup
    /// policy as startup because they go through the same closure.
    ///
    /// Returns the loader's [`super::skills::LoadSkillsOutput`] (skills + per-skill
    /// diagnostics) so the caller can surface a summary to the user. On success the new
    /// catalog has already been applied via [`Self::replace_skills`] and the system prompt
    /// rebuilt — the next prompt will see the new `<skills>` block. In-flight turns
    /// continue against their existing context (no mid-turn prompt mutation).
    ///
    /// Errors with [`ReloadSkillsError::NotConfigured`] if no loader was wired at
    /// construction — embedders that don't need reload simply leave `reload_skills_fn` as
    /// `None` and use [`Self::replace_skills`] directly.
    pub async fn reload_skills_from_disk(
        &self,
    ) -> Result<super::skills::LoadSkillsOutput, ReloadSkillsError> {
        let loader = self
            .reload_skills_fn
            .as_ref()
            .ok_or(ReloadSkillsError::NotConfigured)?
            .clone();
        let out = loader().await;
        self.replace_skills(out.skills.clone());
        Ok(out)
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
        // If an `OnTurnEndHook` future is currently in flight (typically waiting on an
        // evaluator sub-agent), cancel it too so Ctrl-C / `/cancel` interrupts the
        // entire prompt+continuation pipeline, not just the inner agent loop.
        if let Some(token) = self.active_hook_cancel.lock().as_ref() {
            token.cancel();
        }
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
        self.check_budget_cap()?;
        // Run compaction if we've crossed the threshold. This must happen before the user
        // message is appended so the cut point doesn't risk splitting the current turn.
        self.run_auto_compaction().await?;

        // First iteration runs `agent.prompt(msg)` with the caller's user message; any
        // `TurnEndAction::Continue` follow-up runs `agent.prompt(<new user msg>)` with the
        // text the hook returned. `run_turn_with_continuation` handles the hook loop,
        // persistence listener wiring, audit emission, and continuation cap enforcement.
        let last_user_prompt = extract_user_prompt_text(&msg);
        self.run_turn_with_continuation(Some(msg), last_user_prompt)
            .await
    }

    pub async fn continue_(&self) -> Result<(), AgentRunError> {
        self.ensure_session_start_emitted();
        self.check_budget_cap()?;
        self.run_auto_compaction().await?;

        // `continue_` runs `agent.continue_()` on the first iteration (no new user
        // message), and falls back to `agent.prompt(<hook text>)` on continuations
        // exactly like the `prompt_with_message` path.
        let last_user_prompt = self.last_user_text_from_state();
        self.run_turn_with_continuation(None, last_user_prompt)
            .await
    }

    /// Common driver for one prompt cycle plus zero or more `OnTurnEndHook`-driven
    /// continuation cycles. `first_msg = Some(_)` triggers `agent.prompt(msg)` on the
    /// first iteration; `None` triggers `agent.continue_()` (used by the public
    /// [`Self::continue_`] entry). Subsequent iterations always go through
    /// `agent.prompt(<user msg built from hook text>)`.
    async fn run_turn_with_continuation(
        &self,
        first_msg: Option<AgentMessage>,
        last_user_prompt: Option<String>,
    ) -> Result<(), AgentRunError> {
        let mut continuation_count: u32 = 0;
        let mut pending_user_msg = first_msg;
        let mut is_first_iteration = true;
        let mut last_user_prompt = last_user_prompt;

        loop {
            let (listener, persist_errors) = make_session_listener(self.session.clone());
            let unsub = self.agent.subscribe(listener);
            let result = if is_first_iteration {
                match pending_user_msg.take() {
                    Some(msg) => self.agent.prompt(msg).await,
                    None => self.agent.continue_().await,
                }
            } else {
                // Continuation: every iteration after the first runs as a fresh prompt.
                let msg = pending_user_msg.take().expect(
                    "continuation iteration must have a pending user message from the hook",
                );
                self.agent.prompt(msg).await
            };
            unsub();
            finish_persisted_run(result, persist_errors)?;
            is_first_iteration = false;

            // No hook configured → behave like the legacy single-cycle path. Skip event
            // and audit emission so embedders that never opt in pay zero overhead and
            // see no schema change in their session jsonl.
            let Some(hook) = self.on_turn_end.clone() else {
                return Ok(());
            };

            // Cap enforcement: if the previous iteration was already a continuation and
            // the cap is exhausted, record `budget_limited` and stop without invoking
            // the hook again. Counted on `continuation_count`, not the loop iteration
            // count, so the initial turn never counts against the cap.
            if continuation_count >= self.turn_continuation_cap {
                let reason = format!(
                    "continuation cap reached: {} >= {}",
                    continuation_count, self.turn_continuation_cap
                );
                self.record_turn_end_decision(
                    "budget_limited",
                    continuation_count,
                    Some(reason.clone()),
                    None,
                    None,
                )
                .await;
                return Ok(());
            }

            // Snapshot transcript outside the hook future so the parking_lot guard is
            // released before any `.await`. The hook is responsible for trimming.
            let transcript_snapshot = self.agent.state().messages.clone();
            let ctx = OnTurnEndContext {
                transcript: transcript_snapshot,
                continuation_count,
                last_user_prompt: last_user_prompt.clone(),
            };

            // Wire a cancel token to harness.abort() for the duration of the hook
            // future. Released in all exit paths below so abort() does not see stale
            // tokens between turns.
            let cancel = tokio_util::sync::CancellationToken::new();
            *self.active_hook_cancel.lock() = Some(cancel.clone());
            let decision = hook(ctx, cancel).await;
            *self.active_hook_cancel.lock() = None;

            match decision.action {
                TurnEndAction::Noop => {
                    // Hook deliberately recused itself — behave as if no hook were
                    // configured: no audit, no event. Lets long-lived hooks (e.g.
                    // `/goal`'s permanent registration) stay quiet on every plain
                    // turn that doesn't have an active goal.
                    return Ok(());
                }
                TurnEndAction::Stop => {
                    self.record_turn_end_decision(
                        "stop",
                        continuation_count,
                        None,
                        None,
                        decision.payload,
                    )
                    .await;
                    return Ok(());
                }
                TurnEndAction::Pause { reason } => {
                    self.record_turn_end_decision(
                        "pause",
                        continuation_count,
                        Some(reason),
                        None,
                        decision.payload,
                    )
                    .await;
                    return Ok(());
                }
                TurnEndAction::Continue { prompt } => {
                    continuation_count = continuation_count.saturating_add(1);
                    let preview = Some(preview_for_banner(&prompt, 80));
                    self.record_turn_end_decision(
                        "continue",
                        continuation_count,
                        None,
                        preview,
                        decision.payload,
                    )
                    .await;
                    // Build the follow-up user message and loop. Re-check the budget cap
                    // before each continuation iteration so a `Continue` decision cannot
                    // bypass a tripped cap. Compaction also runs again because the
                    // previous turn may have grown the transcript past the threshold.
                    self.check_budget_cap()?;
                    self.run_auto_compaction().await?;
                    let user_msg = AgentMessage::Llm(PiMessage::User(pie_ai::UserMessage {
                        role: pie_ai::UserRole::User,
                        content: pie_ai::UserContent::Text(prompt.clone()),
                        timestamp: chrono::Utc::now().timestamp_millis(),
                    }));
                    last_user_prompt = Some(prompt);
                    pending_user_msg = Some(user_msg);
                }
            }
        }
    }

    /// Shared budget-cap precondition used by every entry path
    /// (`prompt` / `prompt_with_images` / `continue_` / continuation iterations).
    fn check_budget_cap(&self) -> Result<(), AgentRunError> {
        if let Some(cap) = self.budget_cap_usd {
            let total = self.cost.snapshot().tokens.cost.total;
            if total >= cap {
                return Err(AgentRunError::Other(format!(
                    "budget cap reached: ${total:.4} >= ${cap:.4}. Reset with /cost reset or raise budget_cap_usd.",
                )));
            }
        }
        Ok(())
    }

    /// Walk the current agent transcript in reverse and return the text of the most
    /// recent `Message::User` with text content, if any. Used by `continue_` to fill
    /// `OnTurnEndContext::last_user_prompt` so evaluators don't need to re-scan.
    fn last_user_text_from_state(&self) -> Option<String> {
        let state = self.agent.state();
        state.messages.iter().rev().find_map(|m| match m {
            AgentMessage::Llm(PiMessage::User(u)) => extract_user_message_text(u),
            _ => None,
        })
    }

    /// Persist a `turn_end_decision` audit entry and emit the matching
    /// [`HarnessEvent::TurnEnded`] event. Best-effort: persistence failures do not
    /// abort the surrounding prompt cycle (the event still fires so observers can
    /// flag the lost audit), matching the trigger audit reflux pattern.
    async fn record_turn_end_decision(
        &self,
        decision: &'static str,
        continuation_count: u32,
        reason: Option<String>,
        next_prompt_preview: Option<String>,
        payload: Option<serde_json::Value>,
    ) {
        let data = serde_json::json!({
            "decision": decision,
            "continuation_count": continuation_count,
            "reason": reason,
            "next_prompt_preview": next_prompt_preview,
            "payload": payload.unwrap_or(serde_json::Value::Null),
        });
        if let Err(e) = self
            .session
            .append_custom("turn_end_decision", Some(data))
            .await
        {
            self.emit_harness_event(HarnessEvent::PersistenceError {
                context: "turn_end_decision".into(),
                message: format!("turn_end_decision append failed: {:?}", e.code),
            });
        }
        self.emit_harness_event(HarnessEvent::TurnEnded {
            decision,
            continuation_count,
            reason,
            next_prompt_preview,
        });
    }

    /// Force a compaction immediately, regardless of token thresholds. Useful for `/compact`-
    /// style slash commands.
    pub async fn force_compact(
        &self,
        custom_instructions: Option<String>,
    ) -> Result<bool, AgentRunError> {
        self.do_compact(true, custom_instructions).await
    }

    /// Run a tool-less, in-memory evaluator sub-agent and return its last assistant
    /// text. Used by `OnTurnEndHook` implementations (e.g. the `/goal` stop hook) that
    /// need to ask a separate model "is the user's goal met by this transcript?"
    /// without contaminating the parent session, cost tracker, or audit log.
    ///
    /// Behavior:
    /// - The sub-agent has `tools: []`. The evaluator is a judge, not an actor — it
    ///   must never invoke a tool, even if the embedder accidentally leaves tool
    ///   hooks on. Mirrors the `disable_model_invocation` posture for evaluators.
    /// - The sub-agent uses [`MemorySessionStorage`] so its conversation is discarded
    ///   when this call returns. No `--resume` artifact is created.
    /// - `system_prompt` and `user_prompt` are passed verbatim — the caller owns the
    ///   evaluator's JSON-output contract.
    /// - `cancel` is honored on the sub-agent's `Agent::abort()` path: the call
    ///   returns [`EvaluatorError::Cancelled`] if the token is tripped (typically by
    ///   the surrounding hook getting `cancel.cancelled()` from
    ///   [`AgentHarness::abort`]).
    /// - Cost is **not** attributed to the parent `CostTracker`. The evaluator runs
    ///   on a bare `Agent` without a tracker subscriber — same honesty rule the
    ///   `trigger_result.cost_usd: null` audit follows. Embedders that need cost
    ///   accounting on evaluators should subscribe their own listener.
    pub async fn run_evaluator(
        &self,
        system_prompt: String,
        user_prompt: String,
        model: Model,
        thinking_level: ThinkingLevel,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<EvaluatorOutput, EvaluatorError> {
        let mut state = AgentState::default();
        state.model = Some(model);
        state.thinking_level = Some(thinking_level);
        state.tools = Vec::new();
        state.system_prompt = system_prompt;

        let eval_agent = Agent::new(AgentOptions {
            initial_state: Some(state),
            stream_fn: self.stream_fn.clone(),
            // Intentionally no before/after_tool_call hooks — evaluator has no tools.
            ..Default::default()
        });

        let user_message = AgentMessage::Llm(PiMessage::User(pie_ai::UserMessage {
            role: pie_ai::UserRole::User,
            content: pie_ai::UserContent::Text(user_prompt),
            timestamp: chrono::Utc::now().timestamp_millis(),
        }));

        let run_outcome: Result<(), AgentRunError> = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                eval_agent.abort();
                return Err(EvaluatorError::Cancelled);
            }
            res = eval_agent.prompt(user_message) => res,
        };
        run_outcome?;

        let last_assistant_text = {
            let state = eval_agent.state();
            last_assistant_text(&state)
        };
        Ok(EvaluatorOutput {
            last_assistant_text,
        })
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
            match event {
                AgentEvent::MessageEnd { message } => {
                    if let Err(e) = session.append_message(message).await {
                        listener_errors.lock().push(e);
                    }
                }
                AgentEvent::ControlPlanePromptResolved {
                    tool_call_id,
                    tool_name,
                    args_hash,
                    label,
                    decision,
                    reason,
                } => {
                    // Issue #110 design v0.2 Artifact E: write a `control_plane_prompt`
                    // Custom audit per resolution. Label is capped at 200 chars
                    // (cap-inclusive on char boundary) so a hook-supplied unbounded
                    // string cannot grow the audit / `--resume` body without limit
                    // — per @QA-Release-Lead non-blocking note on PR #135.
                    let data = serde_json::json!({
                        "schema_version": 1,
                        "tool_call_id": tool_call_id,
                        "tool_name": tool_name,
                        "args_hash": args_hash,
                        "label": cap_control_plane_audit_label(&label),
                        "decision": decision,
                        "reason": reason,
                        "at": chrono::Utc::now().to_rfc3339(),
                    });
                    if let Err(e) = session
                        .append_custom("control_plane_prompt", Some(data))
                        .await
                    {
                        listener_errors.lock().push(e);
                    }
                }
                _ => {}
            }
        })
    });
    (listener, errors)
}

/// Cap rule for `control_plane_prompt.data.label`. Hook-supplied labels MUST be
/// bounded before persistence to prevent an embedder hook from inflating audit /
/// `--resume` body size. Per @QA-Release-Lead non-blocking note on PR #135.
///
/// Caps at 200 chars, cap-inclusive on char boundary (same shape as RFC 1 sub-PR 5a's
/// 4 KiB summary cap — character-walked, not byte-walked, so multi-byte chars don't
/// land mid-rune).
const CONTROL_PLANE_PROMPT_LABEL_CAP_CHARS: usize = 200;

fn cap_control_plane_audit_label(label: &str) -> String {
    if label.chars().count() <= CONTROL_PLANE_PROMPT_LABEL_CAP_CHARS {
        return label.to_string();
    }
    let mut out: String = label
        .chars()
        .take(CONTROL_PLANE_PROMPT_LABEL_CAP_CHARS.saturating_sub(1))
        .collect();
    out.push('…');
    out
}

fn build_trigger_prompt_request(trigger: &Trigger, reason: String) -> TriggerPromptRequest {
    let receiver_agent_id = validated_payload_agent_id(trigger, &["receiver_agent_id"])
        .or_else(|| validated_payload_agent_id(trigger, &["_meta", "receiver_agent_id"]));
    let sender_agent_id = validated_payload_agent_id(trigger, &["sender_agent_id"])
        .or_else(|| validated_payload_agent_id(trigger, &["_meta", "sender_agent_id"]))
        .or_else(|| validated_payload_agent_id(trigger, &["agent_id"]))
        .unwrap_or_else(|| cap_control_plane_audit_label(&trigger.authority.principal_id));
    let action_class = validated_payload_action_class(trigger, &["action_class"])
        .or_else(|| validated_payload_action_class(trigger, &["_meta", "action_class"]))
        .unwrap_or_else(|| cap_control_plane_audit_label(&trigger.event_label));
    let trigger_summary = trigger
        .payload_summary
        .clone()
        .map(|summary| truncate_on_char_boundary(summary, PROMOTION_BODY_CAP_BYTES).0);
    let payload = serde_json::json!({
        "source_kind": trigger.source_kind,
        "source_label": cap_control_plane_audit_label(&trigger.source_label),
        "event_label": cap_control_plane_audit_label(&trigger.event_label),
        "payload_visibility": trigger.payload_visibility,
        "payload_summary": trigger_summary,
        "authority": {
            "principal_id": trigger.authority.principal_id.clone(),
            "principal_label": cap_control_plane_audit_label(&trigger.authority.principal_label),
            "credential_scope": trigger.authority.credential_scope,
            "allowed_source_actions": trigger.authority.allowed_source_actions.clone(),
        }
    });
    let binding = serde_json::json!([
        "trigger_prompt:v1",
        trigger.idempotency_key.clone(),
        trigger.trace_id.clone(),
        trigger.source_kind,
        trigger.source_label.clone(),
        trigger.event_label.clone(),
        receiver_agent_id.clone(),
        sender_agent_id.clone(),
        action_class.clone(),
    ]);
    let trigger_prompt_id = sha256_hex(&binding.to_string());
    TriggerPromptRequest {
        trigger_prompt_id,
        trace_id: trigger.trace_id.clone(),
        source_label: cap_control_plane_audit_label(&trigger.source_label),
        receiver_agent_id,
        sender_agent_id,
        action_class,
        trigger_summary,
        payload,
        reason: cap_trigger_prompt_reason(&reason),
    }
}

fn validated_payload_agent_id(trigger: &Trigger, path: &[&str]) -> Option<String> {
    let value = trigger_json_string(trigger, path)?;
    uuid::Uuid::parse_str(&value).ok()?;
    Some(value)
}

fn validated_payload_action_class(trigger: &Trigger, path: &[&str]) -> Option<String> {
    let value = trigger_json_string(trigger, path)?;
    is_valid_action_class(&value).then_some(value)
}

fn trigger_json_string(trigger: &Trigger, path: &[&str]) -> Option<String> {
    let mut value = trigger.payload.as_ref()?;
    for key in path {
        value = value.get(*key)?;
    }
    value.as_str().map(str::to_string)
}

fn is_valid_action_class(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("sk-") || lower.contains("bearer") || lower.contains("token") {
        return false;
    }
    value.len() <= 64
        && first.is_ascii_lowercase()
        && chars.all(|ch| {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '_' | '-' | '.' | ':')
        })
}

const TRIGGER_PROMPT_REASON_CAP_CHARS: usize = 512;

fn cap_trigger_prompt_reason(reason: &str) -> String {
    if reason.chars().count() <= TRIGGER_PROMPT_REASON_CAP_CHARS {
        return reason.to_string();
    }
    let mut out: String = reason
        .chars()
        .take(TRIGGER_PROMPT_REASON_CAP_CHARS.saturating_sub(1))
        .collect();
    out.push('…');
    out
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
/// 3. Build the sub-agent's `Agent` on an in-memory session, inheriting the parent model,
///    system prompt, tools, thinking level, and tool hooks. It does not inherit the parent
///    conversation messages unless a later promotion writes trigger output back.
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
    parent_agent: Arc<Agent>,
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

    // 1b. Direct-inject delivery. Skip the sub-agent entirely and promote
    // `trigger.payload_summary` straight into the parent loop via `apply_promotion`. No
    // model call, no tools, cost is a real 0.0. The kernel stays domain-agnostic — it only
    // moves the opaque summary string and never learns what the source is. We still emit the
    // ExecutionStarted/Completed pair and a `trigger_result` audit (with `message_count: 0`
    // distinguishing it from a sub-agent run) so `/triggers` and jsonl readers see a normal
    // terminal lifecycle.
    if action.delivery == TriggerDelivery::InjectSummary {
        let summary = trigger.payload_summary.clone();
        emit_from_listeners(
            &listeners,
            HarnessEvent::TriggerExecutionStarted {
                trace_id: trace_id.clone(),
                source_label: source_label.clone(),
                event_label: event_label.clone(),
                prompt_preview: preview_for_banner(
                    summary.as_deref().unwrap_or("(no summary)"),
                    80,
                ),
            },
        );
        let result_data = serde_json::json!({
            "trace_id": trace_id,
            "branch_id": serde_json::Value::Null,
            "success": true,
            "summary": summary,
            "message_count": 0,
            // Honest measurement: an inject performs no model call, unlike the sub-agent
            // path which reports `null` because its bare `Agent` has no CostTracker.
            "cost_usd": 0.0,
            "reason": serde_json::Value::Null,
            "details": serde_json::Value::Null,
            "delivery": "inject_summary",
        });
        if let Err(e) = parent_session
            .append_custom("trigger_result", Some(result_data))
            .await
        {
            emit_from_listeners(
                &listeners,
                HarnessEvent::PersistenceError {
                    context: "trigger_result".into(),
                    message: format!("trigger_result (inject) append failed: {:?}", e.code),
                },
            );
        }
        emit_from_listeners(
            &listeners,
            HarnessEvent::TriggerCompleted {
                trace_id: trace_id.clone(),
                summary: summary.clone(),
                cost_usd: Some(0.0),
                details: serde_json::Value::Null,
            },
        );
        // Reuse the full promotion machinery: prefix enforcement, streaming/idle injection,
        // dedup, and the `trigger_promotion` audit. `summary` carries the payload summary, so
        // a `{{trigger.payload_summary}}` (or `{{result.summary}}`) template renders it.
        apply_promotion(
            &listeners,
            &parent_session,
            &parent_agent,
            &trace_id,
            &trigger,
            true,
            &summary,
            0,
            None,
            &action.promote,
            action.promote_requires_approval,
            &serde_json::Value::Null,
        )
        .await;
        return;
    }

    // 1c. Inject-and-run delivery. Inject `action.prompt` (a user-rule instruction carrying
    // whatever source context the rule chose) into the PARENT conversation, then arrange for
    // ONE model turn in the parent's full context. The kernel never runs the single-tenant
    // parent agent from this detached task:
    //   * streaming → enqueue a follow-up; the in-flight loop runs it at the next boundary.
    //   * idle      → append the message + emit `TriggerRequestsMainRun`; the embedder (which
    //                 owns the parent agent) schedules the turn on its own serialized loop.
    // The model turn itself is a normal parent-loop event, NOT attributed to this
    // `trigger_result` (whose `message_count` stays 0 — this action only injects + requests).
    if action.delivery == TriggerDelivery::InjectAndRun {
        let (body, _truncated) =
            truncate_on_char_boundary(action.prompt.clone(), PROMOTION_BODY_CAP_BYTES);
        // Same engine-enforced `[Trigger <id>] ` prefix as promotion, so an injected
        // instruction is never indistinguishable from human input.
        let (body, prefix_injected) = ensure_trigger_prefix(body, &trace_id);
        emit_from_listeners(
            &listeners,
            HarnessEvent::TriggerExecutionStarted {
                trace_id: trace_id.clone(),
                source_label: source_label.clone(),
                event_label: event_label.clone(),
                prompt_preview: preview_for_banner(&body, 80),
            },
        );

        let user_message = AgentMessage::Llm(PiMessage::User(pie_ai::UserMessage {
            role: pie_ai::UserRole::User,
            content: pie_ai::UserContent::Text(body.clone()),
            timestamp: chrono::Utc::now().timestamp_millis(),
        }));

        // Inject. Mirror `apply_promotion`'s two-branch persistence so the message lands in
        // the jsonl exactly once and in the right order relative to any in-flight turn.
        let queued_for_followup = parent_agent.is_streaming();
        if queued_for_followup {
            parent_agent.enqueue_follow_up(user_message);
        } else if let Err(e) = parent_session.append_message(user_message.clone()).await {
            emit_from_listeners(
                &listeners,
                HarnessEvent::PersistenceError {
                    context: "trigger_inject_and_run".into(),
                    message: format!("inject_and_run append failed: {:?}", e.code),
                },
            );
        } else {
            parent_agent.state().messages.push(user_message);
        }

        let result_data = serde_json::json!({
            "trace_id": trace_id,
            "branch_id": serde_json::Value::Null,
            "success": true,
            "summary": body,
            "message_count": 0,
            "cost_usd": 0.0,
            "reason": serde_json::Value::Null,
            "details": serde_json::Value::Null,
            "delivery": "inject_and_run",
            "prefix_injected": prefix_injected,
            "run_dispatch": if queued_for_followup { "follow_up" } else { "main_run_request" },
        });
        if let Err(e) = parent_session
            .append_custom("trigger_result", Some(result_data))
            .await
        {
            emit_from_listeners(
                &listeners,
                HarnessEvent::PersistenceError {
                    context: "trigger_result".into(),
                    message: format!(
                        "trigger_result (inject_and_run) append failed: {:?}",
                        e.code
                    ),
                },
            );
        }

        emit_from_listeners(
            &listeners,
            HarnessEvent::TriggerCompleted {
                trace_id: trace_id.clone(),
                summary: Some(body),
                cost_usd: Some(0.0),
                details: serde_json::Value::Null,
            },
        );

        // Idle parent: no in-flight loop to drain the follow-up, so ask the embedder to run
        // one turn. Streaming parent already has the follow-up queued.
        if !queued_for_followup {
            emit_from_listeners(
                &listeners,
                HarnessEvent::TriggerRequestsMainRun {
                    trace_id: trace_id.clone(),
                },
            );
        }
        return;
    }

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
            source_label: source_label.clone(),
            event_label: event_label.clone(),
            prompt_preview,
        },
    );

    // 3. Build sub-agent. It receives the parent's already-rendered system prompt, tool
    // list, and hooks. That means model-facing skill catalog text and the live Skill tool
    // remain available to trigger actions, but parent conversation messages are not copied
    // into the trigger run. In sub-PR 5a the sub-agent transcript lives in memory only and
    // is discarded when this task finishes. Per the issue #20 amendment, jsonl-backed
    // retained branches land in sub-PR 5c. The `trigger_result.summary` we persist to the
    // parent session is the only durable record of what the sub-agent produced in 5a.
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
    //
    // `details` is the structured sub-agent result envelope per RFC 1 §5.C: marker tools
    // (`mark_dynamic_rule_matched` and future per-source equivalents) write through the
    // [`TriggerResultDetailsBuilder`] accumulator while the sub-agent runs; runtime
    // snapshots the builder here. Until callers wire a builder into the sub-agent, this is
    // `Null` and any `PromoteAction::PromoteSummaryWhenResultDetailsMatch` evaluation
    // fails closed with `PromotionConditionSkipReason::PointerMissing` — the safe default.
    let details_for_promotion: serde_json::Value = serde_json::Value::Null;
    let result_data = serde_json::json!({
        "trace_id": trace_id,
        "branch_id": serde_json::Value::Null,
        "success": success,
        "summary": summary,
        "message_count": message_count,
        "cost_usd": serde_json::Value::Null,
        "reason": failure_reason,
        "details": details_for_promotion,
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
                // Resolution after 5a merge: HEAD (main) has cost_usd: Option<f64> = None
                // per CLI-TUI review (3845107). 5b needs summary.clone() because the
                // promotion step below consumes `summary` by reference. Combine both.
                summary: summary.clone(),
                cost_usd: None,
                details: details_for_promotion.clone(),
            },
        );
    } else {
        emit_from_listeners(
            &listeners,
            HarnessEvent::TriggerFailed {
                trace_id: trace_id.clone(),
                reason: failure_reason
                    .clone()
                    .unwrap_or_else(|| "unknown failure".to_string()),
            },
        );
    }

    // 7b. Promotion. RFC 1 §5.C: `PromoteAction` decides whether (and how) the
    // `trigger_result` is mirrored back into the parent transcript / LLM context. Runs
    // AFTER the terminal `TriggerCompleted | TriggerFailed` so the event order pinned in
    // RFC 1 §5.F holds. Promotion outcomes are themselves emitted + audited as
    // `TriggerPromoted | PromotionPending` + `Custom { custom_type: "trigger_promotion" }`.
    apply_promotion(
        &listeners,
        &parent_session,
        &parent_agent,
        &trace_id,
        &trigger,
        success,
        &summary,
        message_count,
        failure_reason.as_deref(),
        &action.promote,
        action.promote_requires_approval,
        // Sub-agent result details. Populated via marker tools that write through the
        // [`TriggerResultDetailsBuilder`] accumulator (sub-PR for marker-tool wiring lands
        // separately). Until that wires in, this stays `Null` and any caller using
        // `PromoteAction::PromoteSummaryWhenResultDetailsMatch` will fail closed with
        // `PromotionConditionSkipReason::PointerMissing` — the safe default.
        &details_for_promotion,
    )
    .await;

    // 8. Remove from registry.
    running_registry.lock().remove(&trace_id);
}

/// Inputs allowlisted for the promotion template per RFC 1 §5.C. Constructed once per
/// promotion and exposed to the renderer as a sealed map; references to anything not in
/// this set fail the render (fail-closed).
fn build_template_context(
    trace_id: &str,
    trigger: &Trigger,
    success: bool,
    summary: &Option<String>,
    message_count: usize,
) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;
    let mut ctx: HashMap<String, String> = HashMap::new();
    ctx.insert("trace_id".into(), trace_id.to_string());
    let (source_kind_str, source_server, source_method, source_topic, source_subkind) =
        match &trigger.source {
            super::trigger::TriggerSource::Mcp {
                server_name,
                method,
            } => (
                "mcp".to_string(),
                Some(server_name.clone()),
                Some(method.clone()),
                None,
                None,
            ),
            super::trigger::TriggerSource::Hub { topic } => {
                ("hub".to_string(), None, None, Some(topic.clone()), None)
            }
            super::trigger::TriggerSource::Local { subkind } => {
                ("local".to_string(), None, None, None, Some(subkind.clone()))
            }
            super::trigger::TriggerSource::AgentDelegate { .. } => {
                ("agent_delegate".to_string(), None, None, None, None)
            }
        };
    ctx.insert("trigger.source.kind".into(), source_kind_str);
    if let Some(v) = source_server {
        ctx.insert("trigger.source.server_name".into(), v);
    }
    if let Some(v) = source_method {
        ctx.insert("trigger.source.method".into(), v);
    }
    if let Some(v) = source_topic {
        ctx.insert("trigger.source.topic".into(), v);
    }
    if let Some(v) = source_subkind {
        ctx.insert("trigger.source.subkind".into(), v);
    }
    ctx.insert("trigger.source_label".into(), trigger.source_label.clone());
    ctx.insert("trigger.event_label".into(), trigger.event_label.clone());
    if let Some(s) = &trigger.payload_summary {
        ctx.insert("trigger.payload_summary".into(), s.clone());
    } else {
        ctx.insert("trigger.payload_summary".into(), String::new());
    }
    ctx.insert(
        "trigger.received_at".into(),
        trigger.received_at.to_rfc3339(),
    );
    ctx.insert(
        "trigger.idempotency_key".into(),
        trigger.idempotency_key.clone(),
    );
    ctx.insert(
        "trigger.authority.principal_id".into(),
        trigger.authority.principal_id.clone(),
    );
    ctx.insert(
        "trigger.authority.principal_label".into(),
        trigger.authority.principal_label.clone(),
    );
    ctx.insert(
        "trigger.authority.credential_scope".into(),
        format!("{:?}", trigger.authority.credential_scope),
    );
    ctx.insert("result.summary".into(), summary.clone().unwrap_or_default());
    ctx.insert(
        "result.status".into(),
        if success { "success" } else { "failed" }.into(),
    );
    ctx.insert("result.message_count".into(), message_count.to_string());
    ctx.insert("result.cost_usd".into(), "null".into());
    ctx.insert("result.branch_id".into(), "null".into());
    ctx
}

/// Forbidden field references — referencing any of these via `{{name}}` in a promotion
/// template fails the render at validation time (independent of whether the field happens
/// to exist in the allowlist). RFC 1 §5.C: explicitly redacted boundary.
const FORBIDDEN_TEMPLATE_FIELDS: &[&str] = &[
    "trigger.payload",
    "trigger.authority.allowed_source_actions",
];

#[derive(Debug, PartialEq, Eq)]
enum TemplateRenderError {
    UnknownField(String),
    ForbiddenField(String),
}

/// Render a promotion template against the allowlisted context. Returns
/// `Err(TemplateRenderError::UnknownField | ForbiddenField)` on any unknown or forbidden
/// `{{...}}` reference (fail-closed; the caller must NOT insert anything on Err).
///
/// Whitespace inside `{{...}}` is tolerated (`{{ trace_id }}` works). `_meta.*` references
/// are treated as unknown (the only metadata channel adapters have today flows through
/// `trigger.payload_summary` per PR #56's privacy contract; bypassing that is forbidden).
fn render_promotion_template(
    body: &str,
    ctx: &std::collections::HashMap<String, String>,
) -> Result<String, TemplateRenderError> {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 2..];
        let close = after_open.find("}}").ok_or_else(|| {
            TemplateRenderError::UnknownField("unclosed `{{` placeholder".to_string())
        })?;
        let raw_name = &after_open[..close];
        let name = raw_name.trim();
        if FORBIDDEN_TEMPLATE_FIELDS.contains(&name) || name.starts_with("_meta") {
            return Err(TemplateRenderError::ForbiddenField(name.to_string()));
        }
        let value = ctx
            .get(name)
            .ok_or_else(|| TemplateRenderError::UnknownField(name.to_string()))?;
        out.push_str(value);
        rest = &after_open[close + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Built-in fallback template used when `PromoteSummaryNow { template: None }`.
const DEFAULT_PROMOTE_SUMMARY_TEMPLATE: &str = "[Trigger {{trace_id}}] {{trigger.source_label}} fired {{trigger.event_label}}.\nResult: {{result.summary}}";

/// Same byte cap used for `result.summary` truncation; applied to the rendered promotion
/// body so a runaway template (e.g. summary already at cap + verbose template body) cannot
/// inflate the parent transcript beyond the 4 KiB boundary per RFC 1 §5.B.
const PROMOTION_BODY_CAP_BYTES: usize = 4096;

/// Truncate a promotion body to the byte cap on a UTF-8 char boundary. Returns the new
/// string and `truncated: bool`. Walk-back ensures `truncate` never panics on a
/// multi-byte char.
/// Stable hex-encoded SHA-256 of the template body. Used only as a content fingerprint in
/// the `trigger_promotion` audit so RFC 4 rule edits / template version bumps are
/// detectable from JSONL log re-reads. Not used as a credential / authentication
/// primitive — see `sha2` dep comment in `Cargo.toml`.
fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let out = hasher.finalize();
    // Lowercase hex; the first 8 chars are sliced off by callers for the `inline:` name.
    let mut s = String::with_capacity(out.len() * 2);
    for byte in out.iter() {
        use std::fmt::Write;
        let _ = write!(&mut s, "{byte:02x}");
    }
    s
}

/// Enforce the `[Trigger {trace_id}] ` disambiguation prefix on a promotion body. Per
/// @Tools-MCP-Lead's PR #65 review: trusting template authors to include the prefix is
/// unsafe — a custom template that forgets it would produce a `Message::User` in the
/// parent transcript that looks like human input, polluting the next-turn LLM context
/// without user awareness. Idempotent only for the **current** trace id: if the body
/// already begins with `[Trigger {trace_id}] ` (the form the engine would produce), the
/// prefix is not re-added. A `[Trigger evil] ` prefix carrying a different trace id is
/// NOT trusted — the engine still prepends the real `[Trigger {trace_id}] ` so the
/// authoritative trace id wins. Returns `(prefixed_body, injected)`.
fn ensure_trigger_prefix(body: String, trace_id: &str) -> (String, bool) {
    let expected = format!("[Trigger {trace_id}] ");
    if body.starts_with(&expected) {
        (body, false)
    } else {
        (format!("{expected}{body}"), true)
    }
}

/// Truncation marker appended to bodies that overrun `cap_bytes`. Counted toward the cap
/// so the final string length is `<= cap_bytes`.
const TRUNCATION_MARKER: &str = "…[truncated]";

/// Truncate `body` to fit within `cap_bytes` *including* the truncation marker. The body
/// portion is cut on a UTF-8 char boundary so `truncate` never panics on a multi-byte
/// codepoint. The final length is at most `cap_bytes`: we reserve
/// `TRUNCATION_MARKER.len()` from the budget before the boundary walk.
fn truncate_on_char_boundary(body: String, cap_bytes: usize) -> (String, bool) {
    if body.len() <= cap_bytes {
        return (body, false);
    }
    // Reserve room for the marker so the final string fits the cap. If the cap is
    // somehow smaller than the marker, fall back to "marker-only" output.
    let budget = cap_bytes.saturating_sub(TRUNCATION_MARKER.len());
    let mut cut = budget.min(body.len());
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut truncated = body;
    truncated.truncate(cut);
    truncated.push_str(TRUNCATION_MARKER);
    (truncated, true)
}

/// Apply the trigger's [`PromoteAction`] after the sub-agent has finished and the
/// `trigger_result` audit was written. RFC 1 §5.C — implements the v1 promotion variants
/// `None` (no-op) and `PromoteSummaryNow { template }` (templated insertion into the
/// parent session; fail-closed on render error; pending state when
/// `promote_requires_approval = true`).
#[allow(clippy::too_many_arguments)]
async fn apply_promotion(
    listeners: &Arc<Mutex<Vec<HarnessListener>>>,
    parent_session: &Session,
    parent_agent: &Arc<Agent>,
    trace_id: &str,
    trigger: &Trigger,
    success: bool,
    summary: &Option<String>,
    message_count: usize,
    _failure_reason: Option<&str>,
    promote: &PromoteAction,
    require_approval: bool,
    details: &serde_json::Value,
) {
    // Extract the inline template body (if any). v1 does not look up named templates from
    // any registry; that lands in sub-PR 6 / RFC 4 rule engine work. The body is what we
    // render against — never persisted as `template_name` in the audit.
    let (template_body_arg, promote_kind): (Option<String>, &'static str) = match promote {
        PromoteAction::None => return, // most common path; nothing else to do
        PromoteAction::PromoteSummaryNow { template_body } => {
            (template_body.clone(), "promote_summary_now")
        }
        #[allow(deprecated)]
        PromoteAction::PromoteSummaryWhenSummaryContains {
            template_body,
            required_substrings,
        } => {
            let summary_text = summary.as_deref().unwrap_or_default();
            if !required_substrings
                .iter()
                .any(|needle| summary_text.contains(needle))
            {
                return;
            }
            (template_body.clone(), "promote_summary_now")
        }
        PromoteAction::PromoteSummaryWhenResultDetailsMatch {
            template_body,
            condition,
        } => {
            // Authorization gate. The sub-agent's `summary` is NEVER consulted — promotion
            // fires only when the structured `details` blob satisfies `condition`. Any
            // failure (pointer missing, value not an array, empty intersection) emits a
            // `trigger_promotion { state: "skipped", reason }` audit and returns without
            // touching the parent transcript.
            match condition.evaluate(details) {
                Ok(_matched) => (
                    template_body.clone(),
                    "promote_summary_when_result_details_match",
                ),
                Err(reason) => {
                    let audit_data = serde_json::json!({
                        "state": "skipped",
                        "trace_id": trace_id,
                        "promote_kind": "promote_summary_when_result_details_match",
                        "reason": reason.as_audit_str(),
                        "template_name": serde_json::Value::Null,
                        "template_hash": serde_json::Value::Null,
                        "inserted_entry_id": serde_json::Value::Null,
                        "rule_id": serde_json::Value::Null,
                        "redaction_status": "skipped",
                        "dedup_collapsed": false,
                        "prefix_injected": false,
                    });
                    let _ = parent_session
                        .append_custom("trigger_promotion", Some(audit_data))
                        .await;
                    return;
                }
            }
        }
    };

    // Build the sealed allowlisted template context once. Anything not in here is unknown
    // to the renderer; anything explicitly forbidden fails before substitution.
    let ctx = build_template_context(trace_id, trigger, success, summary, message_count);

    // Resolve the body to render: explicit if provided, otherwise the built-in default.
    // Both flow through the same renderer (per Provider/Auth: no fixed-summary insertion
    // path that bypasses sanitization).
    let body_template: &str = template_body_arg
        .as_deref()
        .unwrap_or(DEFAULT_PROMOTE_SUMMARY_TEMPLATE);

    // `template_name` / `template_hash` for audit + events: stable identifier + content
    // fingerprint per @Tools-MCP-Lead's PR #65 follow-up. v1 categories:
    // - `"default"` when no inline body was provided
    // - `"inline:{hash[..8]}"` when the hook supplied a literal body
    // - (future) `"rules.{rule_id}.template"` when RFC 4 rule engine names a template
    // Provider/Auth blocker: the raw body is NEVER stored as `template_name`.
    let template_hash = sha256_hex(body_template);
    let template_name = match &template_body_arg {
        None => "default".to_string(),
        Some(_) => format!("inline:{}", &template_hash[..8]),
    };
    let template_name = Some(template_name);
    let template_hash = Some(template_hash);

    let rendered = match render_promotion_template(body_template, &ctx) {
        Ok(s) => s,
        Err(err) => {
            // Render failure → fail-closed. Write a `trigger_promotion { state: "failed" }`
            // audit so jsonl-only readers can see what happened, and emit a
            // `PersistenceError` reflux so live subscribers know promotion was lost.
            let redaction_status = match &err {
                TemplateRenderError::UnknownField(_) => "render_error",
                TemplateRenderError::ForbiddenField(_) => "forbidden_field",
            };
            let err_msg = match &err {
                TemplateRenderError::UnknownField(name) => {
                    format!("unknown template field: {name}")
                }
                TemplateRenderError::ForbiddenField(name) => {
                    format!("forbidden template field: {name}")
                }
            };
            let audit_data = serde_json::json!({
                "state": "failed",
                "trace_id": trace_id,
                "promote_kind": promote_kind,
                "template_name": template_name,
                "template_hash": template_hash,
                "inserted_entry_id": serde_json::Value::Null,
                "rule_id": serde_json::Value::Null,
                "redaction_status": redaction_status,
                "dedup_collapsed": false,
                // Render failed before the prefix step ran; record false so the audit shape
                // stays uniform across all promotion states.
                "prefix_injected": false,
            });
            if let Err(e) = parent_session
                .append_custom("trigger_promotion", Some(audit_data))
                .await
            {
                emit_from_listeners(
                    listeners,
                    HarnessEvent::PersistenceError {
                        context: "trigger_promotion".into(),
                        message: format!("trigger_promotion (failed) append failed: {:?}", e.code),
                    },
                );
            }
            emit_from_listeners(
                listeners,
                HarnessEvent::PersistenceError {
                    context: "trigger_promotion".into(),
                    message: err_msg,
                },
            );
            return;
        }
    };

    // Per @Tools-MCP-Lead's PR #65 review: enforce the `[Trigger {trace_id}] ` prefix at
    // the engine level instead of trusting the template author to include it. A custom
    // template that forgets the prefix would otherwise produce a parent-session
    // `Message::User` that looks indistinguishable from human input, polluting the
    // next-turn LLM context without user awareness. Idempotent: if the rendered body
    // already starts with `[Trigger ` (e.g. the built-in default template), the prefix
    // is not added twice.
    let (rendered, prefix_injected) = ensure_trigger_prefix(rendered, trace_id);

    // Pending path: render succeeded so we have a preview, but `promote_requires_approval`
    // is true and there is no `/triggers approve` command in v1 — fail-closed-to-pending.
    if require_approval {
        let (preview, truncated) =
            truncate_on_char_boundary(rendered.clone(), PROMOTION_BODY_CAP_BYTES);
        let redaction_status = if truncated { "truncated" } else { "clean" };
        let audit_data = serde_json::json!({
            "state": "pending",
            "trace_id": trace_id,
            "promote_kind": promote_kind,
            "template_name": template_name,
            "template_hash": template_hash,
            "inserted_entry_id": serde_json::Value::Null,
            "rule_id": serde_json::Value::Null,
            "redaction_status": redaction_status,
            "dedup_collapsed": false,
            "prefix_injected": prefix_injected,
        });
        if let Err(e) = parent_session
            .append_custom("trigger_promotion", Some(audit_data))
            .await
        {
            emit_from_listeners(
                listeners,
                HarnessEvent::PersistenceError {
                    context: "trigger_promotion".into(),
                    message: format!("trigger_promotion (pending) append failed: {:?}", e.code),
                },
            );
        }
        emit_from_listeners(
            listeners,
            HarnessEvent::PromotionPending {
                trace_id: trace_id.to_string(),
                promote_kind: promote_kind.into(),
                template_name,
                preview: Some(preview),
            },
        );
        return;
    }

    // Success path: render OK, no approval gate → insert into parent transcript.
    // pie_ai has no `Message::System` role; use `Message::User` with the rendered body.
    // The engine-injected `[Trigger {trace_id}] ` prefix (above) guarantees the appended
    // entry is visually disambiguated from human input regardless of which template was
    // used.
    let (final_body, truncated) = truncate_on_char_boundary(rendered, PROMOTION_BODY_CAP_BYTES);
    let redaction_status = if truncated { "truncated" } else { "clean" };

    let user_message = AgentMessage::Llm(PiMessage::User(pie_ai::UserMessage {
        role: pie_ai::UserRole::User,
        content: pie_ai::UserContent::Text(final_body),
        timestamp: chrono::Utc::now().timestamp_millis(),
    }));

    // Single persistence path. The promoted message must land in the session JSONL exactly
    // once, with deterministic ordering relative to any in-flight assistant response. Two
    // disjoint branches based on parent loop state:
    //
    // - **Streaming**: parent has an active prompt. Hand the message to the loop's
    //   follow-up queue. The loop drains it at the next turn boundary (after the in-flight
    //   assistant response has emitted its `MessageEnd` and been persisted by the session
    //   listener), pushes it into `state.messages`, and emits a `MessageEnd` whose session
    //   listener writes the single canonical session entry. Order in JSONL: assistant
    //   response → user_promoted, matching what the model actually saw. We do NOT call
    //   `parent_session.append_message` here — that would double-persist and land in the
    //   wrong order. Audit captures the queued state; `inserted_entry_id` is only known
    //   after the loop drains, so it's `Null` here and correlated via `trace_id`.
    //
    // - **Idle**: no active loop, no listener race. Synchronously
    //   `parent_session.append_message` (single write) then push to `state.messages` so
    //   the user's next `prompt()` / `continue_()` sees the promotion without an explicit
    //   rehydrate. Loop isn't running, so no `MessageEnd` fires for this message → no
    //   duplicate listener write.
    let queued_for_followup = parent_agent.is_streaming();
    let (audit_state, inserted_entry_id_value, inserted_entry_id_str) = if queued_for_followup {
        parent_agent.enqueue_follow_up(user_message);
        (
            "queued",
            serde_json::Value::Null,
            String::new(), // event field is set; TUI / /triggers audit join by trace_id
        )
    } else {
        let id = match parent_session.append_message(user_message.clone()).await {
            Ok(id) => id,
            Err(e) => {
                emit_from_listeners(
                    listeners,
                    HarnessEvent::PersistenceError {
                        context: "trigger_promotion".into(),
                        message: format!("promotion message append failed: {:?}", e.code),
                    },
                );
                // Audit the failure so jsonl-only readers know promotion attempted but
                // was lost.
                let audit_data = serde_json::json!({
                    "state": "failed",
                    "trace_id": trace_id,
                    "promote_kind": promote_kind,
                    "template_name": template_name,
                    "template_hash": template_hash,
                    "inserted_entry_id": serde_json::Value::Null,
                    "rule_id": serde_json::Value::Null,
                    "redaction_status": "render_error",
                    "dedup_collapsed": false,
                    "prefix_injected": prefix_injected,
                });
                let _ = parent_session
                    .append_custom("trigger_promotion", Some(audit_data))
                    .await;
                return;
            }
        };
        parent_agent.state().messages.push(user_message);
        ("success", serde_json::Value::String(id.clone()), id)
    };

    let audit_data = serde_json::json!({
        "state": audit_state,
        "trace_id": trace_id,
        "promote_kind": promote_kind,
        "template_name": template_name,
        "template_hash": template_hash,
        "inserted_entry_id": inserted_entry_id_value,
        "rule_id": serde_json::Value::Null,
        "redaction_status": redaction_status,
        "dedup_collapsed": false,
        "prefix_injected": prefix_injected,
    });
    if let Err(e) = parent_session
        .append_custom("trigger_promotion", Some(audit_data))
        .await
    {
        emit_from_listeners(
            listeners,
            HarnessEvent::PersistenceError {
                context: "trigger_promotion".into(),
                message: format!(
                    "trigger_promotion ({audit_state}) append failed: {:?}",
                    e.code
                ),
            },
        );
    }
    emit_from_listeners(
        listeners,
        HarnessEvent::TriggerPromoted {
            trace_id: trace_id.to_string(),
            promote_kind: promote_kind.into(),
            inserted_entry_id: inserted_entry_id_str,
            template_name,
            redaction_status: redaction_status.into(),
        },
    );
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
    // Per @QA-Release-Lead's PR #65 review: cap must include the truncation marker so
    // the final body fits the documented 4 KiB boundary. Reuse the shared helper for
    // consistency between `trigger_result.summary` and promotion body truncation.
    let (capped, _truncated) = truncate_on_char_boundary(text, SUMMARY_CAP_BYTES);
    Some(capped)
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

/// Extract the text body of a `Message::User`, joining `Blocks` text content. Returns
/// `None` for image-only messages or empty text. Used to fill
/// [`OnTurnEndContext::last_user_prompt`] for the most recent user message in the
/// transcript.
fn extract_user_message_text(u: &pie_ai::UserMessage) -> Option<String> {
    match &u.content {
        pie_ai::UserContent::Text(s) => {
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        pie_ai::UserContent::Blocks(blocks) => {
            let mut out = String::new();
            for block in blocks {
                if let pie_ai::UserContentBlock::Text(t) = block {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&t.text);
                }
            }
            if out.is_empty() { None } else { Some(out) }
        }
    }
}

/// Extract the text payload from the `AgentMessage` the caller passed into
/// `prompt_with_message`. Returns `None` for non-LLM or non-user messages and for empty
/// content. Used to fill [`OnTurnEndContext::last_user_prompt`] for the freshly-arrived
/// user prompt before the transcript has been mutated.
fn extract_user_prompt_text(msg: &AgentMessage) -> Option<String> {
    match msg {
        AgentMessage::Llm(PiMessage::User(u)) => extract_user_message_text(u),
        _ => None,
    }
}
