//! Hermetic Streamable HTTP transport fixture. No real hub/Cloudflare calls.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use pie_mcp::{HttpMcpTransport, HttpMcpTransportOptions, McpClient, McpError, Transport};
use serde_json::json;
use tokio::net::TcpListener;

#[derive(Clone)]
struct FixtureState {
    expected_auth: &'static str,
    seen_auth: Arc<Mutex<Vec<String>>>,
    mode: FixtureMode,
}

#[derive(Clone, Copy)]
enum FixtureMode {
    Normal,
    HttpError,
    OversizePost,
}

#[tokio::test]
async fn streamable_http_posts_requests_and_receives_sse_notifications() {
    let (endpoint, seen_auth) = spawn_fixture(FixtureMode::Normal).await;
    let transport = Arc::new(
        HttpMcpTransport::connect(HttpMcpTransportOptions::new(endpoint).bearer("fixture-token"))
            .unwrap(),
    );
    let client = McpClient::new(transport.clone());

    let init = client.initialize("pie-test").await.unwrap();
    assert_eq!(init.server_info.name, "fixture-hub");

    let mut notifications = client
        .take_notifications()
        .expect("notification receiver should be available");
    let tools = client.tools_list().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "send_notification");

    let notification =
        tokio::time::timeout(std::time::Duration::from_secs(2), notifications.recv())
            .await
            .expect("notification should arrive")
            .expect("notification channel should be open");
    assert_eq!(notification.method, "notifications/agent_message");
    assert_eq!(
        notification.params["_meta"]["pie_summary"].as_str(),
        Some("fixture")
    );

    transport.close().await;
    let seen = seen_auth.lock().unwrap().clone();
    assert!(
        seen.iter().all(|h| h == "Bearer fixture-token"),
        "every POST and SSE request must use Authorization: Bearer; saw {seen:?}"
    );
}

#[tokio::test]
async fn streamable_http_error_body_is_redacted() {
    let (endpoint, _seen_auth) = spawn_fixture(FixtureMode::HttpError).await;
    let transport =
        HttpMcpTransport::connect(HttpMcpTransportOptions::new(endpoint).bearer("fixture-token"))
            .unwrap();

    let err = transport
        .send_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.into())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("400 Bad Request"), "{err}");
    assert!(!err.contains("hub_agent_should_not_leak"), "{err}");
    assert!(!err.contains("payload secret"), "{err}");
}

#[tokio::test]
async fn streamable_http_body_cap_rejects_oversize_response() {
    let (endpoint, _seen_auth) = spawn_fixture(FixtureMode::OversizePost).await;
    let transport =
        HttpMcpTransport::connect(HttpMcpTransportOptions::new(endpoint).bearer("fixture-token"))
            .unwrap();

    let err = transport
        .send_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.into())
        .await
        .unwrap_err();
    assert!(
        matches!(err, McpError::Protocol(ref msg) if msg.contains("exceeded cap")),
        "unexpected error: {err:?}"
    );
}

async fn spawn_fixture(mode: FixtureMode) -> (String, Arc<Mutex<Vec<String>>>) {
    let seen_auth = Arc::new(Mutex::new(Vec::new()));
    let state = FixtureState {
        expected_auth: "Bearer fixture-token",
        seen_auth: seen_auth.clone(),
        mode,
    };
    let app = Router::new()
        .route("/mcp", post(post_mcp).get(get_mcp))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/mcp"), seen_auth)
}

async fn post_mcp(State(state): State<FixtureState>, headers: HeaderMap, body: String) -> Response {
    if let Some(response) = check_auth(&state, &headers) {
        return response;
    }
    if matches!(state.mode, FixtureMode::HttpError) {
        return (
            StatusCode::BAD_REQUEST,
            "hub_agent_should_not_leak payload secret",
        )
            .into_response();
    }
    if matches!(state.mode, FixtureMode::OversizePost) {
        return (StatusCode::OK, "x".repeat(2 * 1024 * 1024)).into_response();
    }
    let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
    let Some(id) = payload.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let method = payload.get("method").and_then(|m| m.as_str()).unwrap();
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "serverInfo": {"name": "fixture-hub", "version": "0.1.0"}
        }),
        "tools/list" => json!({
            "tools": [{
                "name": "send_notification",
                "description": "fixture",
                "inputSchema": {"type": "object"}
            }]
        }),
        other => panic!("unexpected method {other}"),
    };
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

async fn get_mcp(State(state): State<FixtureState>, headers: HeaderMap) -> Response {
    if let Some(response) = check_auth(&state, &headers) {
        return response;
    }
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        "id: fixture-1\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/agent_message\",\"params\":{\"_meta\":{\"pie_summary\":\"fixture\",\"pie_dedup_key\":\"fixture-1\"}}}\n\n",
    )
        .into_response()
}

fn check_auth(state: &FixtureState, headers: &HeaderMap) -> Option<Response> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    state.seen_auth.lock().unwrap().push(auth.clone());
    if auth != state.expected_auth {
        return Some(StatusCode::UNAUTHORIZED.into_response());
    }
    None
}
