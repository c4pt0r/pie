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
        .tools_call("echo", Some(serde_json::json!({ "text": "hi" })))
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
