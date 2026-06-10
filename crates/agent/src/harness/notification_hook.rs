//! RFC 1 (issue #20) `NotificationHook` trait + status surface.
//!
//! A `NotificationHook` is the runtime's transport-agnostic plug for external sources
//! (MCP server pushes, local cron, file-watch, etc.). Adapters own the transport,
//! normalize the inbound stream into [`Trigger`](super::trigger::Trigger) envelopes, and
//! push them into a shared `TriggerSink`. The runtime consumes whatever the hooks
//! produce, regardless of source.
//!
//! Status: **types and trait only**. The runtime supervisor that spawns / monitors hooks
//! and the per-source fair scheduler land in a follow-up PR; this skeleton exists so
//! Tools/MCP and Provider/Auth can build adapters and credential resolvers against the
//! trait in parallel.

use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::trigger::Trigger;

/// Sink that hooks push triggers into. The runtime owns the receiver and the dedup /
/// permission / agent-loop pipeline. Cloning the sender is cheap; multiple hooks share the
/// same sink and the runtime fair-schedules between them.
///
/// `mpsc::UnboundedSender` is intentional for v1 — bounded back-pressure is a follow-up
/// (and will be enforced at the hook level via per-source `queued_count` watermarks rather
/// than upstream channel capacity).
pub type TriggerSink = mpsc::UnboundedSender<Trigger>;

/// Long-running source adapter trait. One instance per configured source.
///
/// Implementations live in `crates/coding-agent` (or downstream crates) — the runtime
/// crate must stay transport-agnostic. The runtime invokes `run` once per hook on a
/// dedicated task; the task is expected to live until the supervisor cancels it (Tokio
/// cancellation token), at which point `run` should return promptly.
#[async_trait::async_trait]
pub trait NotificationHook: Send + Sync {
    /// Stable label used in `NotificationHookStatus`, `/triggers hooks` UI rows, and
    /// per-source counters. Should be short and human-readable (e.g. `"mcp:filesystem"`,
    /// `"cron"`).
    fn label(&self) -> &str;

    /// Drive the source. Push triggers into `sink` as they arrive. Return `Ok(())` on
    /// clean shutdown or `Err` on protocol / auth failure; the supervisor records the
    /// failure on the hook status and may restart per its backoff policy.
    async fn run(&self, sink: TriggerSink) -> Result<(), HookError>;

    /// Snapshot for status views (`/triggers hooks`, `pie status`). Called frequently; the
    /// implementation should keep this cheap (atomic loads or `parking_lot::Mutex`).
    fn status(&self) -> NotificationHookStatus;
}

/// Hooks can also be stored / shared as boxed trait objects. Most callers will use this
/// alias instead of writing the trait-object syntax everywhere.
pub type DynNotificationHook = Arc<dyn NotificationHook>;

/// Boxed long-running future, exposed for downstream test fixtures that want to spawn a
/// hook manually without going through the supervisor.
pub type HookFuture = Pin<Box<dyn std::future::Future<Output = Result<(), HookError>> + Send>>;

/// Failure modes reported by a hook to the runtime supervisor. The supervisor decides
/// whether to restart, escalate to `requires_attention`, or surface as a user error.
#[derive(Clone, Debug, thiserror::Error)]
pub enum HookError {
    /// Source-specific authentication failed (token expired, scope mismatch, etc.). The
    /// supervisor marks the hook as `AuthFailed` and does not auto-restart.
    #[error("auth failed: {reason}")]
    AuthFailed { reason: String },

    /// Source negotiated an incompatible protocol version. Distinct from `AuthFailed`
    /// because UX should suggest "upgrade client/source" not "re-login".
    #[error("protocol mismatch: {reason}")]
    ProtocolMismatch { reason: String },

    /// Transport closed cleanly or due to a recoverable network error. Supervisor restarts
    /// with exponential backoff.
    #[error("disconnected: {reason}")]
    Disconnected { reason: String },

    /// The source produced a frame that did not match the declared schema. Supervisor
    /// records and may restart; if it persists the hook is moved to `AuthFailed`-equivalent
    /// `requires_attention`.
    #[error("schema invalid: {reason}")]
    SchemaInvalid { reason: String },

    /// Sink was dropped — the runtime is shutting down. Hook should exit promptly.
    #[error("sink closed")]
    SinkClosed,

    /// Catch-all for unexpected errors so adapters do not need a custom error enum just to
    /// surface odd one-off failures.
    #[error("hook error: {0}")]
    Other(String),
}

/// Snapshot of a hook's current state. The runtime aggregates these into
/// `harness.trigger_status()` and exposes them via `/triggers hooks`.
///
/// Field names match RFC 1 §2.5 verbatim so the UI / acceptance tests share one
/// vocabulary with the spec.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationHookStatus {
    pub state: HookState,
    /// Wall-clock time the most recent trigger was pushed into the sink, if any.
    pub last_event_at: Option<DateTime<Utc>>,
    /// Wall-clock time the most recent ack was received, if the adapter protocol has
    /// explicit acknowledgements. MCP push / cron / file-watch leave this `None`.
    pub last_ack_at: Option<DateTime<Utc>>,
    /// Most recent transport-level error, if any. Cleared on next successful transition
    /// back to `Connected`.
    pub last_error: Option<String>,
    /// Adapter-side queued depth. The runtime's bounded back-pressure is a follow-up; for
    /// v1 hooks expose their own queue depth so `/triggers hooks` can show it.
    pub queued_count: u64,
    /// Count of events the adapter intentionally dropped (e.g. unsigned custom MCP
    /// notification without `_pie_dedup_key`).
    pub dropped_count: u64,
    /// Count of events the adapter dedup-suppressed before pushing into the sink. Distinct
    /// from runtime-side dedup, which is separate and counted in `TriggerRecord`.
    pub deduped_count: u64,
    /// User-readable subscription labels (e.g. `"GitHub: repo c4pt0r/pie"`,
    /// `"Slock: #dev"`). Stable across reconnects.
    pub subscription_labels: Vec<String>,
    /// When `Some`, UI highlights this hook and surfaces the message. The supervisor only
    /// sets this when the cause is one the user can act on (panic, protocol violation,
    /// auth failure, sustained reconnect backoff > 60s — exact thresholds in §2.5).
    pub requires_attention: Option<String>,
}

impl NotificationHookStatus {
    /// Construct a fresh status for a hook that has not yet started. Used by hooks during
    /// their constructor before the first `run` invocation.
    pub fn pending() -> Self {
        Self {
            state: HookState::Disconnected {
                reason: "not yet started".into(),
            },
            last_event_at: None,
            last_ack_at: None,
            last_error: None,
            queued_count: 0,
            dropped_count: 0,
            deduped_count: 0,
            subscription_labels: Vec::new(),
            requires_attention: None,
        }
    }
}

/// Per-hook lifecycle state. The runtime supervisor reads this for `/triggers hooks`; the
/// hook itself updates it as transport events arrive. RFC 1 §2.5 + Provider/Auth refinement
/// (RFC 0 §3.3): `AuthFailed` is reserved for credential failures, `Disconnected` covers
/// protocol mismatches, and `Disabled` is only entered when explicitly disabled by the
/// user / supervisor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookState {
    Connected,
    Reconnecting,
    Disconnected {
        reason: String,
    },
    /// User or supervisor explicitly disabled this hook. Distinct from `Disconnected`:
    /// `Disabled` is intentional, `Disconnected` is transient.
    Disabled,
    /// Credential failure. Use `Disconnected { reason: "protocol_mismatch" }` for protocol
    /// version mismatches; do not collapse them into `AuthFailed`.
    AuthFailed {
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_pending_serializes_with_disconnected_state() {
        let pending = NotificationHookStatus::pending();
        let json = serde_json::to_string(&pending).unwrap();
        assert!(
            json.contains("\"kind\":\"disconnected\""),
            "pending status uses snake_case disconnected variant, got {json}"
        );
        assert!(json.contains("\"reason\":\"not yet started\""));
    }

    #[test]
    fn hook_state_serde_round_trip_for_each_variant() {
        for state in [
            HookState::Connected,
            HookState::Reconnecting,
            HookState::Disconnected {
                reason: "broken pipe".into(),
            },
            HookState::Disabled,
            HookState::AuthFailed {
                reason: "401 unauthorized".into(),
            },
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let decoded: HookState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, decoded);
        }
    }

    #[test]
    fn hook_state_uses_snake_case_kind_tag() {
        let cases = [
            (HookState::Connected, "connected"),
            (HookState::Reconnecting, "reconnecting"),
            (
                HookState::Disconnected { reason: "x".into() },
                "disconnected",
            ),
            (HookState::Disabled, "disabled"),
            (HookState::AuthFailed { reason: "x".into() }, "auth_failed"),
        ];
        for (state, expected_kind) in cases {
            let json = serde_json::to_string(&state).unwrap();
            assert!(
                json.contains(&format!("\"kind\":\"{expected_kind}\"")),
                "{state:?} → {json} (expected kind={expected_kind})"
            );
        }
    }

    #[test]
    fn hook_error_displays_with_distinct_message_per_kind() {
        // Important: `AuthFailed` and `ProtocolMismatch` UX divergence (see RFC 0 §3.3).
        assert!(
            HookError::AuthFailed {
                reason: "401".into(),
            }
            .to_string()
            .contains("auth failed")
        );
        assert!(
            HookError::ProtocolMismatch {
                reason: "v=2 not supported".into(),
            }
            .to_string()
            .contains("protocol mismatch")
        );
        assert!(
            HookError::Disconnected {
                reason: "closed".into(),
            }
            .to_string()
            .contains("disconnected")
        );
        assert!(HookError::SinkClosed.to_string().contains("sink closed"));
    }
}
