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
//! Mapping rules (RFC 1 §4.2.3 + PR #35 / PR #56 QA notes):
//!
//! | MCP method                            | runtime idempotency key                          | replacement      |
//! |---------------------------------------|--------------------------------------------------|------------------|
//! | `notifications/tools/listChanged`     | `mcp:{server}:tools`                             | `LatestReplaces` |
//! | `notifications/resources/listChanged` | `mcp:{server}:resources`                         | `LatestReplaces` |
//! | `notifications/resources/updated`     | `mcp:{server}:resources:{safe-uri-or-hash}`      | `LatestReplaces` |
//! | `notifications/prompts/listChanged`   | `mcp:{server}:prompts`                           | `LatestReplaces` |
//! | custom `notifications/*`              | `mcp:{server}:custom:{safe-key-or-hash}`         | `Drop`           |
//!
//! Two layers of namespacing:
//!
//! - **`mcp:{server_name}:` prefix** keeps the same intrinsic key from two MCP servers
//!   (e.g. both `tools/listChanged`) from dedup-cancelling each other in the runtime's
//!   global dedup window (PR #56 QA blocker #1).
//! - **`custom:` segment** keeps user-supplied dedup keys in their own slot within a
//!   server, so a custom notification with `_meta.pie_dedup_key = "tools"` cannot collide
//!   with the built-in `tools/listChanged` row (PR #56 QA blocker #2). Built-in subsystems
//!   (`tools` / `resources` / `prompts`) own the un-prefixed slot; everything user-provided
//!   lives under `custom:`.
//!
//! A custom notification that provides neither dedup key form is dropped at the adapter
//! with `dropped_count += 1`; the runtime never sees it. Adapters do **not** dedup
//! themselves — the runtime owns the dedup window. We surface a stable, server-scoped key
//! per source/method so the runtime can do its job.
//!
//! Privacy contract: `payload_visibility = Local` means the full `params` blob is dropped
//! before persistence; only `payload_summary` survives into the audit. The summary is
//! method-name-only for custom / unknown notifications (PR #56 QA blocker) — a sentinel
//! secret tucked into a custom notification's params must never end up in the persisted
//! `Custom { custom_type: "trigger" }` audit entry. Adapters that genuinely need
//! human-readable per-event detail can opt in via `_meta.pie_summary: "<text>"`, capped and
//! redacted before persistence. Unsafe resource URIs / custom dedup keys are hashed before
//! they enter persisted trigger audit fields.

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
use sha2::{Digest, Sha256};
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
    /// Atomic-cheap status snapshot. Re-read frequently by `/triggers sources`; we keep it
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
            // Bookkeeping after successful push so `/triggers sources` shows the latest event
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
    let (idempotency_key, replacement_policy) = idempotency_for(server_name, &n.method, &n.params)?;
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
        payload: None,
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

pub(crate) fn safe_display(value: &str, cap: usize) -> String {
    let redacted = redact_notification_text(value).replace('\n', " ");
    truncate_chars(&redacted, cap)
}

fn redact_notification_text(value: &str) -> String {
    value
        .split_whitespace()
        .map(|part| {
            let lower = part.to_ascii_lowercase();
            if lower.starts_with("hub_agent_")
                || lower.starts_with("hub_hs_")
                || lower.starts_with("hub_ep_")
                || lower.starts_with("sk-")
                || lower.contains("bearer")
                || lower.contains("token")
            {
                "[redacted]"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_chars(value: &str, cap: usize) -> String {
    if value.chars().count() <= cap {
        return value.to_string();
    }
    let mut out = value
        .chars()
        .take(cap.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

/// Derive `(idempotency_key, replacement_policy)` for a given method + params per RFC 1
/// §4.2.3 / PR #35 QA follow-up. Returns `None` for custom methods that don't supply a
/// dedup key — the caller drops those at the adapter with diagnostics.
///
/// Every key is namespaced with `mcp:{server_name}:` so two MCP servers that legitimately
/// emit the same intrinsic key (both `tools/listChanged`, both with the same custom
/// `_meta.pie_dedup_key`) do not dedup each other in the runtime. The runtime dedup window
/// is global per harness; namespacing at the adapter is the only place we can prevent
/// cross-server collisions.
fn idempotency_for(
    server_name: &str,
    method: &str,
    params: &serde_json::Value,
) -> Option<(String, ReplacementPolicy)> {
    let prefix = format!("mcp:{server_name}:");
    match method {
        "notifications/tools/listChanged" => {
            Some((format!("{prefix}tools"), ReplacementPolicy::LatestReplaces))
        }
        "notifications/resources/listChanged" => Some((
            format!("{prefix}resources"),
            ReplacementPolicy::LatestReplaces,
        )),
        "notifications/prompts/listChanged" => Some((
            format!("{prefix}prompts"),
            ReplacementPolicy::LatestReplaces,
        )),
        "notifications/resources/updated" => {
            // Per-URI keying so multiple updates to different resources don't collapse into
            // one event. If the server omitted `uri` (shouldn't happen per MCP spec but
            // defensive), fall back to the unscoped `"resources"` key.
            let uri = params
                .get("uri")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            Some((
                format!("{prefix}resources:{}", safe_idempotency_segment(uri)),
                ReplacementPolicy::LatestReplaces,
            ))
        }
        _ => {
            // Custom notification — require an explicit dedup key. Prefer `_meta.pie_dedup_key`
            // (canonical going forward) over `_pie_dedup_key` (legacy, kept for adapters
            // already in the wild). Either form is treated as `Drop` semantics: every
            // explicit key represents one logical event, no replacement.
            //
            // The `custom:` segment after the server prefix keeps custom keys in their
            // own namespace within the server so a user supplying
            // `_meta.pie_dedup_key = "tools"` does NOT collide with the built-in
            // `tools/listChanged` row. Built-in subsystems (`tools` / `resources` /
            // `prompts`) own the un-prefixed slot; everything user-provided lives under
            // `custom:`. PR #56 QA re-review blocker.
            extract_dedup_key(params).map(|k| {
                (
                    format!("{prefix}custom:{}", safe_idempotency_segment(&k)),
                    ReplacementPolicy::Drop,
                )
            })
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

pub(crate) fn safe_idempotency_segment(value: &str) -> String {
    let redacted = redact_notification_text(value);
    let has_sensitive_text = redacted != value;
    let is_unbounded = value.chars().count() > 200;
    let has_control_chars = value.chars().any(|ch| ch.is_control());
    if has_sensitive_text || is_unbounded || has_control_chars {
        let digest = Sha256::digest(value.as_bytes());
        return format!("hash:{}", hex::encode(&digest[..6]));
    }
    value.to_string()
}

/// Render a short human-readable summary for `payload_summary`. Capped well below the
/// runtime 4 KiB persistence cap; the runtime will still re-truncate if a future caller
/// emits more.
///
/// Privacy contract (RFC 0 §3.2.2 / RFC 1 §4.2.3 + PR #56 QA follow-up): the hook is
/// configured with `payload_visibility = Local`, which means `payload` is dropped and only
/// `payload_summary` survives into the persisted audit. So this function must not echo
/// arbitrary params content — a sentinel secret in a custom notification's params field
/// would otherwise persist into the trigger audit entry.
///
/// The contract: only **method name** plus bounded/redacted display metadata (`uri` for
/// `resources/updated`) appear in the summary. Adapters that need per-event detail must opt
/// in via `_meta.pie_summary: "<human-safe text>"`; we still cap and redact that string
/// because it is MCP-server-controlled input.
fn render_summary(method: &str, params: &serde_json::Value) -> Option<String> {
    match method {
        // `uri` is part of the MCP resource identity, but an MCP server can still stuff
        // token-like values into it. Keep useful display metadata while applying the same
        // redaction as other user-visible/audit-visible strings.
        "notifications/resources/updated" => {
            if let Some(uri) = params.get("uri").and_then(|v| v.as_str()) {
                Some(format!("{method} uri={}", safe_display(uri, 200)))
            } else {
                Some(method.to_string())
            }
        }
        // Standard listChanged events have no per-event detail worth rendering.
        "notifications/tools/listChanged"
        | "notifications/resources/listChanged"
        | "notifications/prompts/listChanged" => Some(method.to_string()),
        // Custom / unknown methods: NEVER serialize arbitrary params. Allow explicit
        // opt-in via `_meta.pie_summary`; otherwise just the method name. This is what
        // prevents secrets in a server's custom params from leaking into the audit.
        _ => {
            if let Some(s) = params
                .get("_meta")
                .and_then(|m| m.get("pie_summary"))
                .and_then(|v| v.as_str())
            {
                Some(format!("{method} {}", safe_display(s, 200)))
            } else {
                Some(method.to_string())
            }
        }
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

    /// `tools/listChanged` → idempotency `"mcp:{server}:tools"` + `LatestReplaces`, no
    /// payload, MCP source kind, server name + method threaded through. The `mcp:{server}:`
    /// prefix is what prevents two MCP servers' identical method-local keys from
    /// dedup-cancelling each other in the runtime.
    #[tokio::test]
    async fn tools_list_changed_maps_to_latest_replaces() {
        let (tx, mut rx, _status, handle) = fixture();
        tx.send(note("notifications/tools/listChanged", json!({})))
            .unwrap();
        let trigger = rx.recv().await.expect("trigger should arrive");
        assert_eq!(trigger.idempotency_key, "mcp:filesystem:tools");
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
    /// Key is `"mcp:{server}:resources:{uri}"`.
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
        assert_eq!(t1.idempotency_key, "mcp:filesystem:resources:file:///a.md");
        assert_eq!(t2.idempotency_key, "mcp:filesystem:resources:file:///b.md");
        assert_ne!(t1.idempotency_key, t2.idempotency_key);
        drop(tx);
        let _ = handle.await;
    }

    /// Custom method with `_meta.pie_dedup_key` is accepted with `Drop` policy. Key gets
    /// both the server prefix AND the `custom:` segment so user-supplied keys cannot
    /// collide with the built-in `tools` / `resources` / `prompts` slots.
    #[tokio::test]
    async fn custom_with_meta_dedup_key_passes_through() {
        let (tx, mut rx, _status, handle) = fixture();
        tx.send(note(
            "notifications/custom/event",
            json!({ "_meta": { "pie_dedup_key": "build-42" }, "detail": "ok" }),
        ))
        .unwrap();
        let trigger = rx.recv().await.unwrap();
        assert_eq!(trigger.idempotency_key, "mcp:filesystem:custom:build-42");
        assert_eq!(trigger.replacement_policy, ReplacementPolicy::Drop);
        drop(tx);
        let _ = handle.await;
    }

    /// Legacy `_pie_dedup_key` (without `_meta`) is honored for backward compat. Newer
    /// `_meta.pie_dedup_key` takes precedence when both are present. Both forms still get
    /// the `mcp:{server}:custom:` prefix.
    #[tokio::test]
    async fn legacy_dedup_key_works_and_meta_wins() {
        let (tx, mut rx, _status, handle) = fixture();
        tx.send(note(
            "notifications/custom/event",
            json!({ "_pie_dedup_key": "legacy-key", "detail": "ok" }),
        ))
        .unwrap();
        let t1 = rx.recv().await.unwrap();
        assert_eq!(t1.idempotency_key, "mcp:filesystem:custom:legacy-key");

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
        assert_eq!(t2.idempotency_key, "mcp:filesystem:custom:new-key");

        drop(tx);
        let _ = handle.await;
    }

    /// Dedup keys are persisted inside trigger audit records. A malicious MCP server must
    /// not be able to smuggle token-like material into that audit via `_meta.pie_dedup_key`;
    /// hash unsafe keys while preserving stable dedup semantics.
    #[tokio::test]
    async fn custom_dedup_key_redacts_secret_like_text_in_idempotency_key() {
        let (tx, mut rx, _status, handle) = fixture();
        tx.send(note(
            "notifications/custom/payload",
            json!({ "_meta": { "pie_dedup_key": "hub_agent_secret_should_not_persist" } }),
        ))
        .unwrap();
        let trigger = rx.recv().await.unwrap();
        assert!(
            trigger
                .idempotency_key
                .starts_with("mcp:filesystem:custom:hash:"),
            "{}",
            trigger.idempotency_key
        );
        assert!(
            !trigger
                .idempotency_key
                .contains("hub_agent_secret_should_not_persist"),
            "{}",
            trigger.idempotency_key
        );
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

    /// `resources/updated` without a `uri` field falls back to `resources:unknown` (still
    /// server-namespaced) rather than crashing. Defensive — MCP spec requires uri but
    /// adapters in the wild may misbehave.
    #[tokio::test]
    async fn resources_updated_without_uri_falls_back_to_resources_key() {
        let (tx, mut rx, _status, handle) = fixture();
        tx.send(note("notifications/resources/updated", json!({})))
            .unwrap();
        let trigger = rx.recv().await.unwrap();
        assert_eq!(trigger.idempotency_key, "mcp:filesystem:resources:unknown");
        drop(tx);
        let _ = handle.await;
    }

    /// Two MCP servers emitting the **same** method-local key (`tools` / `resources` /
    /// per-URI / custom `pie_dedup_key`) must produce **distinct** runtime
    /// `idempotency_key`s so the harness dedup window does not collapse one server's event
    /// onto the other's. The fix: prefix every key with `mcp:{server_name}:` at the
    /// adapter. PR #56 QA blocker.
    #[tokio::test]
    async fn idempotency_keys_are_namespaced_per_server() {
        let (note_tx_a, note_rx_a) = mpsc::unbounded_channel::<McpServerNotification>();
        let (trig_tx_a, mut trig_rx_a) = mpsc::unbounded_channel::<Trigger>();
        let hook_a = Arc::new(McpNotificationHook::new("server-a", note_rx_a));
        let driver_a = hook_a.clone();
        let handle_a = tokio::spawn(async move { driver_a.run(trig_tx_a).await });

        let (note_tx_b, note_rx_b) = mpsc::unbounded_channel::<McpServerNotification>();
        let (trig_tx_b, mut trig_rx_b) = mpsc::unbounded_channel::<Trigger>();
        let hook_b = Arc::new(McpNotificationHook::new("server-b", note_rx_b));
        let driver_b = hook_b.clone();
        let handle_b = tokio::spawn(async move { driver_b.run(trig_tx_b).await });

        // Both servers emit identical `tools/listChanged` — without the prefix they would
        // collide as a single dedup-window entry. Same exercise for a per-URI key, and a
        // custom `_meta.pie_dedup_key`.
        note_tx_a
            .send(note("notifications/tools/listChanged", json!({})))
            .unwrap();
        note_tx_b
            .send(note("notifications/tools/listChanged", json!({})))
            .unwrap();
        note_tx_a
            .send(note(
                "notifications/resources/updated",
                json!({ "uri": "file:///shared.md" }),
            ))
            .unwrap();
        note_tx_b
            .send(note(
                "notifications/resources/updated",
                json!({ "uri": "file:///shared.md" }),
            ))
            .unwrap();
        note_tx_a
            .send(note(
                "notifications/custom/event",
                json!({ "_meta": { "pie_dedup_key": "shared-build-1" } }),
            ))
            .unwrap();
        note_tx_b
            .send(note(
                "notifications/custom/event",
                json!({ "_meta": { "pie_dedup_key": "shared-build-1" } }),
            ))
            .unwrap();

        // Server A emitted 3 triggers; server B emitted 3 triggers. All 6 keys must be
        // distinct; specifically each pair must differ in the `mcp:{server}:` prefix.
        let mut a_keys = Vec::new();
        for _ in 0..3 {
            a_keys.push(trig_rx_a.recv().await.unwrap().idempotency_key);
        }
        let mut b_keys = Vec::new();
        for _ in 0..3 {
            b_keys.push(trig_rx_b.recv().await.unwrap().idempotency_key);
        }
        for k in &a_keys {
            assert!(
                k.starts_with("mcp:server-a:"),
                "server-a key missing prefix: {k}"
            );
        }
        for k in &b_keys {
            assert!(
                k.starts_with("mcp:server-b:"),
                "server-b key missing prefix: {k}"
            );
        }
        // Pairwise: cross-server keys are never equal.
        for ka in &a_keys {
            for kb in &b_keys {
                assert_ne!(
                    ka, kb,
                    "cross-server keys collided — runtime would dedup them as duplicates"
                );
            }
        }

        drop(note_tx_a);
        drop(note_tx_b);
        let _ = handle_a.await;
        let _ = handle_b.await;
    }

    /// Within a single server, a user-supplied custom dedup key (`_meta.pie_dedup_key`)
    /// must not collide with the built-in `tools` / `resources` / `prompts` slots. The
    /// adversarial case: a custom notification with `_meta.pie_dedup_key = "tools"`. Before
    /// the `custom:` segment fix both events produced `mcp:filesystem:tools` and the
    /// runtime would dedup them as duplicates; afterwards the custom key sits under
    /// `mcp:filesystem:custom:tools`. PR #56 QA re-review blocker.
    #[tokio::test]
    async fn custom_key_cannot_collide_with_builtin_within_same_server() {
        let (tx, mut rx, _status, handle) = fixture();
        // Built-in path.
        tx.send(note("notifications/tools/listChanged", json!({})))
            .unwrap();
        // Adversarial custom path: user picked the exact string the built-in uses.
        tx.send(note(
            "notifications/custom/payload",
            json!({ "_meta": { "pie_dedup_key": "tools" } }),
        ))
        .unwrap();
        // Same adversarial collision for `resources` and `prompts`.
        tx.send(note(
            "notifications/custom/payload",
            json!({ "_meta": { "pie_dedup_key": "resources" } }),
        ))
        .unwrap();
        tx.send(note(
            "notifications/custom/payload",
            json!({ "_meta": { "pie_dedup_key": "prompts" } }),
        ))
        .unwrap();
        // And one that mimics the `resources:{uri}` shape of `resources/updated`.
        tx.send(note(
            "notifications/custom/payload",
            json!({ "_meta": { "pie_dedup_key": "resources:file:///x.md" } }),
        ))
        .unwrap();

        let t1 = rx.recv().await.unwrap();
        let t2 = rx.recv().await.unwrap();
        let t3 = rx.recv().await.unwrap();
        let t4 = rx.recv().await.unwrap();
        let t5 = rx.recv().await.unwrap();

        assert_eq!(t1.idempotency_key, "mcp:filesystem:tools");
        assert_eq!(t2.idempotency_key, "mcp:filesystem:custom:tools");
        assert_eq!(t3.idempotency_key, "mcp:filesystem:custom:resources");
        assert_eq!(t4.idempotency_key, "mcp:filesystem:custom:prompts");
        assert_eq!(
            t5.idempotency_key,
            "mcp:filesystem:custom:resources:file:///x.md"
        );

        // Pairwise distinct — none of the four custom keys equals the built-in or each other.
        let keys = [
            &t1.idempotency_key,
            &t2.idempotency_key,
            &t3.idempotency_key,
            &t4.idempotency_key,
            &t5.idempotency_key,
        ];
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                assert_ne!(
                    a, b,
                    "custom/built-in same-server key collision: {a} vs {b}"
                );
            }
        }

        drop(tx);
        let _ = handle.await;
    }

    /// `payload_visibility = Local` means the full `payload` is dropped; only
    /// `payload_summary` survives into the persisted audit. For custom / unknown
    /// notifications the adapter MUST NOT echo arbitrary params content into the summary
    /// because params may contain secrets the server tucked in (API tokens, file contents,
    /// PII, etc.). PR #56 QA blocker.
    #[tokio::test]
    async fn custom_method_summary_does_not_leak_params_content() {
        let (tx, mut rx, _status, handle) = fixture();
        // Sentinel string the test would only find in the summary if `render_summary`
        // serialized arbitrary params.
        let sentinel = "TOKEN_SENTINEL_SHOULD_NOT_APPEAR_IN_AUDIT";
        tx.send(note(
            "notifications/custom/secret-bearing",
            json!({
                "_meta": { "pie_dedup_key": "evt-1" },
                "secret": sentinel,
                "nested": { "more_secret": sentinel },
            }),
        ))
        .unwrap();
        let trigger = rx.recv().await.unwrap();
        let summary = trigger.payload_summary.unwrap_or_default();
        assert!(
            !summary.contains(sentinel),
            "summary leaked params content: {summary}"
        );
        assert_eq!(
            summary, "notifications/custom/secret-bearing",
            "custom-method summary must reduce to bare method name (no params echo)"
        );
        drop(tx);
        let _ = handle.await;
    }

    /// Adapters that need per-event human-readable detail for a custom notification can
    /// opt in via `_meta.pie_summary: "<text>"`. The opt-in field is treated as
    /// declaratively-safe by the server and surfaces into the summary capped at 200 chars.
    /// Counterpart to the secret-leak test above.
    #[tokio::test]
    async fn custom_method_pie_summary_opt_in_appears_in_summary() {
        let (tx, mut rx, _status, handle) = fixture();
        tx.send(note(
            "notifications/custom/build-finished",
            json!({
                "_meta": {
                    "pie_dedup_key": "build-99",
                    "pie_summary": "build #99 finished: 3 tests failed",
                },
                "internal_token": "should-not-appear",
            }),
        ))
        .unwrap();
        let trigger = rx.recv().await.unwrap();
        let summary = trigger.payload_summary.unwrap_or_default();
        assert!(
            summary.contains("build #99 finished"),
            "opt-in pie_summary should surface: {summary}"
        );
        assert!(
            !summary.contains("should-not-appear"),
            "params outside of pie_summary must not leak: {summary}"
        );
        drop(tx);
        let _ = handle.await;
    }

    /// `_meta.pie_summary` is opt-in display text, but it is still MCP-server-controlled
    /// input that becomes `trigger.payload_summary` and is persisted in trigger audit.
    /// Redact token-like material before the runtime ever sees it.
    #[tokio::test]
    async fn custom_method_pie_summary_opt_in_redacts_secret_like_text() {
        let (tx, mut rx, _status, handle) = fixture();
        tx.send(note(
            "notifications/custom/build-finished",
            json!({
                "_meta": {
                    "pie_dedup_key": "build-100",
                    "pie_summary": "build leaked hub_agent_secret_should_not_persist token=sk-secret",
                },
            }),
        ))
        .unwrap();
        let trigger = rx.recv().await.unwrap();
        let summary = trigger.payload_summary.unwrap_or_default();
        assert!(summary.contains("notifications/custom/build-finished"));
        assert!(summary.contains("[redacted]"), "{summary}");
        assert!(!summary.contains("hub_agent_secret_should_not_persist"));
        assert!(!summary.contains("sk-secret"));
        drop(tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn agent_message_notification_is_generic_custom_mcp_trigger() {
        let trigger = map_notification(
            "remote-agent",
            &note(
                "notifications/agent_message",
                json!({
                    "_meta": {
                        "pie_dedup_key": "note-1",
                        "pie_summary": "message ready",
                        "receiver_agent_id": "11111111-1111-4111-8111-111111111111",
                        "sender_agent_id": "22222222-2222-4222-8222-222222222222",
                    },
                    "sender": "@alice@example",
                    "payload": {
                        "secret": "hub_agent_secret_should_not_leave_local_payload"
                    },
                }),
            ),
        )
        .expect("custom notification with dedup key should map");

        assert_eq!(trigger.source_label, "mcp:remote-agent");
        assert_eq!(trigger.event_label, "notifications/agent_message");
        assert_eq!(trigger.payload_visibility, PayloadVisibility::Local);
        assert!(
            trigger.payload.is_none(),
            "must not build special-case binding payload"
        );
        let summary = trigger.payload_summary.unwrap_or_default();
        assert!(summary.contains("message ready"), "{summary}");
        assert!(!summary.contains("hub_agent_secret_should_not_leave_local_payload"));
    }

    /// Known `resources/updated` keeps the `uri` in the summary — `uri` is part of the
    /// public resource address per MCP spec, not arbitrary params. Pins that we don't
    /// over-correct and drop legitimate detail.
    #[tokio::test]
    async fn resources_updated_summary_includes_uri() {
        let (tx, mut rx, _status, handle) = fixture();
        tx.send(note(
            "notifications/resources/updated",
            json!({ "uri": "file:///proj/README.md", "rev": 5 }),
        ))
        .unwrap();
        let trigger = rx.recv().await.unwrap();
        let summary = trigger.payload_summary.unwrap_or_default();
        assert!(summary.contains("uri=file:///proj/README.md"), "{summary}");
        // Defensive: `rev` is a non-spec field and must not leak.
        assert!(
            !summary.contains("rev"),
            "non-spec params field leaked into summary: {summary}"
        );
        drop(tx);
        let _ = handle.await;
    }

    /// Resource URIs are allowed display metadata, but they can still be hostile strings
    /// from an MCP server. Keep legitimate paths while redacting token-like URI values.
    #[tokio::test]
    async fn resources_updated_summary_redacts_secret_like_uri() {
        let (tx, mut rx, _status, handle) = fixture();
        tx.send(note(
            "notifications/resources/updated",
            json!({ "uri": "file:///proj/README.md?token=hub_agent_secret_should_not_persist" }),
        ))
        .unwrap();
        let trigger = rx.recv().await.unwrap();
        let summary = trigger.payload_summary.unwrap_or_default();
        assert!(
            summary.contains("notifications/resources/updated"),
            "{summary}"
        );
        assert!(summary.contains("uri=[redacted]"), "{summary}");
        assert!(!summary.contains("hub_agent_secret_should_not_persist"));
        assert!(
            !trigger
                .idempotency_key
                .contains("hub_agent_secret_should_not_persist"),
            "{}",
            trigger.idempotency_key
        );
        assert!(
            trigger.idempotency_key.contains("resources:hash:"),
            "{}",
            trigger.idempotency_key
        );
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
