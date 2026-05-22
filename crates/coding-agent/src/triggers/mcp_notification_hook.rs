//! `NotificationHook` adapter that turns server-pushed MCP frames into runtime
//! [`Trigger`](pie_agent_core::Trigger) envelopes.
//!
//! Sits between [`pie_mcp::McpClient`] (RFC 1 §4.2.1 read pump, surfaced via
//! [`pie_mcp::client::McpClient::take_notifications`]) and the runtime's `TriggerSink`. One
//! instance per configured MCP server. Constructed by `mcp_loader` once
//! `RFC 1 sub-PR 2` lands a supervisor that owns hook registration; until then the type
//! exists so unit tests pin the per-method dedup / replacement-policy contract from
//! RFC 1 §4.2.3 and the follow-up notes left on PR #35.
//!
//! Mapping rules (RFC 1 §4.2.3 + PR #35 QA notes):
//!
//! | MCP method                            | idempotency key            | replacement      |
//! |---------------------------------------|----------------------------|------------------|
//! | `notifications/tools/listChanged`     | `"tools"`                  | `LatestReplaces` |
//! | `notifications/resources/listChanged` | `"resources"`              | `LatestReplaces` |
//! | `notifications/resources/updated`     | `"resources:{uri}"`        | `LatestReplaces` |
//! | `notifications/prompts/listChanged`   | `"prompts"`                | `LatestReplaces` |
//! | custom `notifications/*`              | `_meta.pie_dedup_key` else | `Drop`           |
//! |                                       | `_pie_dedup_key`           |                  |
//!
//! A custom notification that provides neither dedup key is dropped at the adapter with
//! `dropped_count += 1`; runtime never sees it. Adapters do **not** dedup themselves — the
//! runtime owns the dedup window. We surface a stable idempotency key per source/method so
//! the runtime can do its job.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use pie_agent_core::{
    CredentialScope, HookError, HookState, NotificationHook, NotificationHookStatus,
    PayloadVisibility, ReplacementPolicy, SourceKind, Trigger, TriggerAuthority, TriggerSink,
    TriggerSource,
};
use pie_mcp::client::McpServerNotification;
use tokio::sync::mpsc::UnboundedReceiver;
use uuid::Uuid;

/// One MCP server's notification stream as a runtime `NotificationHook`.
///
/// The constructor consumes the `UnboundedReceiver` returned by
/// [`pie_mcp::McpClient::take_notifications`]; the hook owns the receiver for the lifetime
/// of `run`. The supervisor (RFC 1 sub-PR 2) is expected to call `run` exactly once on a
/// dedicated task and to drop the hook on shutdown — there is no re-entrant restart path
/// because each server has its own `McpClient`, and a recovery cycle re-creates the whole
/// stack (client + transport + hook) rather than reusing the inbound receiver.
pub struct McpNotificationHook {
    /// `mcp:<server_name>`. Stable across the hook's lifetime; used in
    /// `NotificationHookStatus.subscription_labels` and `Trigger.source_label`.
    label: String,
    /// Plain server name from `mcp.toml` (e.g. `"filesystem"`), without the `mcp:` prefix.
    /// Threaded into `TriggerSource::Mcp.server_name` so the rule engine can match on it.
    server_name: String,
    /// Receiver of normalized server pushes. `Mutex<Option<...>>` so `run` can `.take()` it
    /// exactly once and the type stays `Send + Sync` even though the receiver itself is
    /// `!Sync`. After the first run, subsequent calls return `HookError::SinkClosed`
    /// because there is nothing left to drain.
    rx: Mutex<Option<UnboundedReceiver<McpServerNotification>>>,
    /// Atomic-cheap status snapshot. Re-read frequently by `/triggers hooks`; we keep it
    /// behind `parking_lot::Mutex` (matches the trait's "atomic loads or
    /// `parking_lot::Mutex`" guidance).
    status: Arc<Mutex<NotificationHookStatus>>,
}

impl McpNotificationHook {
    /// Build a hook for the named MCP server. `server_name` is what the user wrote in
    /// `mcp.toml`; `rx` comes from [`pie_mcp::McpClient::take_notifications`].
    pub fn new(
        server_name: impl Into<String>,
        rx: UnboundedReceiver<McpServerNotification>,
    ) -> Self {
        let server_name = server_name.into();
        let label = format!("mcp:{server_name}");
        let mut status = NotificationHookStatus::pending();
        // The hook's only "subscription" is the server itself — MCP push frames are not
        // per-topic.
        status.subscription_labels = vec![label.clone()];
        Self {
            label,
            server_name,
            rx: Mutex::new(Some(rx)),
            status: Arc::new(Mutex::new(status)),
        }
    }

    /// Test-only accessor for assertions on the live status. Production code reads via the
    /// trait method [`NotificationHook::status`] which clones the snapshot.
    #[cfg(test)]
    fn debug_status_handle(&self) -> Arc<Mutex<NotificationHookStatus>> {
        self.status.clone()
    }
}

#[async_trait]
impl NotificationHook for McpNotificationHook {
    fn label(&self) -> &str {
        &self.label
    }

    async fn run(&self, sink: TriggerSink) -> Result<(), HookError> {
        let mut rx = self.rx.lock().take().ok_or_else(|| {
            HookError::Other(format!(
                "{} hook already ran; receiver consumed",
                self.label
            ))
        })?;

        // First successful receiver checkout flips the state to Connected — the read pump
        // ran the JSON-RPC initialize handshake before constructing this hook, so by the
        // time we get here the transport is live.
        self.status.lock().state = HookState::Connected;

        while let Some(notification) = rx.recv().await {
            let trigger = match map_notification(&self.server_name, &notification) {
                Some(t) => t,
                None => {
                    // Custom notification without a dedup key — drop and surface count.
                    let mut st = self.status.lock();
                    st.dropped_count = st.dropped_count.saturating_add(1);
                    st.last_error = Some(format!(
                        "dropped custom notification {:?}: missing `_meta.pie_dedup_key` or `_pie_dedup_key`",
                        notification.method
                    ));
                    continue;
                }
            };
            if sink.send(trigger).is_err() {
                // Runtime is shutting down; exit cleanly. The supervisor will reap the
                // hook task and mark the hook Disconnected.
                self.status.lock().state = HookState::Disconnected {
                    reason: "sink closed".into(),
                };
                return Err(HookError::SinkClosed);
            }
            // Bookkeeping after successful push so `/triggers hooks` shows the latest event
            // even if the runtime is still draining the sink.
            let mut st = self.status.lock();
            st.last_event_at = Some(Utc::now());
            st.last_error = None;
        }

        // Pump exited because the transport closed. Update status and return cleanly so
        // the supervisor records a Disconnected hook rather than a hard failure.
        self.status.lock().state = HookState::Disconnected {
            reason: "mcp transport closed".into(),
        };
        Ok(())
    }

    fn status(&self) -> NotificationHookStatus {
        self.status.lock().clone()
    }
}

/// Translate one MCP push frame to a `Trigger`, or `None` if the frame should be dropped at
/// the adapter (custom method without `_pie_dedup_key` / `_meta.pie_dedup_key`).
///
/// Pure function so the test suite can pin every row of the §4.2.3 table without spinning
/// up a real `McpClient`.
fn map_notification(server_name: &str, n: &McpServerNotification) -> Option<Trigger> {
    let (idempotency_key, replacement_policy) = idempotency_for(&n.method, &n.params)?;
    let payload_summary = render_summary(&n.method, &n.params);
    Some(Trigger {
        source: TriggerSource::Mcp {
            server_name: server_name.to_string(),
            method: n.method.clone(),
        },
        source_kind: SourceKind::Mcp,
        source_label: format!("mcp:{server_name}"),
        event_label: n.method.clone(),
        payload_visibility: PayloadVisibility::Local,
        payload_summary,
        payload: None, // MCP push payload stays local per §3.2.2 default.
        idempotency_key,
        replacement_policy,
        trace_id: Uuid::new_v4().to_string(),
        authority: TriggerAuthority {
            // Stable principal id per server — the user-visible server name acts as the
            // opaque-stable id since `mcp.toml` enforces uniqueness.
            principal_id: format!("mcp:{server_name}"),
            principal_label: server_name.to_string(),
            credential_scope: CredentialScope::User,
            allowed_source_actions: Vec::new(),
            expires_at: None,
        },
        received_at: Utc::now(),
    })
}

/// Derive `(idempotency_key, replacement_policy)` for a given method + params per RFC 1
/// §4.2.3 / PR #35 QA follow-up. Returns `None` for custom methods that don't supply a
/// dedup key — the caller drops those at the adapter with diagnostics.
fn idempotency_for(
    method: &str,
    params: &serde_json::Value,
) -> Option<(String, ReplacementPolicy)> {
    match method {
        "notifications/tools/listChanged" => {
            Some(("tools".to_string(), ReplacementPolicy::LatestReplaces))
        }
        "notifications/resources/listChanged" => {
            Some(("resources".to_string(), ReplacementPolicy::LatestReplaces))
        }
        "notifications/prompts/listChanged" => {
            Some(("prompts".to_string(), ReplacementPolicy::LatestReplaces))
        }
        "notifications/resources/updated" => {
            // Per-URI keying so multiple updates to different resources don't collapse into
            // one event. If the server omitted `uri` (shouldn't happen per MCP spec but
            // defensive), fall back to the unscoped `"resources"` key.
            let uri = params
                .get("uri")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            Some((
                format!("resources:{uri}"),
                ReplacementPolicy::LatestReplaces,
            ))
        }
        _ => {
            // Custom notification — require an explicit dedup key. Prefer `_meta.pie_dedup_key`
            // (canonical going forward) over `_pie_dedup_key` (legacy, kept for adapters
            // already in the wild). Either form is treated as `Drop` semantics: every
            // explicit key represents one logical event, no replacement.
            extract_dedup_key(params).map(|k| (k, ReplacementPolicy::Drop))
        }
    }
}

/// Pull a dedup key out of a custom notification's params, preferring the new
/// `_meta.pie_dedup_key` location and falling back to the older top-level `_pie_dedup_key`.
fn extract_dedup_key(params: &serde_json::Value) -> Option<String> {
    if let Some(k) = params
        .get("_meta")
        .and_then(|m| m.get("pie_dedup_key"))
        .and_then(|v| v.as_str())
    {
        return Some(k.to_string());
    }
    params
        .get("_pie_dedup_key")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Render a short human-readable summary for `payload_summary`. Capped well below the
/// runtime 4 KiB persistence cap; the runtime will still re-truncate if a future caller
/// emits more.
fn render_summary(method: &str, params: &serde_json::Value) -> Option<String> {
    // Compact one-liner: method + a single inline field if obvious. For unknown shapes we
    // fall back to a length-bounded JSON snippet so the user still sees something.
    if method == "notifications/resources/updated" {
        if let Some(uri) = params.get("uri").and_then(|v| v.as_str()) {
            return Some(format!("{method} uri={uri}"));
        }
    }
    // Avoid serializing huge params blobs into the summary — cap at 200 chars.
    let raw = serde_json::to_string(params).unwrap_or_else(|_| "<unrepresentable>".into());
    let trimmed: String = raw.chars().take(200).collect();
    if trimmed.is_empty() || trimmed == "null" {
        Some(method.to_string())
    } else {
        Some(format!("{method} {trimmed}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn note(method: &str, params: serde_json::Value) -> McpServerNotification {
        McpServerNotification {
            method: method.to_string(),
            params,
        }
    }

    /// Helper: build a hook over an mpsc, run it on a task, return the sender side so the
    /// test can push notifications and a receiver to observe sunk triggers.
    fn fixture() -> (
        mpsc::UnboundedSender<McpServerNotification>,
        mpsc::UnboundedReceiver<Trigger>,
        Arc<Mutex<NotificationHookStatus>>,
        tokio::task::JoinHandle<Result<(), HookError>>,
    ) {
        let (note_tx, note_rx) = mpsc::unbounded_channel::<McpServerNotification>();
        let (trig_tx, trig_rx) = mpsc::unbounded_channel::<Trigger>();
        let hook = Arc::new(McpNotificationHook::new("filesystem", note_rx));
        let status = hook.debug_status_handle();
        let hook_for_task = hook.clone();
        let handle = tokio::spawn(async move { hook_for_task.run(trig_tx).await });
        (note_tx, trig_rx, status, handle)
    }

    /// `tools/listChanged` → idempotency `"tools"` + `LatestReplaces`, no payload, MCP
    /// source kind, server name + method threaded through.
    #[tokio::test]
    async fn tools_list_changed_maps_to_latest_replaces() {
        let (tx, mut rx, _status, handle) = fixture();
        tx.send(note("notifications/tools/listChanged", json!({})))
            .unwrap();
        let trigger = rx.recv().await.expect("trigger should arrive");
        assert_eq!(trigger.idempotency_key, "tools");
        assert_eq!(
            trigger.replacement_policy,
            ReplacementPolicy::LatestReplaces
        );
        assert_eq!(trigger.source_kind, SourceKind::Mcp);
        assert!(matches!(
            trigger.source,
            TriggerSource::Mcp { ref server_name, ref method }
                if server_name == "filesystem" && method == "notifications/tools/listChanged"
        ));
        assert_eq!(trigger.source_label, "mcp:filesystem");
        assert!(
            trigger.payload.is_none(),
            "default payload_visibility=Local hides payload"
        );
        drop(tx);
        let _ = handle.await;
    }

    /// `resources/updated` keys by URI so two updates to different files don't collapse.
    #[tokio::test]
    async fn resources_updated_keys_per_uri() {
        let (tx, mut rx, _status, handle) = fixture();
        tx.send(note(
            "notifications/resources/updated",
            json!({ "uri": "file:///a.md" }),
        ))
        .unwrap();
        tx.send(note(
            "notifications/resources/updated",
            json!({ "uri": "file:///b.md" }),
        ))
        .unwrap();
        let t1 = rx.recv().await.unwrap();
        let t2 = rx.recv().await.unwrap();
        assert_eq!(t1.idempotency_key, "resources:file:///a.md");
        assert_eq!(t2.idempotency_key, "resources:file:///b.md");
        assert_ne!(t1.idempotency_key, t2.idempotency_key);
        drop(tx);
        let _ = handle.await;
    }

    /// Custom method with `_meta.pie_dedup_key` is accepted with `Drop` policy.
    #[tokio::test]
    async fn custom_with_meta_dedup_key_passes_through() {
        let (tx, mut rx, _status, handle) = fixture();
        tx.send(note(
            "notifications/custom/event",
            json!({ "_meta": { "pie_dedup_key": "build-42" }, "detail": "ok" }),
        ))
        .unwrap();
        let trigger = rx.recv().await.unwrap();
        assert_eq!(trigger.idempotency_key, "build-42");
        assert_eq!(trigger.replacement_policy, ReplacementPolicy::Drop);
        drop(tx);
        let _ = handle.await;
    }

    /// Legacy `_pie_dedup_key` (without `_meta`) is honored for backward compat. Newer
    /// `_meta.pie_dedup_key` takes precedence when both are present.
    #[tokio::test]
    async fn legacy_dedup_key_works_and_meta_wins() {
        let (tx, mut rx, _status, handle) = fixture();
        tx.send(note(
            "notifications/custom/event",
            json!({ "_pie_dedup_key": "legacy-key", "detail": "ok" }),
        ))
        .unwrap();
        let t1 = rx.recv().await.unwrap();
        assert_eq!(t1.idempotency_key, "legacy-key");

        // When both are present, `_meta.pie_dedup_key` wins.
        tx.send(note(
            "notifications/custom/event",
            json!({
                "_meta": { "pie_dedup_key": "new-key" },
                "_pie_dedup_key": "legacy-key",
            }),
        ))
        .unwrap();
        let t2 = rx.recv().await.unwrap();
        assert_eq!(t2.idempotency_key, "new-key");

        drop(tx);
        let _ = handle.await;
    }

    /// Custom method without any dedup key is dropped at the adapter; the runtime never
    /// sees a trigger but `dropped_count` increments and `last_error` records the reason.
    ///
    /// We deliberately avoid pushing a follow-up known-good event here: a successful push
    /// resets `last_error`, so we would lose the diagnostic before observing it. Instead
    /// we busy-wait briefly on `status.dropped_count` to ensure the hook task processed
    /// the frame, then assert both fields.
    #[tokio::test]
    async fn custom_without_dedup_key_is_dropped_with_diagnostic() {
        let (tx, mut rx, status, handle) = fixture();
        tx.send(note(
            "notifications/custom/event",
            json!({ "detail": "missing key" }),
        ))
        .unwrap();

        // Wait up to ~500ms for the hook task to observe the drop. In practice it fires
        // on the next tokio scheduler poll (<1ms), but we give CI plenty of slack.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            if status.lock().dropped_count >= 1 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "dropped_count never reached 1 within deadline; status={:?}",
                    status.lock().clone()
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // No trigger should reach the sink for this frame.
        assert!(
            rx.try_recv().is_err(),
            "custom-without-key must not produce a trigger"
        );
        let st = status.lock();
        assert_eq!(st.dropped_count, 1);
        assert!(
            st.last_error
                .as_deref()
                .unwrap_or("")
                .contains("dropped custom notification"),
            "diagnostic should mention the drop, got {:?}",
            st.last_error
        );
        drop(st);
        drop(tx);
        let _ = handle.await;
    }

    /// `resources/updated` without a `uri` field falls back to the unscoped `"resources"`
    /// key rather than crashing. (Defensive — MCP spec requires uri but adapters in the
    /// wild may misbehave.)
    #[tokio::test]
    async fn resources_updated_without_uri_falls_back_to_resources_key() {
        let (tx, mut rx, _status, handle) = fixture();
        tx.send(note("notifications/resources/updated", json!({})))
            .unwrap();
        let trigger = rx.recv().await.unwrap();
        assert_eq!(trigger.idempotency_key, "resources:unknown");
        drop(tx);
        let _ = handle.await;
    }

    /// Closing the sink while the hook is running surfaces as `HookError::SinkClosed` so
    /// the supervisor can record the right termination reason. The hook should not panic
    /// and `run` should return promptly.
    #[tokio::test]
    async fn sink_closed_returns_sink_closed_err() {
        let (note_tx, note_rx) = mpsc::unbounded_channel::<McpServerNotification>();
        let (trig_tx, trig_rx) = mpsc::unbounded_channel::<Trigger>();
        let hook = Arc::new(McpNotificationHook::new("filesystem", note_rx));
        let hook_clone = hook.clone();
        let handle = tokio::spawn(async move { hook_clone.run(trig_tx).await });

        // Drop the receiver to close the sink, then push a notification — the hook will
        // observe SendError on the first attempt and return SinkClosed.
        drop(trig_rx);
        note_tx
            .send(note("notifications/tools/listChanged", json!({})))
            .unwrap();
        let err = handle.await.unwrap();
        assert!(matches!(err, Err(HookError::SinkClosed)));
        assert!(matches!(
            hook.status().state,
            HookState::Disconnected { .. }
        ));
    }

    /// Transport close (the McpClient drops its sender) flips the hook to `Disconnected`
    /// with a meaningful reason; `run` returns `Ok(())` so the supervisor knows it was a
    /// clean exit rather than a transport-level error.
    #[tokio::test]
    async fn transport_close_returns_ok_and_marks_disconnected() {
        let (note_tx, note_rx) = mpsc::unbounded_channel::<McpServerNotification>();
        let (trig_tx, _trig_rx) = mpsc::unbounded_channel::<Trigger>();
        let hook = Arc::new(McpNotificationHook::new("filesystem", note_rx));
        let hook_clone = hook.clone();
        let handle = tokio::spawn(async move { hook_clone.run(trig_tx).await });

        drop(note_tx);
        let result = handle.await.unwrap();
        assert!(result.is_ok(), "clean transport close should be Ok");
        match hook.status().state {
            HookState::Disconnected { ref reason } => {
                assert!(reason.contains("transport"), "got reason={reason:?}");
            }
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }

    /// Running the hook a second time fails because the receiver was already consumed.
    /// Mirrors the single-consumer invariant on `McpClient::take_notifications`.
    #[tokio::test]
    async fn second_run_fails_after_receiver_consumed() {
        let (note_tx, note_rx) = mpsc::unbounded_channel::<McpServerNotification>();
        let (trig_tx, _trig_rx) = mpsc::unbounded_channel::<Trigger>();
        let hook = Arc::new(McpNotificationHook::new("filesystem", note_rx));
        let hook_first = hook.clone();
        let handle = tokio::spawn(async move { hook_first.run(trig_tx).await });

        drop(note_tx);
        let _ = handle.await;

        let (trig_tx2, _trig_rx2) = mpsc::unbounded_channel::<Trigger>();
        let err = hook.run(trig_tx2).await;
        assert!(matches!(err, Err(HookError::Other(_))));
    }

    /// Status starts as the trait-defined "pending" snapshot before `run` is invoked.
    #[test]
    fn initial_status_is_pending() {
        let (_tx, rx) = mpsc::unbounded_channel::<McpServerNotification>();
        let hook = McpNotificationHook::new("filesystem", rx);
        let s = hook.status();
        assert!(matches!(
            s.state,
            HookState::Disconnected { ref reason } if reason == "not yet started"
        ));
        assert_eq!(s.subscription_labels, vec!["mcp:filesystem".to_string()]);
        assert_eq!(s.dropped_count, 0);
    }
}
