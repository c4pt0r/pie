//! Core type universe for `pie-agent-core`. 1:1 port of `packages/agent/src/types.ts`.
//!
//! The agent runtime sits on top of `pie-ai` and adds:
//! - `AgentMessage`: superset of `pie_ai::Message` plus user-defined custom variants
//! - `AgentTool`: tool definition with executor, label, and execution-mode hint
//! - `AgentEvent`: lifecycle events for UI subscribers
//! - `AgentLoopConfig`: per-run callbacks (`convert_to_llm`, `transform_context`, before/after tool
//!   hooks, steering/follow-up queue providers, etc.)
//!
//! Two Rust-specific adaptations:
//! - TS uses declaration merging on `CustomAgentMessages`. Rust gets a `Custom { role, payload }`
//!   variant of `AgentMessage` — apps pick a role tag and put arbitrary JSON in payload. The
//!   `convert_to_llm` hook filters/translates these before each LLM call.
//! - TS callback fields become `Box<dyn Fn(...) -> Pin<Box<dyn Future>>>` (async closures via
//!   `async_trait` traits, or boxed `Future`s for one-shots). Pure-data fields stay structs.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use pie_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Context as PiContext,
    ImageContent, Message, Model, SimpleStreamOptions, TextContent, ToolCall, ToolResultMessage,
    UserContentBlock,
};

// ──────────────────────────────────────────────────────────────────────────────────────────
// Execution modes / queue modes
// ──────────────────────────────────────────────────────────────────────────────────────────

/// Configuration for how tool calls from a single assistant message are executed.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolExecutionMode {
    /// Each tool call is prepared, executed, and finalized before the next one starts.
    Sequential,
    /// Tool calls are prepared sequentially, then allowed tools execute concurrently.
    #[default]
    Parallel,
}

/// Controls how many queued user messages are injected at a queue drain point.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    /// Drain and inject every queued message at that point.
    #[default]
    All,
    /// Drain and inject only the oldest queued message; the rest stay queued.
    OneAtATime,
}

/// Thinking/reasoning level for the agent runtime. Wider than `pie_ai::ThinkingLevel` because the
/// agent layer exposes an explicit "off".
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }

    /// Translate to the pie-ai `ThinkingLevel`. Returns `None` for `Off` since `pie-ai` has no
    /// off variant — callers should skip emitting reasoning when this is `None`.
    pub fn to_pie_ai(self) -> Option<pie_ai::ThinkingLevel> {
        match self {
            Self::Off => None,
            Self::Minimal => Some(pie_ai::ThinkingLevel::Minimal),
            Self::Low => Some(pie_ai::ThinkingLevel::Low),
            Self::Medium => Some(pie_ai::ThinkingLevel::Medium),
            Self::High => Some(pie_ai::ThinkingLevel::High),
            Self::Xhigh => Some(pie_ai::ThinkingLevel::Xhigh),
        }
    }
}

impl std::str::FromStr for ThinkingLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Self::Off),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            other => Err(format!("invalid thinking level: {other}")),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// AgentMessage — pie-ai Message superset + user-defined custom variants
// ──────────────────────────────────────────────────────────────────────────────────────────

/// The agent's superset message type. Custom variants carry an opaque JSON payload tagged by a
/// `role` string of the app's choosing; the `convert_to_llm` hook filters/translates them before
/// each LLM call. UI-only messages should be filtered out there.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentMessage {
    /// One of the three pie-ai message roles (user/assistant/toolResult).
    Llm(Message),
    /// App-specific custom message (e.g. compaction summary, branch marker, UI notification).
    Custom(CustomMessage),
}

/// Tagged custom message. Apps pick the `role` string and the `payload` shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomMessage {
    pub role: String,
    pub timestamp: i64,
    #[serde(flatten)]
    pub payload: serde_json::Value,
}

impl From<Message> for AgentMessage {
    fn from(m: Message) -> Self {
        Self::Llm(m)
    }
}

impl AgentMessage {
    /// Convenience: returns the inner LLM message if this variant is `Llm`.
    pub fn as_llm(&self) -> Option<&Message> {
        match self {
            Self::Llm(m) => Some(m),
            Self::Custom(_) => None,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Tools
// ──────────────────────────────────────────────────────────────────────────────────────────

/// A single tool call content block from an assistant message. Alias for clarity.
pub type AgentToolCall = ToolCall;

/// Final or partial result produced by a tool.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentToolResult {
    /// Text or image content returned to the model.
    pub content: Vec<UserContentBlock>,
    /// Arbitrary structured details for logs or UI rendering.
    #[serde(default)]
    pub details: serde_json::Value,
    /// Hint that the agent should stop after the current tool batch. Early termination only
    /// happens when every finalized tool result in the batch sets this to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}

impl Default for AgentToolResult {
    fn default() -> Self {
        Self {
            content: Vec::new(),
            details: serde_json::Value::Null,
            terminate: None,
        }
    }
}

/// Callback used by tools to stream partial execution updates back to the agent runtime.
pub type AgentToolUpdate = Arc<dyn Fn(AgentToolResult) + Send + Sync>;

/// Tool definition used by the agent runtime.
///
/// TS layers a schema generic on top of `pie_ai::Tool`; in Rust the schema is a free-form JSON
/// Schema (matching `pie-ai`'s decision), so we keep this as a trait and let implementations carry
/// whatever typed state they want.
#[async_trait]
pub trait AgentTool: Send + Sync {
    /// Underlying pie-ai tool (`name`, `description`, `parameters` JSON Schema).
    fn definition(&self) -> &pie_ai::Tool;

    /// Human-readable label for UI display.
    fn label(&self) -> &str;

    /// Per-tool execution mode override; `None` means "use the loop default".
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        None
    }

    /// Compatibility shim for raw tool-call arguments. Runs once between tool resolution and
    /// dispatch, and the result is what the `before_tool_call` hook (both `ctx.args` and
    /// `ctx.tool_call.arguments`) and [`AgentTool::execute`]'s `params` see. Default passes
    /// the argument map through unchanged.
    fn prepare_arguments(&self, args: serde_json::Value) -> serde_json::Value {
        args
    }

    /// Per-tool classification evaluated **before** [`BeforeToolCallHook`]. The agent loop
    /// uses the returned [`PermissionClassification`] to decide whether to run the user's
    /// `before_tool_call` hook ([`PermissionClassification::Allow`], the default), route
    /// through the user-confirmation prompt channel ([`PermissionClassification::Prompt`]),
    /// or hard-deny ([`PermissionClassification::Block`]).
    ///
    /// `prepared_args` is the value after [`AgentTool::prepare_arguments`] — the same shape
    /// the tool will actually execute against and the same shape used to compute the
    /// `args_hash` that binds prompt approvals. Tools should classify against the prepared
    /// form, not the raw args.
    ///
    /// Default impl returns [`PermissionClassification::Allow`] so existing tools compile
    /// unchanged and behave exactly as before. Tools opt into prompt-gating per issue #110
    /// design v0.2 — see e.g. `SetSkillState::permission_classification` (sub-PR 3) for the
    /// canonical pattern of returning `Prompt` for escalating arg shapes and `Allow` for
    /// narrowing.
    fn permission_classification(
        &self,
        _prepared_args: &serde_json::Value,
    ) -> PermissionClassification {
        PermissionClassification::Allow
    }

    /// Execute the tool call. Implementations should *not* encode errors in `content` — return
    /// `Err` instead; the agent loop wraps it into an `is_error: true` tool result.
    ///
    /// `on_update`, when `Some`, is the per-call streaming-progress callback. It is bound to
    /// the lifetime of this `execute` call — the agent loop builds a pump that consumes
    /// updates in send order and emits them as [`crate::AgentEvent::ToolExecutionUpdate`].
    ///
    /// Contract: do not retain `on_update` past `execute`'s return — e.g. by cloning the
    /// `Arc` into a `tokio::spawn`ed task that outlives this call. The agent loop caps the
    /// pump shutdown with a short timeout for safety, but any updates emitted after return
    /// are dropped without reaching subscribers.
    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        cancel: CancellationToken,
        on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError>;
}

#[derive(Debug, thiserror::Error)]
pub enum AgentToolError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

impl From<String> for AgentToolError {
    fn from(s: String) -> Self {
        Self::Message(s)
    }
}

impl From<&str> for AgentToolError {
    fn from(s: &str) -> Self {
        Self::Message(s.to_string())
    }
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Agent context / state / events
// ──────────────────────────────────────────────────────────────────────────────────────────

/// Context snapshot passed into the low-level agent loop.
#[derive(Default)]
pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<dyn AgentTool>>,
}

impl Clone for AgentContext {
    fn clone(&self) -> Self {
        Self {
            system_prompt: self.system_prompt.clone(),
            messages: self.messages.clone(),
            tools: self.tools.clone(),
        }
    }
}

/// Public agent state. Use the getter/setter methods rather than mutating fields directly so
/// implementations can copy assigned arrays before storing them (matches the TS accessor
/// semantics).
#[derive(Default)]
pub struct AgentState {
    /// System prompt sent with each model request.
    pub system_prompt: String,
    /// Active model used for future turns.
    pub model: Option<Model>,
    /// Requested reasoning level for future turns.
    pub thinking_level: Option<ThinkingLevel>,
    /// Available tools.
    pub tools: Vec<Arc<dyn AgentTool>>,
    /// Conversation transcript.
    pub messages: Vec<AgentMessage>,
    /// True while the agent is processing a prompt or continuation.
    pub is_streaming: bool,
    /// Partial assistant message for the current streamed response, if any.
    pub streaming_message: Option<AgentMessage>,
    /// Tool call ids currently executing.
    pub pending_tool_calls: HashSet<String>,
    /// Error message from the most recent failed or aborted assistant turn, if any.
    pub error_message: Option<String>,
}

/// Events emitted by the Agent for UI updates.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    /// A tool call's [`PermissionClassification::Prompt`] surfaced through the
    /// `on_control_plane_prompt` hook (or fell back to fail-closed deny when no hook was
    /// configured). Fires after the hook returns; the decision is final by this point.
    /// Issue #110 design v0.2 — observability for prompt resolution. Harness layer
    /// translates this into [`crate::HarnessEvent::ControlPlanePromptResolved`] and writes
    /// the canonical `control_plane_prompt` Custom audit entry.
    ControlPlanePromptResolved {
        tool_call_id: String,
        tool_name: String,
        args_hash: String,
        label: String,
        decision: String,
        reason: Option<String>,
    },
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    TurnStart,
    TurnEnd {
        message: AgentMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: AgentMessage,
    },
    MessageUpdate {
        message: AgentMessage,
        assistant_message_event: AssistantMessageEvent,
    },
    MessageEnd {
        message: AgentMessage,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
        partial_result: AgentToolResult,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
    },
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Hook contexts and results
// ──────────────────────────────────────────────────────────────────────────────────────────

/// Result returned from `before_tool_call`. `block: true` skips execution; `reason` becomes the
/// error text shown in the synthesized tool result. When `prompt.is_some()` and `block: false`,
/// the agent loop suspends the tool call and routes through the [`OnControlPlanePromptHook`]
/// channel — see issue #110 design v0.2.
#[derive(Clone, Debug, Default)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    /// When `Some`, the agent loop awaits user confirmation via
    /// [`OnControlPlanePromptHook`] before dispatching the tool. `block: true` always wins —
    /// a `before_tool_call` hook that wants to hard-deny doesn't get its decision
    /// "promoted" to a prompt. Default `None` preserves legacy behavior for every tool that
    /// has not opted in via [`AgentTool::permission_classification`].
    pub prompt: Option<ControlPlanePromptRequest>,
}

/// Per-tool classification override evaluated **before** [`BeforeToolCallHook`]. Tools that
/// mutate persistent state (skills, triggers, hub trust) return [`PermissionClassification::Prompt`]
/// with a bounded human-readable reason; tools that wrap escalations the model must never
/// self-authorize (e.g. re-enabling a `disable_model_invocation=true` skill) return
/// [`PermissionClassification::Block`]. Default impl on [`AgentTool::permission_classification`]
/// returns [`PermissionClassification::Allow`] so existing tools compile and behave unchanged.
///
/// Issue #110 design v0.2 Artifact A.
#[derive(Clone, Debug)]
pub enum PermissionClassification {
    /// Default. Tool dispatches through the existing `before_tool_call` path with no
    /// runtime-side gating beyond what the user's hook decides.
    Allow,
    /// Tool requires a user-mediated confirmation before dispatch. The agent loop synthesizes
    /// a [`BeforeToolCallResult::prompt`] from the supplied `reason` (and bounded args
    /// preview) and routes through [`OnControlPlanePromptHook`]. If no prompt hook is
    /// configured the runtime fails closed.
    Prompt { reason: String },
    /// Hard categorical refusal. The agent loop synthesizes a `Block` result with the
    /// supplied `reason` and never invokes either the user's `before_tool_call` hook or the
    /// prompt channel. Use for tool calls the runtime treats as non-negotiable refusals
    /// (e.g. the `SetSkillState(enabled=true)` stopgap before issue #110 ships).
    Block { reason: String },
}

/// Bounded preview-safe payload describing a control-plane write the runtime is asking the
/// user to confirm. Wired through [`OnControlPlanePromptHook`]; the embedder owns rendering
/// (CLI prompt card, Web confirmation modal, headless `--yes` policy, etc.).
///
/// **Bounded fields only.** The `label` and `payload` MUST NOT contain raw SKILL.md bodies,
/// raw rule text, install source URL tokens, provider/base_url credentials, auth-store
/// values, or raw payload bytes — same redaction discipline as `fefe_trust_decision`
/// (RFC #18 §5.7). Runtime caps `label` at 200 chars before persistence; `payload` is
/// embedder-defined JSON, bounded by the tool/classifier that produced it.
#[derive(Clone, Debug)]
pub struct ControlPlanePromptRequest {
    /// The `tool_call_id` of the call this prompt is gating. Used by the resolution path
    /// for replay-binding (per issue #110 design v0.2 §1 Decision binding).
    pub tool_call_id: String,
    /// Tool name (e.g. `InstallSkill`). Display-only at the runtime layer.
    pub tool_name: String,
    /// SHA-256 over `canonical_json(prepare_arguments(args))`. Binds an approval to a single
    /// concrete invocation; the runtime rejects any resolution whose `args_hash` does not
    /// match the in-flight call.
    pub args_hash: String,
    /// Embedder-facing one-line label. Runtime caps at 200 chars before persistence.
    pub label: String,
    /// Embedder-rendered preview payload. Runtime never inspects fields; the tool/classifier
    /// that produced the prompt owns its shape and redaction.
    pub payload: serde_json::Value,
    /// Why this prompt was raised (forwarded from
    /// [`PermissionClassification::Prompt { reason }`] verbatim).
    pub reason: String,
}

/// Decision returned from [`OnControlPlanePromptHook`]. The agent loop maps `Allow` to
/// dispatch, `Deny` / `Timeout` to a synthesized block. Issue #110 design v0.2 Artifact C.
#[derive(Clone, Debug)]
pub enum ControlPlanePromptDecision {
    Allow,
    Deny {
        /// Optional reason surfaced into the synthesized tool result and audit. Embedder
        /// caps before passing.
        reason: Option<String>,
    },
    /// Embedder timed out / disconnected before the user resolved. Runtime treats
    /// identically to `Deny { reason: Some("prompt timed out") }` but the audit records the
    /// distinct outcome so analytics / acceptance tests can tell them apart.
    Timeout,
}

impl ControlPlanePromptDecision {
    /// Stable `decision` string for the `control_plane_prompt` audit entry. Avoid
    /// stringifying the `Debug` representation — these values are part of the audit
    /// contract.
    pub fn as_audit_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny { .. } => "deny",
            Self::Timeout => "timeout",
        }
    }
}

/// Partial override returned from `after_tool_call`. Omitted fields keep the original executed
/// tool result values; no deep merge is performed.
#[derive(Clone, Debug, Default)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<UserContentBlock>>,
    pub details: Option<serde_json::Value>,
    pub is_error: Option<bool>,
    pub terminate: Option<bool>,
}

/// Snapshot passed into [`BeforeToolCallHook`]. Owned values so the hook future can be `'static`
/// — Rust async closures can't carry borrowed context across `.await` boundaries the way TS
/// promises do, so the loop clones what the hook needs.
#[derive(Clone)]
pub struct BeforeToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: ToolCall,
    pub args: serde_json::Value,
    pub context: AgentContext,
}

#[derive(Clone)]
pub struct AfterToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: ToolCall,
    pub args: serde_json::Value,
    pub result: AgentToolResult,
    pub is_error: bool,
    pub context: AgentContext,
}

#[derive(Clone)]
pub struct ShouldStopAfterTurnContext {
    pub message: AssistantMessage,
    pub tool_results: Vec<ToolResultMessage>,
    pub context: AgentContext,
    pub new_messages: Vec<AgentMessage>,
}

pub type PrepareNextTurnContext = ShouldStopAfterTurnContext;

/// Replacement runtime state returned from `prepare_next_turn`. `None` keeps the current values.
#[derive(Default)]
pub struct AgentLoopTurnUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Stream function alias and loop config
// ──────────────────────────────────────────────────────────────────────────────────────────

/// Stream function used by the agent loop. Mirrors `pie_ai::stream_simple` directly — sync
/// dispatch returning the event stream. Tests inject a fake to drive deterministic behavior
/// without touching `pie-ai`.
pub type StreamFn = Arc<
    dyn Fn(&Model, &PiContext, Option<&SimpleStreamOptions>) -> AssistantMessageEventStream
        + Send
        + Sync,
>;

/// Build the default `StreamFn` — delegates to `pie_ai::stream_simple`.
pub fn default_stream_fn() -> StreamFn {
    Arc::new(pie_ai::stream_simple)
}

/// Sync convertToLlm callback shape. Implementations must not panic; return a safe fallback
/// (typically an empty Vec) instead.
pub type ConvertToLlm = Arc<dyn Fn(&[AgentMessage]) -> Vec<Message> + Send + Sync>;

/// Async transformContext callback (optional). Runs before `convert_to_llm`.
pub type TransformContext = Arc<
    dyn Fn(
            Vec<AgentMessage>,
            CancellationToken,
        ) -> Pin<Box<dyn std::future::Future<Output = Vec<AgentMessage>> + Send>>
        + Send
        + Sync,
>;

/// Resolves an API key dynamically per LLM call. Useful for short-lived OAuth tokens.
pub type GetApiKey = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Configuration for one run of [`crate::agent_loop::run_agent_loop`]. Matches `AgentLoopConfig`
/// in TS field-for-field, with Rust closure types for the callbacks.
pub struct AgentLoopConfig {
    pub model: Model,
    pub simple_options: SimpleStreamOptions,

    pub convert_to_llm: ConvertToLlm,
    pub transform_context: Option<TransformContext>,
    pub get_api_key: Option<GetApiKey>,

    /// Override the streaming entry point. Defaults to `pie_ai::stream_simple`.
    pub stream_fn: Option<StreamFn>,

    /// Tool execution mode. Default: [`ToolExecutionMode::Parallel`].
    pub tool_execution: ToolExecutionMode,

    pub before_tool_call: Option<BeforeToolCallHook>,
    pub after_tool_call: Option<AfterToolCallHook>,

    pub should_stop_after_turn: Option<ShouldStopHook>,
    pub prepare_next_turn: Option<PrepareNextTurnHook>,

    pub get_steering_messages: Option<MessageQueueProvider>,
    pub get_follow_up_messages: Option<MessageQueueProvider>,

    /// Control-plane prompt resolution channel. When a tool's
    /// [`AgentTool::permission_classification`] returns
    /// [`PermissionClassification::Prompt`] (or a `before_tool_call` hook returns
    /// [`BeforeToolCallResult::prompt`] populated), the agent loop calls this hook with the
    /// synthesized [`ControlPlanePromptRequest`] and awaits a
    /// [`ControlPlanePromptDecision`]. `None` is **fail-closed deny** — any prompt-required
    /// tool call is treated as `Deny { reason: "no prompt channel" }` so an embedder that
    /// forgets to wire the channel cannot accidentally allow escalating writes.
    pub on_control_plane_prompt: Option<OnControlPlanePromptHook>,
}

// Hook trait-object aliases (boxed async closures).

pub type BeforeToolCallHook = Arc<
    dyn Fn(
            BeforeToolCallContext,
            CancellationToken,
        ) -> Pin<Box<dyn std::future::Future<Output = BeforeToolCallResult> + Send>>
        + Send
        + Sync,
>;

pub type OnControlPlanePromptHook = Arc<
    dyn Fn(
            ControlPlanePromptRequest,
            CancellationToken,
        ) -> Pin<Box<dyn std::future::Future<Output = ControlPlanePromptDecision> + Send>>
        + Send
        + Sync,
>;

pub type AfterToolCallHook = Arc<
    dyn Fn(
            AfterToolCallContext,
            CancellationToken,
        ) -> Pin<Box<dyn std::future::Future<Output = AfterToolCallResult> + Send>>
        + Send
        + Sync,
>;

pub type ShouldStopHook = Arc<
    dyn Fn(ShouldStopAfterTurnContext) -> Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        + Send
        + Sync,
>;

pub type PrepareNextTurnHook = Arc<
    dyn Fn(
            PrepareNextTurnContext,
        )
            -> Pin<Box<dyn std::future::Future<Output = Option<AgentLoopTurnUpdate>> + Send>>
        + Send
        + Sync,
>;

pub type MessageQueueProvider = Arc<
    dyn Fn() -> Pin<Box<dyn std::future::Future<Output = Vec<AgentMessage>> + Send>> + Send + Sync,
>;

/// Default convert-to-llm: keep only `AgentMessage::Llm` variants.
pub fn default_convert_to_llm() -> ConvertToLlm {
    Arc::new(|msgs: &[AgentMessage]| {
        msgs.iter()
            .filter_map(|m| match m {
                AgentMessage::Llm(m) => Some(m.clone()),
                AgentMessage::Custom(_) => None,
            })
            .collect()
    })
}

// Re-export pie-ai types frequently used alongside agent types so consumers don't need a second
// import line.
pub use pie_ai::{
    AssistantMessage as PiAssistantMessage, ImageContent as PiImageContent, Message as PiMessage,
    TextContent as PiTextContent, ToolResultMessage as PiToolResultMessage,
};

// Silence "unused import" warnings for re-exports the rest of the crate consumes through this
// module rather than directly from pie_ai.
#[allow(dead_code)]
fn _exports_marker(_: AssistantMessage, _: ImageContent, _: TextContent, _: ToolResultMessage) {}
