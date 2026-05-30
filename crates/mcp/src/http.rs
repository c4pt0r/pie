//! Streamable HTTP transport for MCP.
//!
//! This adapts MCP's HTTP POST + SSE shape to the existing line-oriented [`Transport`]
//! trait. Each outbound JSON-RPC frame is sent as one POST. JSON POST responses and
//! SSE data frames from either POST or the long-lived GET stream are enqueued as raw
//! JSON lines for `McpClient`, which already routes responses vs. notifications by
//! JSON-RPC `id` presence.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use parking_lot::Mutex;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::errors::McpError;
use crate::transport::Transport;

const DEFAULT_BODY_CAP_BYTES: usize = 1024 * 1024;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_USER_AGENT: &str = concat!(
    "pie-mcp/",
    env!("CARGO_PKG_VERSION"),
    " (mcp-streamable-http/2025-03-26)"
);

#[derive(Clone, Debug)]
pub struct ReconnectPolicy {
    pub initial_delay: Duration,
    pub max_delay: Duration,
    /// `None` means retry indefinitely until [`Transport::close`] is called.
    pub max_attempts: Option<usize>,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            max_attempts: None,
        }
    }
}

#[derive(Clone, Default)]
pub enum HttpMcpAuth {
    #[default]
    None,
    Bearer {
        token: String,
    },
}

impl std::fmt::Debug for HttpMcpAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Bearer { .. } => f.write_str("Bearer { token: <redacted> }"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HttpMcpTransportOptions {
    pub endpoint_url: String,
    pub auth: HttpMcpAuth,
    pub reconnect_policy: ReconnectPolicy,
    pub body_cap_bytes: usize,
    pub request_timeout: Duration,
    pub sse_idle_timeout: Duration,
    pub user_agent: String,
}

impl HttpMcpTransportOptions {
    pub fn new(endpoint_url: impl Into<String>) -> Self {
        Self {
            endpoint_url: endpoint_url.into(),
            auth: HttpMcpAuth::None,
            reconnect_policy: ReconnectPolicy::default(),
            body_cap_bytes: DEFAULT_BODY_CAP_BYTES,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            sse_idle_timeout: DEFAULT_SSE_IDLE_TIMEOUT,
            user_agent: DEFAULT_USER_AGENT.into(),
        }
    }

    pub fn bearer(mut self, token: impl Into<String>) -> Self {
        self.auth = HttpMcpAuth::Bearer {
            token: token.into(),
        };
        self
    }
}

pub struct HttpMcpTransport {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    auth: Arc<Mutex<HttpMcpAuth>>,
    body_cap_bytes: usize,
    request_timeout: Duration,
    user_agent: HeaderValue,
    tx: mpsc::Sender<Result<String, McpError>>,
    rx: AsyncMutex<mpsc::Receiver<Result<String, McpError>>>,
    close_token: CancellationToken,
    sse_task: AsyncMutex<Option<JoinHandle<()>>>,
}

impl HttpMcpTransport {
    pub fn connect(opts: HttpMcpTransportOptions) -> Result<Self, McpError> {
        let endpoint = reqwest::Url::parse(&opts.endpoint_url)
            .map_err(|e| McpError::Transport(format!("invalid MCP HTTP endpoint: {e}")))?;
        if endpoint.scheme() != "https" && endpoint.host_str() != Some("127.0.0.1") {
            return Err(McpError::Transport(
                "streamable_http endpoint must be https, except 127.0.0.1 test fixtures".into(),
            ));
        }
        let user_agent = HeaderValue::from_str(&opts.user_agent)
            .map_err(|_| McpError::Transport("invalid streamable_http user agent".into()))?;
        let client = reqwest::Client::builder()
            .timeout(opts.request_timeout)
            .build()
            .map_err(|e| McpError::Transport(e.to_string()))?;
        let (tx, rx) = mpsc::channel(256);
        let close_token = CancellationToken::new();
        let auth = Arc::new(Mutex::new(opts.auth));
        let sse_task = spawn_sse_loop(
            client.clone(),
            endpoint.clone(),
            auth.clone(),
            opts.body_cap_bytes,
            opts.sse_idle_timeout,
            user_agent.clone(),
            opts.reconnect_policy,
            tx.clone(),
            close_token.clone(),
        );
        Ok(Self {
            client,
            endpoint,
            auth,
            body_cap_bytes: opts.body_cap_bytes,
            request_timeout: opts.request_timeout,
            user_agent,
            tx,
            rx: AsyncMutex::new(rx),
            close_token,
            sse_task: AsyncMutex::new(Some(sse_task)),
        })
    }

    pub fn set_auth(&self, auth: HttpMcpAuth) {
        *self.auth.lock() = auth;
    }

    fn auth(&self) -> HttpMcpAuth {
        self.auth.lock().clone()
    }
}

#[async_trait]
impl Transport for HttpMcpTransport {
    async fn send_line(&self, line: String) -> Result<(), McpError> {
        if line.len() > self.body_cap_bytes {
            return Err(McpError::Protocol(
                "MCP HTTP request exceeded body cap".into(),
            ));
        }
        let response = with_auth(
            self.client
                .post(self.endpoint.clone())
                .timeout(self.request_timeout)
                .header(CONTENT_TYPE, "application/json")
                .header(ACCEPT, "application/json, text/event-stream")
                .header(USER_AGENT, self.user_agent.clone())
                .body(line),
            &self.auth(),
        )
        .send()
        .await
        .map_err(|e| McpError::Transport(e.to_string()))?;
        enqueue_response_body(
            response,
            self.body_cap_bytes,
            self.tx.clone(),
            self.close_token.clone(),
        )
        .await
    }

    async fn recv_line(&self) -> Result<Option<String>, McpError> {
        let mut rx = self.rx.lock().await;
        match rx.recv().await {
            Some(Ok(line)) => Ok(Some(line)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    async fn close(&self) {
        self.close_token.cancel();
        if let Some(handle) = self.sse_task.lock().await.take() {
            handle.abort();
        }
    }
}

fn spawn_sse_loop(
    client: reqwest::Client,
    endpoint: reqwest::Url,
    auth: Arc<Mutex<HttpMcpAuth>>,
    body_cap_bytes: usize,
    sse_idle_timeout: Duration,
    user_agent: HeaderValue,
    reconnect_policy: ReconnectPolicy,
    tx: mpsc::Sender<Result<String, McpError>>,
    close_token: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let last_event_id = std::sync::Arc::new(Mutex::new(None::<String>));
        let mut delay = reconnect_policy.initial_delay;
        let mut attempts = 0usize;
        loop {
            if close_token.is_cancelled() {
                break;
            }
            let mut headers = HeaderMap::new();
            headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
            headers.insert(USER_AGENT, user_agent.clone());
            if let Some(id) = last_event_id.lock().clone()
                && let Ok(v) = HeaderValue::from_str(&id)
            {
                headers.insert("last-event-id", v);
            }
            let current_auth = auth.lock().clone();
            let request = with_auth(client.get(endpoint.clone()).headers(headers), &current_auth);
            let result = match tokio::time::timeout(sse_idle_timeout, request.send()).await {
                Ok(Ok(response)) => {
                    read_sse_response(
                        response,
                        body_cap_bytes,
                        Some(sse_idle_timeout),
                        tx.clone(),
                        close_token.clone(),
                        Some(last_event_id.clone()),
                    )
                    .await
                }
                Ok(Err(e)) => Err(McpError::Transport(e.to_string())),
                Err(_) => Err(McpError::Timeout {
                    seconds: sse_idle_timeout.as_secs(),
                }),
            };
            if close_token.is_cancelled() {
                break;
            }
            if result.is_ok() {
                delay = reconnect_policy.initial_delay;
                attempts = 0;
            } else {
                attempts += 1;
                if let Some(max_attempts) = reconnect_policy.max_attempts
                    && attempts >= max_attempts
                {
                    let _ = tx
                        .send(Err(McpError::Transport(
                            "MCP HTTP SSE reconnect attempts exhausted".into(),
                        )))
                        .await;
                    break;
                }
            }
            tokio::select! {
                _ = close_token.cancelled() => break,
                _ = tokio::time::sleep(delay) => {}
            }
            delay = std::cmp::min(delay.saturating_mul(2), reconnect_policy.max_delay);
        }
    })
}

async fn enqueue_response_body(
    response: reqwest::Response,
    body_cap_bytes: usize,
    tx: mpsc::Sender<Result<String, McpError>>,
    close_token: CancellationToken,
) -> Result<(), McpError> {
    if !response.status().is_success() {
        let status = response.status();
        return Err(McpError::Transport(format!(
            "MCP HTTP status {status}; response body redacted"
        )));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if content_type.starts_with("text/event-stream") {
        tokio::spawn(async move {
            let _ = read_sse_response(response, body_cap_bytes, None, tx, close_token, None).await;
        });
        return Ok(());
    }
    let text = capped_text(response, body_cap_bytes).await?;
    if !text.trim().is_empty() {
        tx.send(Ok(text.trim().to_string()))
            .await
            .map_err(|_| McpError::Transport("MCP HTTP response queue closed".into()))?;
    }
    Ok(())
}

async fn capped_text(response: reqwest::Response, cap: usize) -> Result<String, McpError> {
    let mut stream = response.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| McpError::Transport(e.to_string()))?;
        if out.len() + chunk.len() > cap {
            return Err(McpError::Protocol(
                "MCP HTTP response body exceeded cap".into(),
            ));
        }
        out.extend_from_slice(&chunk);
    }
    String::from_utf8(out).map_err(|e| McpError::Protocol(format!("utf8: {e}")))
}

async fn read_sse_response(
    response: reqwest::Response,
    body_cap_bytes: usize,
    idle_timeout: Option<Duration>,
    tx: mpsc::Sender<Result<String, McpError>>,
    close_token: CancellationToken,
    last_event_id: Option<std::sync::Arc<Mutex<Option<String>>>>,
) -> Result<(), McpError> {
    if !response.status().is_success() {
        return Err(McpError::Transport(format!(
            "MCP HTTP SSE status {}",
            response.status()
        )));
    }
    let mut parser = SseParser::new(body_cap_bytes);
    let mut stream = response.bytes_stream();
    loop {
        let next = read_next_sse_chunk(&mut stream, idle_timeout, close_token.clone()).await?;
        let Some(chunk) = next else {
            return Ok(());
        };
        for event in parser.push(&chunk)? {
            if let Some(id) = event.id
                && let Some(last_event_id) = &last_event_id
            {
                *last_event_id.lock() = Some(id);
            }
            if let Some(data) = event.data {
                tx.send(Ok(data))
                    .await
                    .map_err(|_| McpError::Transport("MCP HTTP response queue closed".into()))?;
            }
        }
    }
}

async fn read_next_sse_chunk<S, B>(
    stream: &mut S,
    idle_timeout: Option<Duration>,
    close_token: CancellationToken,
) -> Result<Option<B>, McpError>
where
    S: futures::Stream<Item = Result<B, reqwest::Error>> + Unpin,
    B: AsRef<[u8]>,
{
    let next = async {
        tokio::select! {
            _ = close_token.cancelled() => None,
            next = stream.next() => next,
        }
    };
    match idle_timeout {
        Some(timeout) => tokio::time::timeout(timeout, next)
            .await
            .map_err(|_| McpError::Timeout {
                seconds: timeout.as_secs(),
            })?
            .transpose()
            .map_err(|e| McpError::Transport(e.to_string())),
        None => next
            .await
            .transpose()
            .map_err(|e| McpError::Transport(e.to_string())),
    }
}

fn with_auth(builder: reqwest::RequestBuilder, auth: &HttpMcpAuth) -> reqwest::RequestBuilder {
    match auth {
        HttpMcpAuth::None => builder,
        HttpMcpAuth::Bearer { token } => builder.header(AUTHORIZATION, format!("Bearer {token}")),
    }
}

#[derive(Default)]
struct SseParser {
    cap: usize,
    buffer: String,
}

impl SseParser {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            buffer: String::new(),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, McpError> {
        if self.buffer.len() + chunk.len() > self.cap {
            return Err(McpError::Protocol("MCP HTTP SSE frame exceeded cap".into()));
        }
        let text =
            std::str::from_utf8(chunk).map_err(|e| McpError::Protocol(format!("utf8: {e}")))?;
        self.buffer.push_str(text);
        let mut out = Vec::new();
        while let Some(idx) = self.buffer.find("\n\n") {
            let raw = self.buffer[..idx].to_string();
            self.buffer.drain(..idx + 2);
            if let Some(event) = parse_sse_event(&raw) {
                out.push(event);
            }
        }
        Ok(out)
    }
}

struct SseEvent {
    id: Option<String>,
    data: Option<String>,
}

fn parse_sse_event(raw: &str) -> Option<SseEvent> {
    let mut id = None;
    let mut data_lines = Vec::new();
    for line in raw.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("id:") {
            id = Some(rest.trim_start().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start().to_string());
        }
    }
    if id.is_none() && data_lines.is_empty() {
        return None;
    }
    Some(SseEvent {
        id,
        data: (!data_lines.is_empty()).then(|| data_lines.join("\n")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_debug_redacts_token() {
        let auth = HttpMcpAuth::Bearer {
            token: "hub_agent_secret".into(),
        };
        let text = format!("{auth:?}");
        assert!(text.contains("<redacted>"));
        assert!(!text.contains("hub_agent_secret"));
    }

    #[test]
    fn sse_parser_ignores_heartbeat_and_extracts_data() {
        let mut parser = SseParser::new(1024);
        let events = parser
            .push(
                b": connected\n\nid: abc\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/x\"}\n\n",
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id.as_deref(), Some("abc"));
        assert_eq!(
            events[0].data.as_deref(),
            Some("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/x\"}")
        );
    }
}
