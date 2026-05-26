//! Shared REPL execution kernel for terminal and future web frontends.
//!
//! This module owns the "what work should the agent run" boundary: prompt futures, abort, model
//! capability checks, and queued-turn value types. The terminal UI still owns rendering and
//! keyboard/mouse handling, but it should not construct harness futures directly. Keeping that
//! split narrow lets the upcoming web UI reuse the same turn semantics without copying TUI code.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::agent_session::{AgentSession, RetrySettings};
use pie_agent_core::{AgentHarness, AgentRunError};
use pie_ai::{ImageContent, InputModality};

/// In-flight model turn, polled by a frontend event loop.
///
/// Running this as a local future (not `tokio::spawn`) sidesteps the `Send` bound:
/// `AgentSession::prompt` briefly holds a `parking_lot` guard across an `.await`, so its future is
/// `!Send`.
pub(crate) type TurnFut = Pin<Box<dyn Future<Output = Result<Option<String>, AgentRunError>>>>;

#[derive(Default)]
pub(crate) struct TurnState {
    pub(crate) fut: Option<TurnFut>,
    pub(crate) aborted: bool,
    /// Prefix for the error line if the turn fails (e.g. `triggered turn: `).
    pub(crate) prefix: &'static str,
}

pub(crate) async fn poll_turn(fut: &mut Option<TurnFut>) -> Result<Option<String>, AgentRunError> {
    // Only created by `select!` when `fut.is_some()`, so the unwrap is sound.
    fut.as_mut().expect("turn future present").await
}

pub(crate) enum QueuedTurn {
    UserPrompt {
        display: String,
        prompt: String,
        images: Vec<ImageContent>,
    },
    AgentPrompt {
        display: String,
        prompt: String,
        error_context: &'static str,
    },
    PromptTemplate {
        display: String,
        name: String,
        vars: serde_json::Map<String, serde_json::Value>,
    },
    Compaction {
        display: String,
        custom: Option<String>,
    },
}

impl QueuedTurn {
    pub(crate) fn display(&self) -> &str {
        match self {
            Self::UserPrompt { display, .. }
            | Self::AgentPrompt { display, .. }
            | Self::PromptTemplate { display, .. }
            | Self::Compaction { display, .. } => display,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ReplKernel {
    harness: Arc<AgentHarness>,
    retry: RetrySettings,
}

impl ReplKernel {
    pub(crate) fn new(harness: Arc<AgentHarness>, retry: RetrySettings) -> Self {
        Self { harness, retry }
    }

    pub(crate) fn harness(&self) -> &Arc<AgentHarness> {
        &self.harness
    }

    pub(crate) fn abort(&self) {
        self.harness.abort();
    }

    pub(crate) fn is_streaming(&self) -> bool {
        self.harness.agent().is_streaming()
    }

    pub(crate) fn current_model_accepts_images(&self) -> bool {
        let state = self.harness.agent().state();
        state
            .model
            .as_ref()
            .map(|model| model.input.contains(&InputModality::Image))
            .unwrap_or(false)
    }

    pub(crate) fn prompt_turn(&self, prompt: String) -> TurnFut {
        let harness = self.harness.clone();
        Box::pin(async move { harness.prompt(prompt).await.map(|_| None) })
    }

    pub(crate) fn user_prompt_turn(
        &self,
        prompt_text: String,
        loaded_images: Vec<ImageContent>,
    ) -> TurnFut {
        let harness = self.harness.clone();
        let retry = self.retry.clone();
        let has_images = !loaded_images.is_empty();
        Box::pin(async move {
            if has_images {
                harness
                    .prompt_with_images(prompt_text, loaded_images)
                    .await
                    .map(|_| None)
            } else {
                AgentSession::new(harness, retry)
                    .prompt(prompt_text)
                    .await
                    .map(|_| None)
            }
        })
    }

    pub(crate) fn template_turn(
        &self,
        name: String,
        vars: serde_json::Map<String, serde_json::Value>,
    ) -> TurnFut {
        let harness = self.harness.clone();
        Box::pin(async move {
            harness
                .prompt_from_template(&name, vars)
                .await
                .map(|_| None)
        })
    }

    pub(crate) fn compaction_turn(&self, custom: Option<String>) -> TurnFut {
        let harness = self.harness.clone();
        Box::pin(async move {
            harness.force_compact(custom).await.map(|ran| {
                Some(if ran {
                    "compaction ran".to_string()
                } else {
                    "nothing to compact".to_string()
                })
            })
        })
    }

    pub(crate) fn continue_turn(&self) -> TurnFut {
        let harness = self.harness.clone();
        Box::pin(async move { harness.continue_().await.map(|_| None) })
    }
}
