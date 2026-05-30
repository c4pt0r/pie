//! `Agent` state machine. Partial 1:1 port of `packages/agent/src/agent.ts` (~554 lines). The
//! bare Agent owns conversation state (`AgentState`) and exposes
//! `prompt()` / `continue()` / `subscribe()` / `abort()`. It never reaches into the filesystem
//! — IO belongs to the harness or the caller.
//!
//! Implemented:
//! - State container + getters/setters (Mutex-protected)
//! - Listener subscription with unsubscribe fn
//! - `prompt(...)` / `continue_()` driving the agent loop
//! - `abort()` via `tokio_util::sync::CancellationToken`
//! - Steering / follow-up queues (`enqueue_steering` / `enqueue_follow_up`)
//!
//! TODO:
//! - `onPayload` / `onResponse` SimpleStreamOptions surface
//! - `transformContext` & `getApiKey` hooks (declared, wired up later)
//! - `prepareNextTurn` model/thinking-level rewrite mid-run

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{run_agent_loop, run_agent_loop_continue};
use crate::types::*;

use pie_ai::Message;

/// Listener type. Receives lifecycle events and the active cancellation token for the run.
pub type AgentListener = Arc<
    dyn Fn(
            AgentEvent,
            CancellationToken,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// Options accepted by [`Agent::new`].
#[derive(Default)]
pub struct AgentOptions {
    pub initial_state: Option<AgentState>,
    pub convert_to_llm: Option<ConvertToLlm>,
    pub transform_context: Option<TransformContext>,
    pub stream_fn: Option<StreamFn>,
    pub get_api_key: Option<GetApiKey>,
    pub before_tool_call: Option<BeforeToolCallHook>,
    pub after_tool_call: Option<AfterToolCallHook>,
    pub on_control_plane_prompt: Option<OnControlPlanePromptHook>,
    pub should_stop_after_turn: Option<ShouldStopHook>,
    pub prepare_next_turn: Option<PrepareNextTurnHook>,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub session_id: Option<String>,
    pub tool_execution: ToolExecutionMode,
}

/// Stateful wrapper around the low-level agent loop.
pub struct Agent {
    inner: Arc<AgentInner>,
}

pub(crate) struct AgentInner {
    pub state: Mutex<AgentState>,
    pub listeners: Mutex<Vec<AgentListener>>,
    pub steering: Mutex<PendingMessageQueue>,
    pub follow_up: Mutex<PendingMessageQueue>,
    pub options: AgentOptions,
    pub active_cancel: Mutex<Option<CancellationToken>>,
    pub idle: Notify,
}

pub(crate) struct PendingMessageQueue {
    mode: QueueMode,
    items: Vec<AgentMessage>,
}

impl PendingMessageQueue {
    fn new(mode: QueueMode) -> Self {
        Self {
            mode,
            items: Vec::new(),
        }
    }

    pub fn enqueue(&mut self, m: AgentMessage) {
        self.items.push(m);
    }

    pub fn drain(&mut self) -> Vec<AgentMessage> {
        match self.mode {
            QueueMode::All => std::mem::take(&mut self.items),
            QueueMode::OneAtATime => {
                if self.items.is_empty() {
                    Vec::new()
                } else {
                    vec![self.items.remove(0)]
                }
            }
        }
    }

    #[allow(dead_code)] // public API for future schedulers; not used by the current loop.
    pub fn has_items(&self) -> bool {
        !self.items.is_empty()
    }
}

impl Agent {
    pub fn new(mut options: AgentOptions) -> Self {
        let state = options.initial_state.take().unwrap_or_default();
        if options.convert_to_llm.is_none() {
            options.convert_to_llm = Some(default_convert_to_llm());
        }
        let inner = AgentInner {
            state: Mutex::new(state),
            listeners: Mutex::new(Vec::new()),
            steering: Mutex::new(PendingMessageQueue::new(options.steering_mode)),
            follow_up: Mutex::new(PendingMessageQueue::new(options.follow_up_mode)),
            options,
            active_cancel: Mutex::new(None),
            idle: Notify::new(),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Subscribe to lifecycle events. Returns an unsubscribe closure.
    pub fn subscribe(&self, listener: AgentListener) -> impl FnOnce() {
        let inner = self.inner.clone();
        inner.listeners.lock().push(listener.clone());
        move || {
            let mut listeners = inner.listeners.lock();
            if let Some(pos) = listeners.iter().position(|l| Arc::ptr_eq(l, &listener)) {
                listeners.remove(pos);
            }
        }
    }

    /// Inspect the current agent state. The lock guards against concurrent loop mutations.
    pub fn state(&self) -> parking_lot::MutexGuard<'_, AgentState> {
        self.inner.state.lock()
    }

    pub fn is_streaming(&self) -> bool {
        self.inner.state.lock().is_streaming
    }

    pub fn enqueue_steering(&self, message: AgentMessage) {
        self.inner.steering.lock().enqueue(message);
    }

    pub fn enqueue_follow_up(&self, message: AgentMessage) {
        self.inner.follow_up.lock().enqueue(message);
    }

    /// Abort the active run, if any. Subsequent calls are no-ops.
    pub fn abort(&self) {
        if let Some(token) = self.inner.active_cancel.lock().as_ref() {
            token.cancel();
        }
    }

    /// Active cancellation token while a run is in flight, otherwise `None`.
    pub fn active_token(&self) -> Option<CancellationToken> {
        self.inner.active_cancel.lock().clone()
    }

    /// Start a new prompt. Appends a user `AgentMessage`, runs the loop, awaits completion.
    pub async fn prompt(&self, message: AgentMessage) -> Result<(), AgentRunError> {
        self.prompt_many(vec![message]).await
    }

    /// Start a new prompt with a batch of messages.
    pub async fn prompt_many(&self, messages: Vec<AgentMessage>) -> Result<(), AgentRunError> {
        self.guard_not_streaming()?;
        run_agent_loop(self.inner.clone(), messages).await
    }

    /// Continue from the current transcript without appending new user messages.
    pub async fn continue_(&self) -> Result<(), AgentRunError> {
        self.guard_not_streaming()?;
        run_agent_loop_continue(self.inner.clone()).await
    }

    fn guard_not_streaming(&self) -> Result<(), AgentRunError> {
        if self.is_streaming() {
            return Err(AgentRunError::AlreadyStreaming);
        }
        Ok(())
    }
}

/// Errors that can short-circuit `prompt` / `continue_`.
#[derive(Debug, thiserror::Error)]
pub enum AgentRunError {
    #[error(
        "Agent is already processing a prompt. Use enqueue_steering/enqueue_follow_up or wait for completion."
    )]
    AlreadyStreaming,
    #[error("{0}")]
    Other(String),
}

impl AgentInner {
    pub fn convert_to_llm(&self, msgs: &[AgentMessage]) -> Vec<Message> {
        self.options
            .convert_to_llm
            .as_ref()
            .expect("convert_to_llm is always set in Agent::new")(msgs)
    }
}
