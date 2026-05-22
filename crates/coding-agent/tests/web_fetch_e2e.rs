//! End-to-end test for the web_fetch tool. Spins up a tiny TCP listener that speaks just
//! enough HTTP/1.1 to serve a single response, then drives WebFetchTool::execute against it.
//! Asserts the rendered output contains stripped text + the expected status/content-type
//! header line.

use std::sync::Arc;

use pie_agent_core::{AgentTool, ToolExecutionMode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[path = "../src/tools/web_fetch.rs"]
mod web_fetch;

async fn spawn_http(
    body: &'static str,
    content_type: &'static str,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}/", addr);

    let handle = tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            // Drain the request — just enough to consume headers; we don't parse it.
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                content_type,
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (url, handle)
}

#[tokio::test]
async fn web_fetch_strips_html_to_text() {
    let html = "<html><body><h1>Hi</h1><p>Hello &amp; <b>world</b></p><script>evil()</script></body></html>";
    let (url, _server) = spawn_http(html, "text/html; charset=utf-8").await;
    let tool = web_fetch::WebFetchTool;
    let result = tool
        .execute(
            "call-1",
            serde_json::json!({ "url": url }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("web_fetch should succeed");
    let body = match &result.content[0] {
        pie_ai::UserContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text content"),
    };
    assert!(body.contains("status: 200"), "status header line: {body}");
    assert!(body.contains("text/html"), "ctype header line: {body}");
    assert!(body.contains("Hi"), "missing heading: {body}");
    assert!(
        body.contains("Hello & world"),
        "missing decoded entity: {body}"
    );
    assert!(
        !body.contains("evil()"),
        "script body must be stripped: {body}"
    );
}

#[tokio::test]
async fn web_fetch_returns_plain_text_unchanged() {
    let txt = "raw plain text\nline two\n";
    let (url, _server) = spawn_http(txt, "text/plain").await;
    let tool = web_fetch::WebFetchTool;
    let result = tool
        .execute(
            "call-2",
            serde_json::json!({ "url": url }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    let body = match &result.content[0] {
        pie_ai::UserContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text content"),
    };
    assert!(body.contains("raw plain text"));
    assert!(body.contains("line two"));
}

#[tokio::test]
async fn web_fetch_missing_url_errors() {
    let tool = web_fetch::WebFetchTool;
    let err = tool
        .execute(
            "call-3",
            serde_json::json!({}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("missing required arg"));
}

#[allow(dead_code)]
fn _exec_mode_is_parallel(t: &web_fetch::WebFetchTool) -> Option<ToolExecutionMode> {
    Arc::new(t).execution_mode()
}

/// Tiny HTTP server that streams a body larger than the 5 MiB cap and records how many
/// bytes it managed to push before the client closed the socket. Used to prove that
/// `web_fetch` enforces the cap **streaming**: it must (a) report the cap as the rendered
/// byte count + `truncated: true`, and (b) not require the whole oversized body to fit in
/// memory before truncating.
///
/// The `MAX_BODY_BYTES` const is duplicated here on purpose — keep the test independent of
/// the impl's internal constant; if the cap changes, both sides should be updated to keep
/// the assertion meaningful.
const TEST_CAP_BYTES: usize = 5 * 1024 * 1024;

async fn spawn_oversize_http(
    body_bytes: usize,
) -> (
    String,
    tokio::task::JoinHandle<usize>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    use std::sync::Arc;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}/", addr);
    let done = Arc::new(tokio::sync::Notify::new());
    let done_signal = done.clone();
    let handle = tokio::spawn(async move {
        let (mut sock, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => {
                done_signal.notify_waiters();
                return 0;
            }
        };
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf).await;
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {body_bytes}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n"
        );
        if sock.write_all(header.as_bytes()).await.is_err() {
            done_signal.notify_waiters();
            return 0;
        }
        let chunk = vec![b'A'; 64 * 1024];
        let mut written = 0usize;
        while written < body_bytes {
            let to_write = std::cmp::min(chunk.len(), body_bytes - written);
            // When the client closes the socket early (which is what we want), this will
            // fail and we exit the loop with whatever we've sent so far.
            if sock.write_all(&chunk[..to_write]).await.is_err() {
                break;
            }
            written += to_write;
        }
        let _ = sock.shutdown().await;
        done_signal.notify_waiters();
        written
    });
    (url, handle, done)
}

/// Oversized body must be truncated to the 5 MiB cap and surface `truncated: true`. The
/// previous `resp.bytes().await` implementation would buffer the entire body before
/// truncating; this test sends 5 MiB + 1 MiB and asserts the client only retains the cap.
#[tokio::test]
async fn web_fetch_truncates_oversized_body_streaming() {
    // 1 MiB of overshoot is plenty to prove we don't blindly slurp the whole thing.
    let total = TEST_CAP_BYTES + 1024 * 1024;
    let (url, server, _done) = spawn_oversize_http(total).await;

    let tool = web_fetch::WebFetchTool;
    let result = tool
        .execute(
            "call-cap",
            serde_json::json!({ "url": url }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("web_fetch should succeed even on oversize body");

    let truncated = result
        .details
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let bytes = result
        .details
        .get("bytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    assert!(
        truncated,
        "oversized body should set truncated=true, details={}",
        result.details
    );
    assert_eq!(
        bytes, TEST_CAP_BYTES,
        "rendered byte count must equal the cap (got {bytes})"
    );

    let body = match &result.content[0] {
        pie_ai::UserContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text content"),
    };
    assert!(
        body.contains("(truncated)"),
        "header should mark response as truncated: {}",
        &body[..body.len().min(200)]
    );

    // Wait for the server task to wind down before the test exits so we know how many
    // bytes it actually wrote. The client must drop the connection well before the server
    // finishes streaming the full 6 MiB; in practice TCP buffering keeps the actual server
    // write below `body + a few socket buffers`, but the only invariant we can
    // deterministically assert is that the client returned without hanging on the full
    // body. The presence of the truncated flag + correct byte count is the load-bearing
    // assertion above; the server's written count is logged for debugging but not asserted
    // because TCP buffer sizes vary too much across kernels.
    let written = server.await.expect("server task should complete");
    let _ = written; // observed but not asserted
}
