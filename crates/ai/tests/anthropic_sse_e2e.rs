//! End-to-end test of the Anthropic provider's HTTP → SSE → event pipeline against a local
//! mock server. No network or API key required. Validates the event ordering contract:
//!   Start → TextStart → TextDelta* → TextEnd → (ToolCall*) → Done
//!
//! This exercises the same SSE machinery every provider shares, so a regression here would also
//! affect OpenAI Responses / Completions.

use futures::StreamExt;
use pie_ai::{
    Api, AssistantMessageEvent, ContentBlock, Context, ErrorReason, KnownApi, Message, Model,
    ModelCost, Provider, StopReason, StreamOptions, UserContent, UserMessage, UserRole, stream,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Serve one canned HTTP/1.1 response carrying the given SSE body, then exit.
async fn serve_once(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        // Drain the request headers (we don't care about the content).
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
    });
    format!("http://{addr}")
}

fn anthropic_model(base_url: String) -> Model {
    Model {
        id: "claude-test".into(),
        name: "Claude Test".into(),
        api: Api::known(KnownApi::AnthropicMessages),
        provider: Provider::from("anthropic"),
        base_url,
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: ModelCost::default(),
        context_window: 200_000,
        max_tokens: 1024,
        headers: None,
        compat: None,
    }
}

fn user_ctx(text: &str) -> Context {
    Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: UserContent::Text(text.into()),
            timestamp: 0,
        })],
        tools: None,
    }
}

#[tokio::test]
async fn text_stream_produces_ordered_events() {
    // Canonical Anthropic SSE for a short text response.
    let body = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n\
event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n\
event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    let base = serve_once(body).await;
    let model = anthropic_model(base);
    let opts = StreamOptions {
        api_key: Some("test-key".into()),
        ..Default::default()
    };

    let mut s = stream(&model, &user_ctx("hi"), Some(&opts));
    let mut kinds = Vec::new();
    let mut text = String::new();
    let mut final_msg = None;
    while let Some(ev) = s.next().await {
        match ev {
            AssistantMessageEvent::Start { .. } => kinds.push("start"),
            AssistantMessageEvent::TextStart { .. } => kinds.push("text_start"),
            AssistantMessageEvent::TextDelta { delta, .. } => {
                kinds.push("text_delta");
                text.push_str(&delta);
            }
            AssistantMessageEvent::TextEnd { .. } => kinds.push("text_end"),
            AssistantMessageEvent::Done { message, .. } => {
                kinds.push("done");
                final_msg = Some(message);
            }
            AssistantMessageEvent::Error { error, .. } => {
                panic!("unexpected error: {:?}", error.error_message);
            }
            _ => {}
        }
    }

    assert_eq!(text, "Hello world");
    assert_eq!(kinds.first(), Some(&"start"));
    assert_eq!(kinds.last(), Some(&"done"));
    // start precedes text_start precedes first text_delta.
    let p_start = kinds.iter().position(|k| *k == "text_start").unwrap();
    let p_delta = kinds.iter().position(|k| *k == "text_delta").unwrap();
    let p_end = kinds.iter().position(|k| *k == "text_end").unwrap();
    assert!(p_start < p_delta && p_delta < p_end);

    let msg = final_msg.expect("final message");
    assert_eq!(msg.usage.input, 10);
    assert_eq!(msg.usage.output, 2);
    assert_eq!(msg.response_id.as_deref(), Some("msg_1"));
}

#[tokio::test]
async fn tool_use_sets_tooluse_stop_reason() {
    let body = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n\
event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"sf\\\"}\"}}\n\n\
event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":8}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    let base = serve_once(body).await;
    let model = anthropic_model(base);
    let opts = StreamOptions {
        api_key: Some("test-key".into()),
        ..Default::default()
    };

    let mut s = stream(&model, &user_ctx("weather?"), Some(&opts));
    let mut saw_tool_start = false;
    let mut tool_end_args = None;
    let mut done_reason = None;
    let mut done_args = None;
    while let Some(ev) = s.next().await {
        match ev {
            AssistantMessageEvent::ToolCallStart { .. } => saw_tool_start = true,
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                tool_end_args = Some(tool_call.arguments);
            }
            AssistantMessageEvent::Done { reason, message } => {
                done_reason = Some(reason);
                done_args = message.content.iter().find_map(|block| match block {
                    ContentBlock::ToolCall(tc) => Some(tc.arguments.clone()),
                    _ => None,
                });
            }
            AssistantMessageEvent::Error { error, .. } => {
                panic!("unexpected error: {:?}", error.error_message)
            }
            _ => {}
        }
    }
    assert!(saw_tool_start, "expected a ToolCallStart event");
    assert_eq!(
        tool_end_args
            .as_ref()
            .and_then(|args| args.get("city"))
            .and_then(|v| v.as_str()),
        Some("sf")
    );
    assert_eq!(
        done_args
            .as_ref()
            .and_then(|args| args.get("city"))
            .and_then(|v| v.as_str()),
        Some("sf")
    );
    assert_eq!(done_reason, Some(pie_ai::DoneReason::ToolUse));
}

#[tokio::test]
async fn http_error_becomes_error_event() {
    // Server returns a 200 but with an SSE `error` event.
    let body =
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n";
    let base = serve_once(body).await;
    let model = anthropic_model(base);
    let opts = StreamOptions {
        api_key: Some("test-key".into()),
        ..Default::default()
    };

    let mut s = stream(&model, &user_ctx("hi"), Some(&opts));
    let mut error_msg = None;
    while let Some(ev) = s.next().await {
        if let AssistantMessageEvent::Error { error, .. } = ev {
            error_msg = error.error_message.clone();
        }
    }
    assert_eq!(error_msg.as_deref(), Some("overloaded"));
}

#[tokio::test]
async fn retries_on_503_then_succeeds() {
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let success_body = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_r\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n\
event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n\
event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    tokio::spawn(async move {
        // First request: 503 with retry-after: 0
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf).await;
        let resp = "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        s.write_all(resp.as_bytes()).await.unwrap();
        // Second request: 200 with the canned SSE body.
        let (mut s, _) = listener.accept().await.unwrap();
        let _ = s.read(&mut buf).await;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            success_body.len(),
            success_body
        );
        s.write_all(resp.as_bytes()).await.unwrap();
    });
    let base = format!("http://{addr}");
    let model = anthropic_model(base);
    let opts = StreamOptions {
        api_key: Some("test-key".into()),
        max_retries: Some(2),
        max_retry_delay_ms: Some(5_000),
        ..Default::default()
    };
    let mut s = stream(&model, &user_ctx("retry-me"), Some(&opts));
    let mut text = String::new();
    let mut done = false;
    while let Some(ev) = s.next().await {
        match ev {
            AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(&delta),
            AssistantMessageEvent::Done { .. } => done = true,
            AssistantMessageEvent::Error { error, .. } => {
                panic!("unexpected error: {:?}", error.error_message)
            }
            _ => {}
        }
    }
    assert!(done);
    assert_eq!(text, "ok");
}

#[tokio::test]
async fn abort_cancels_retry_sleep_before_second_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let token = CancellationToken::new();
    let aborter = token.clone();

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf).await;
        let resp = "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 60\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        s.write_all(resp.as_bytes()).await.unwrap();
        aborter.cancel();
        tokio::time::timeout(std::time::Duration::from_millis(200), listener.accept())
            .await
            .is_ok()
    });

    let base = format!("http://{addr}");
    let model = anthropic_model(base);
    let opts = StreamOptions {
        api_key: Some("test-key".into()),
        max_retries: Some(2),
        max_retry_delay_ms: Some(120_000),
        abort: Some(token),
        ..Default::default()
    };

    let mut s = stream(&model, &user_ctx("retry-me"), Some(&opts));
    let mut aborted = false;
    while let Some(ev) = s.next().await {
        if let AssistantMessageEvent::Error { reason, error } = ev {
            aborted = reason == ErrorReason::Aborted
                && error.stop_reason == StopReason::Aborted
                && error.error_message.as_deref() == Some("aborted");
        }
    }

    assert!(aborted, "expected aborted error event");
    assert!(
        !server.await.unwrap(),
        "aborted retry sleep should not send a second request"
    );
}

#[tokio::test]
async fn abort_cancels_pending_sse_drain() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let token = CancellationToken::new();
    let wrote_prefix = Arc::new(AtomicBool::new(false));
    let server_wrote_prefix = wrote_prefix.clone();

    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf).await;
        let prefix = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_abort\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n";
        let headers =
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
        s.write_all(headers.as_bytes()).await.unwrap();
        s.write_all(prefix.as_bytes()).await.unwrap();
        s.flush().await.unwrap();
        server_wrote_prefix.store(true, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });

    let base = format!("http://{addr}");
    let model = anthropic_model(base);
    let opts = StreamOptions {
        api_key: Some("test-key".into()),
        abort: Some(token.clone()),
        ..Default::default()
    };

    let mut s = stream(&model, &user_ctx("stream-me"), Some(&opts));
    let mut aborted = false;
    while !wrote_prefix.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    token.cancel();
    while let Some(ev) = s.next().await {
        if let AssistantMessageEvent::Error { reason, error } = ev {
            aborted = reason == ErrorReason::Aborted
                && error.stop_reason == StopReason::Aborted
                && error.error_message.as_deref() == Some("aborted");
        }
    }

    assert!(aborted, "expected SSE drain to stop on abort");
}
