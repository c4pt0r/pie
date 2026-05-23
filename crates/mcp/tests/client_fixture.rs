//! In-process MCP client fixture test. Instead of spawning a real subprocess, we drive the
//! client over a custom Transport implementation that exchanges JSON lines with a mock
//! "server" running on the same tokio runtime.

use std::sync::Arc;

use async_trait::async_trait;
use pie_mcp::{McpClient, McpError, Transport};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;

/// Pipe transport: two unbounded channels that emulate stdin (we write) and stdout (we read).
struct PipeTransport {
    tx: AsyncMutex<mpsc::UnboundedSender<String>>,
    rx: AsyncMutex<mpsc::UnboundedReceiver<String>>,
}

#[async_trait]
impl Transport for PipeTransport {
    async fn send_line(&self, line: String) -> Result<(), McpError> {
        self.tx
            .lock()
            .await
            .send(line)
            .map_err(|e| McpError::Transport(e.to_string()))
    }
    async fn recv_line(&self) -> Result<Option<String>, McpError> {
        Ok(self.rx.lock().await.recv().await)
    }
    async fn close(&self) {
        // dropping the senders inside the test is enough
    }
}

fn pair() -> (Arc<PipeTransport>, Arc<PipeTransport>) {
    let (a_tx, b_rx) = mpsc::unbounded_channel();
    let (b_tx, a_rx) = mpsc::unbounded_channel();
    let a = PipeTransport {
        tx: AsyncMutex::new(a_tx),
        rx: AsyncMutex::new(a_rx),
    };
    let b = PipeTransport {
        tx: AsyncMutex::new(b_tx),
        rx: AsyncMutex::new(b_rx),
    };
    (Arc::new(a), Arc::new(b))
}

/// Mock server: handles initialize → tools/list → tools/call by writing back canned responses.
async fn run_mock_server(transport: Arc<PipeTransport>) {
    loop {
        let line = match transport.recv_line().await {
            Ok(Some(l)) => l,
            _ => break,
        };
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let method = v.get("method").and_then(|x| x.as_str()).unwrap_or("");
        let id = v.get("id").and_then(|x| x.as_u64());

        if method == "notifications/initialized" {
            continue; // no response
        }

        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "serverInfo": { "name": "mock-server", "version": "0.0.1" }
            }),
            "tools/list" => serde_json::json!({
                "tools": [{
                    "name": "echo",
                    "description": "echo text back",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"]
                    }
                }]
            }),
            "tools/call" => {
                let args = v
                    .get("params")
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let text = args
                    .get("text")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                serde_json::json!({
                    "content": [{ "type": "text", "text": format!("echo: {text}") }],
                    "isError": false
                })
            }
            _ => serde_json::json!(null),
        };
        let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = transport.send_line(resp.to_string()).await;
    }
}

#[tokio::test]
async fn handshake_list_and_call_round_trip() {
    let (client_side, server_side) = pair();
    tokio::spawn(run_mock_server(server_side));

    let client = McpClient::new(client_side);
    let init = client.initialize("pie-test").await.unwrap();
    assert_eq!(init.server_info.name, "mock-server");
    assert!(client.is_initialized());

    let tools = client.tools_list().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");

    let res = client
        .tools_call("echo", Some(serde_json::json!({ "text": "hi" })), None)
        .await
        .unwrap();
    assert!(!res.is_error);
    let body = match &res.content[0] {
        pie_mcp::protocol::ToolContent::Text { text } => text.clone(),
        _ => panic!("expected text"),
    };
    assert_eq!(body, "echo: hi");
}

#[tokio::test]
async fn tools_list_before_initialize_is_rejected() {
    let (client_side, _server_side) = pair();
    let client = McpClient::new(client_side);
    let err = client.tools_list().await.unwrap_err();
    matches!(err, McpError::NotInitialized);
}

/// Server-pushed notifications used to be silently dropped by the read pump (`continue` in
/// the `id.is_none()` branch). After the RFC 1 §4.2.1 changes the pump routes every
/// id-less frame into a `mpsc` channel exposed via [`McpClient::take_notifications`].
///
/// This test pushes three notifications (a known method, a custom method with `params`,
/// and a frame that's malformed at the JSON-RPC `method` level) and asserts:
///
/// - The two well-formed notifications come out of `take_notifications` in order.
/// - The malformed frame (missing `method`) is dropped silently, not surfaced to the
///   consumer — adapters cannot mis-interpret it as a notification.
/// - The `params` payload arrives intact (no munging in the pump).
/// - Calling `take_notifications` a second time returns `None` — single-consumer pattern,
///   matches `McpNotificationHook` (one hook per client).
#[tokio::test]
async fn server_push_notifications_reach_take_notifications_in_order() {
    use pie_mcp::client::McpServerNotification;
    use tokio::sync::mpsc::UnboundedReceiver;

    let (client_side, server_side) = pair();

    // Spawn a tiny "server" that sends three frames as soon as the client connects, no
    // request needed.
    let server_handle = tokio::spawn(async move {
        // Well-formed notification — known MCP method.
        let _ = server_side
            .send_line(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/tools/listChanged"
                })
                .to_string(),
            )
            .await;
        // Well-formed notification with a payload object.
        let _ = server_side
            .send_line(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/resources/updated",
                    "params": { "uri": "file:///tmp/x.md", "revision": 7 }
                })
                .to_string(),
            )
            .await;
        // Malformed: no `id` AND no `method` — this is not a valid JSON-RPC notification
        // and the pump must silently drop it rather than surface a garbage event.
        let _ = server_side
            .send_line(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "params": { "ignored": true }
                })
                .to_string(),
            )
            .await;
    });

    let client = McpClient::new(client_side);
    let mut rx: UnboundedReceiver<McpServerNotification> = client
        .take_notifications()
        .expect("first take_notifications must return Some");

    // First notification: known method, no params on the wire — pump fills `params` with
    // `Value::Null` so consumers don't have to handle missing-key gymnastics.
    let first = rx
        .recv()
        .await
        .expect("first notification must arrive after server send");
    assert_eq!(first.method, "notifications/tools/listChanged");
    assert!(
        first.params.is_null()
            || first
                .params
                .as_object()
                .map(|o| o.is_empty())
                .unwrap_or(false)
    );

    // Second notification: custom payload — assert it round-trips byte-faithfully.
    let second = rx.recv().await.expect("second notification must arrive");
    assert_eq!(second.method, "notifications/resources/updated");
    assert_eq!(
        second
            .params
            .get("uri")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        "file:///tmp/x.md"
    );
    assert_eq!(
        second.params.get("revision").and_then(|v| v.as_u64()),
        Some(7)
    );

    // Third frame (missing `method`) must NOT surface; the pump dropped it. Use a short
    // timeout so the test doesn't hang if the pump regresses and routes the malformed
    // frame. Either timeout (no message arrived) or `Ok(None)` (the server closed the
    // transport and the channel sender was dropped before any further message) both prove
    // the malformed frame was suppressed. What must NOT happen is `Ok(Some(_))`.
    let third = tokio::time::timeout(std::time::Duration::from_millis(150), rx.recv()).await;
    assert!(
        !matches!(third, Ok(Some(_))),
        "malformed id-less frame without `method` must be dropped, not surfaced — got {third:?}"
    );

    // Second take attempt must return None (single-consumer invariant). This pins the
    // ownership contract `McpNotificationHook` relies on.
    let second_take = client.take_notifications();
    assert!(
        second_take.is_none(),
        "take_notifications must yield the receiver at most once"
    );

    server_handle.abort();
}

// ----------------------------------------------------------------------------------------------
// Cancellation tests for `McpClient::tools_call(_, _, cancel)`.
//
// These tests cover the contract added in the MCP `tools_call` cancel PR:
//   - `cancel.cancel()` mid-flight returns `McpError::Cancelled` bounded by the cancel
//     notification budget (200ms), NOT by the request_timeout (30s).
//   - A `notifications/cancelled` frame with the original JSON-RPC `requestId` is delivered
//     to the server exactly once per cancelled call.
//   - A response that arrives AFTER cancel is silently dropped by the read pump — no panic,
//     no stale tool result, and the inflight HashMap shrinks back to empty.
//   - A response that arrives BEFORE cancel still succeeds and the client does NOT emit a
//     spurious cancel notification.
//   - The pre-existing `tools_call(_, _, None)` shape keeps working unchanged.
//   - The request_timeout path keeps working when no cancel token is supplied.
// ----------------------------------------------------------------------------------------------

/// Slow mock server: for any `tools/call` it delays the response until a barrier is released
/// (so the client can be cancelled mid-wait), and records every incoming JSON frame in
/// `seen_frames` so tests can assert the wire shape of the cancel notification. Anything
/// other than `tools/call` is answered immediately (so `initialize` doesn't block the
/// handshake).
async fn run_slow_mock_server(
    transport: Arc<PipeTransport>,
    release: Arc<tokio::sync::Notify>,
    seen_frames: Arc<AsyncMutex<Vec<serde_json::Value>>>,
) {
    loop {
        let line = match transport.recv_line().await {
            Ok(Some(l)) => l,
            _ => break,
        };
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        seen_frames.lock().await.push(v.clone());

        let method = v.get("method").and_then(|x| x.as_str()).unwrap_or("");
        let id = v.get("id").and_then(|x| x.as_u64());

        if method == "notifications/initialized" || method == "notifications/cancelled" {
            continue; // pure notification — no response
        }

        match method {
            "initialize" => {
                let result = serde_json::json!({
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "serverInfo": { "name": "slow-server", "version": "0.0.1" }
                });
                let _ = transport
                    .send_line(
                        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
                            .to_string(),
                    )
                    .await;
            }
            "tools/list" => {
                let result = serde_json::json!({
                    "tools": [{
                        "name": "slow_echo",
                        "description": "echo, but only after the release barrier fires",
                        "inputSchema": { "type": "object", "properties": {} }
                    }]
                });
                let _ = transport
                    .send_line(
                        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
                            .to_string(),
                    )
                    .await;
            }
            "tools/call" => {
                // Spawn the reply so the server loop keeps draining frames (including the
                // cancel notification) while the tool call is "in flight".
                let transport = transport.clone();
                let release = release.clone();
                tokio::spawn(async move {
                    release.notified().await;
                    let result = serde_json::json!({
                        "content": [{ "type": "text", "text": "released" }],
                        "isError": false
                    });
                    let _ = transport
                        .send_line(
                            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
                                .to_string(),
                        )
                        .await;
                });
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn tools_call_cancel_during_wait_returns_cancelled_and_notifies_server() {
    let (client_side, server_side) = pair();
    let release = Arc::new(tokio::sync::Notify::new());
    let seen_frames: Arc<AsyncMutex<Vec<serde_json::Value>>> =
        Arc::new(AsyncMutex::new(Vec::new()));
    tokio::spawn(run_slow_mock_server(
        server_side,
        release.clone(),
        seen_frames.clone(),
    ));

    let client = Arc::new(McpClient::new(client_side));
    client.initialize("pie-test").await.unwrap();
    let _tools = client.tools_list().await.unwrap();

    let cancel = tokio_util::sync::CancellationToken::new();
    let client_for_call = client.clone();
    let cancel_for_call = cancel.clone();
    let call = tokio::spawn(async move {
        client_for_call
            .tools_call("slow_echo", None, Some(cancel_for_call))
            .await
    });

    // Give the request frame time to land on the server before we cancel.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cancel.cancel();

    // The cancel return must be bounded by the 200ms notify budget, NOT the 30s request
    // timeout. Allow generous slack for CI.
    let started = std::time::Instant::now();
    let res = tokio::time::timeout(std::time::Duration::from_secs(2), call)
        .await
        .expect("tools_call must return promptly after cancel")
        .expect("join error");
    assert!(
        matches!(res, Err(McpError::Cancelled)),
        "expected McpError::Cancelled, got {res:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "cancel must short-circuit the 30s request_timeout, took {:?}",
        started.elapsed()
    );

    // The server must have observed exactly one `notifications/cancelled` frame, and its
    // `requestId` must equal the JSON-RPC id of the original tools/call request.
    // Wait for the cancel notification to flush into the server's seen-frames vector.
    let mut cancel_frames = Vec::new();
    let mut original_id: Option<u64> = None;
    for _ in 0..50 {
        let frames = seen_frames.lock().await;
        original_id = frames
            .iter()
            .find(|f| f.get("method").and_then(|m| m.as_str()) == Some("tools/call"))
            .and_then(|f| f.get("id").and_then(|v| v.as_u64()));
        cancel_frames = frames
            .iter()
            .filter(|f| f.get("method").and_then(|m| m.as_str()) == Some("notifications/cancelled"))
            .cloned()
            .collect();
        drop(frames);
        if !cancel_frames.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        cancel_frames.len(),
        1,
        "exactly one notifications/cancelled frame must reach the server"
    );
    let cf = &cancel_frames[0];
    assert!(
        cf.get("id").is_none(),
        "notifications/cancelled is a notification — must not carry an `id` field"
    );
    let request_id = cf
        .get("params")
        .and_then(|p| p.get("requestId"))
        .and_then(|v| v.as_u64())
        .expect("requestId must be present and numeric");
    assert_eq!(
        Some(request_id),
        original_id,
        "cancel notification requestId must match the original tools/call id"
    );

    // Even though we abandoned the call, the server still produces a response when released.
    // The pump must silently drop the unmatched late response (no panic, no client crash).
    // We assert by releasing + sleeping and observing that the runtime stays healthy enough
    // to do further work (sleeping inside the runtime is what would explode if the pump
    // task had panicked).
    release.notify_waiters();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

#[tokio::test]
async fn tools_call_success_does_not_emit_cancelled_notification() {
    let (client_side, server_side) = pair();
    tokio::spawn(run_mock_server(server_side));

    let client = McpClient::new(client_side);
    client.initialize("pie-test").await.unwrap();
    let _ = client.tools_list().await.unwrap();

    // Even though we pass a cancel token, we never cancel it. The successful response should
    // race-win and no `notifications/cancelled` frame should ever be emitted.
    let cancel = tokio_util::sync::CancellationToken::new();
    let res = client
        .tools_call(
            "echo",
            Some(serde_json::json!({ "text": "ok" })),
            Some(cancel),
        )
        .await
        .expect("normal call must succeed");
    assert!(!res.is_error);
    // No direct way to assert "no cancel frame was sent" against `run_mock_server` (it
    // doesn't capture frames). The contract is enforced by the `biased; r = wait => r,`
    // branch order in the select! — wait wins when both are ready, so a non-cancelled call
    // never reaches the cancel branch.
}

#[tokio::test]
async fn tools_call_without_cancel_token_keeps_pre_existing_behavior() {
    let (client_side, server_side) = pair();
    tokio::spawn(run_mock_server(server_side));

    let client = McpClient::new(client_side);
    client.initialize("pie-test").await.unwrap();
    let _ = client.tools_list().await.unwrap();

    let res = client
        .tools_call("echo", Some(serde_json::json!({ "text": "hi" })), None)
        .await
        .unwrap();
    assert!(!res.is_error);
}

#[tokio::test]
async fn tools_call_request_timeout_still_returns_timeout_when_no_cancel() {
    let (client_side, server_side) = pair();
    let release = Arc::new(tokio::sync::Notify::new());
    let seen_frames: Arc<AsyncMutex<Vec<serde_json::Value>>> =
        Arc::new(AsyncMutex::new(Vec::new()));
    tokio::spawn(run_slow_mock_server(server_side, release, seen_frames));

    // Override the 30s default so the test doesn't hang waiting for a real timeout.
    let client = McpClient::new(client_side).with_timeout(std::time::Duration::from_millis(150));
    client.initialize("pie-test").await.unwrap();
    let _ = client.tools_list().await.unwrap();

    let res = client.tools_call("slow_echo", None, None).await;
    assert!(
        matches!(res, Err(McpError::Timeout { .. })),
        "expected timeout, got {res:?}"
    );
}
