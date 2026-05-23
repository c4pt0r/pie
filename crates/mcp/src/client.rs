//! MCP client. Dispatches requests over a [`Transport`], routes responses back to the
//! caller via a per-id one-shot channel, owns the initialize handshake, and exposes
//! `tools/list` + `tools/call`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::errors::McpError;
use crate::protocol::{
    CancelledNotificationParams, ClientCapabilitiesSpec, ClientInfo, InitializeParams,
    InitializeResult, McpTool, PROTOCOL_VERSION, RpcError, ToolsCallParams, ToolsListResult,
    make_notification, make_request,
};
use crate::transport::Transport;

/// Best-effort upper bound on how long we'll wait for the outbound `notifications/cancelled`
/// frame to flush before returning `McpError::Cancelled`. The cancel path is courtesy — even
/// if the notification never reaches the server, the inflight entry is already dropped on our
/// side so any late response is a no-op.
const CANCEL_NOTIFY_SEND_BUDGET: Duration = Duration::from_millis(200);

type InflightMap = HashMap<u64, oneshot::Sender<Result<serde_json::Value, RpcError>>>;

/// RAII guard that removes an in-flight request id from the shared map on drop. Covers every
/// non-success path uniformly: explicit cancel via `CancellationToken`, `tokio::time::timeout`
/// firing, transport error before the oneshot is awaited, or the caller dropping the request
/// future. Without this, cancellations and dropped futures would leak entries that the read
/// pump can never clean up (the pump only `remove`s on a matched response frame).
struct InflightGuard {
    id: u64,
    inflight: Arc<Mutex<InflightMap>>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.inflight.lock().remove(&self.id);
    }
}

/// One server-pushed JSON-RPC notification surfaced by [`McpClient::take_notifications`].
///
/// MCP servers emit notifications without an `id` field. The client previously dropped them
/// silently; per RFC 1 §4.2.1 we now route them through an `mpsc` channel so the
/// notification-hook adapter in `crates/coding-agent` can map each one into a `Trigger`
/// envelope. Consumers (e.g. `McpNotificationHook`) call [`McpClient::take_notifications`]
/// once at startup to obtain the receiver; the channel is allocated unconditionally so this
/// addition is fully additive — clients that never call `take_notifications` keep the prior
/// silent-drop behaviour (the receiver lives inside the client and the buffer is freed when
/// the client is dropped).
#[derive(Clone, Debug)]
pub struct McpServerNotification {
    /// JSON-RPC `method` field, e.g. `"notifications/tools/listChanged"`.
    pub method: String,
    /// JSON-RPC `params` field (typically a JSON object). Set to `Value::Null` when the
    /// server omitted it.
    pub params: serde_json::Value,
}

/// Caller-facing capabilities advertised to the server. v1 advertises nothing — we're a
/// pure-consumer client (we run their tools, not the other way around).
#[derive(Clone, Debug, Default)]
pub struct ClientCapabilities;

pub struct McpClient {
    transport: Arc<dyn Transport>,
    next_id: AtomicU64,
    inflight: Arc<Mutex<InflightMap>>,
    initialized: Arc<Mutex<bool>>,
    request_timeout: Duration,
    /// Server tool catalog after `tools/list` succeeds. Cached so consumers don't re-fetch.
    catalog: Arc<Mutex<Vec<McpTool>>>,
    /// Receiver half of the server-notification channel. Owned by the client until a
    /// consumer calls [`Self::take_notifications`]; thereafter it lives inside the
    /// consumer's task. Held in a `Mutex` because the receiver is taken at most once and
    /// the take operation runs from outside the constructor. The sender half is moved into
    /// the read pump's tokio task at construction time and is dropped when the pump exits.
    notify_rx: Mutex<Option<mpsc::UnboundedReceiver<McpServerNotification>>>,
}

impl McpClient {
    /// Build a client over an existing transport. Spawns the read pump immediately.
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        let inflight: Arc<Mutex<InflightMap>> = Arc::new(Mutex::new(HashMap::new()));
        let pump_inflight = inflight.clone();
        let pump_transport = transport.clone();
        let (pump_notify_tx, notify_rx) = mpsc::unbounded_channel::<McpServerNotification>();
        tokio::spawn(async move {
            loop {
                match pump_transport.recv_line().await {
                    Ok(Some(line)) => {
                        // Parse leniently: extract id + (result | error).
                        let value: serde_json::Value = match serde_json::from_str(&line) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let id = value.get("id").and_then(|v| v.as_u64());
                        if id.is_none() {
                            // Server-pushed notification (no `id` per JSON-RPC). Route it to
                            // the notification channel so the consumer (typically
                            // `McpNotificationHook` in `crates/coding-agent`) can normalize
                            // it into a `Trigger` envelope. If no consumer took the
                            // receiver, the message buffers in the channel and is freed
                            // when the client is dropped — same blast radius as the
                            // previous silent-drop behaviour.
                            let method = match value.get("method").and_then(|m| m.as_str()) {
                                Some(m) => m.to_string(),
                                None => continue,
                            };
                            let params = value
                                .get("params")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            let _ = pump_notify_tx.send(McpServerNotification { method, params });
                            continue;
                        }
                        let id = id.unwrap();
                        let tx = pump_inflight.lock().remove(&id);
                        if let Some(tx) = tx {
                            if let Some(err) = value.get("error") {
                                let err: Result<RpcError, _> = serde_json::from_value(err.clone());
                                let err = err.unwrap_or(RpcError {
                                    code: -32603,
                                    message: "malformed error frame".into(),
                                    data: None,
                                });
                                let _ = tx.send(Err(err));
                            } else if let Some(result) = value.get("result") {
                                let _ = tx.send(Ok(result.clone()));
                            } else {
                                let _ = tx.send(Err(RpcError {
                                    code: -32603,
                                    message: "response had neither result nor error".into(),
                                    data: None,
                                }));
                            }
                        }
                    }
                    Ok(None) | Err(_) => {
                        // Transport closed — drain inflight with a transport error.
                        let pending: Vec<_> =
                            pump_inflight.lock().drain().map(|(_, tx)| tx).collect();
                        for tx in pending {
                            let _ = tx.send(Err(RpcError {
                                code: -32000,
                                message: "transport closed".into(),
                                data: None,
                            }));
                        }
                        break;
                    }
                }
            }
        });

        Self {
            transport,
            next_id: AtomicU64::new(1),
            inflight,
            initialized: Arc::new(Mutex::new(false)),
            request_timeout: Duration::from_secs(30),
            catalog: Arc::new(Mutex::new(Vec::new())),
            notify_rx: Mutex::new(Some(notify_rx)),
        }
    }

    /// Take ownership of the server-notification receiver. Returns `Some(rx)` on the first
    /// call and `None` on every call thereafter. Intended for `McpNotificationHook` (in
    /// `crates/coding-agent`) to wire its outbound `TriggerSink` to the inbound MCP frames.
    /// If no consumer ever calls this, server-pushed notifications buffer inside the channel
    /// until the client is dropped — equivalent to the prior silent-drop behaviour.
    pub fn take_notifications(&self) -> Option<mpsc::UnboundedReceiver<McpServerNotification>> {
        self.notify_rx.lock().take()
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.request_timeout = t;
        self
    }

    /// Run the initialize handshake. Sends `initialize` then notifies `notifications/initialized`.
    pub async fn initialize(&self, client_name: &str) -> Result<InitializeResult, McpError> {
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.into(),
            capabilities: ClientCapabilitiesSpec::default(),
            client_info: ClientInfo {
                name: client_name.into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
        };
        let result: InitializeResult = self.request("initialize", Some(params), None).await?;
        // Notify server that we're ready.
        let note = make_notification::<()>("notifications/initialized", None);
        self.transport
            .send_line(serde_json::to_string(&note)?)
            .await?;
        *self.initialized.lock() = true;
        Ok(result)
    }

    pub fn is_initialized(&self) -> bool {
        *self.initialized.lock()
    }

    /// Fetch the server's tool catalog and cache it.
    pub async fn tools_list(&self) -> Result<Vec<McpTool>, McpError> {
        if !self.is_initialized() {
            return Err(McpError::NotInitialized);
        }
        let result: ToolsListResult = self.request::<(), _>("tools/list", None, None).await?;
        let tools = result.tools;
        *self.catalog.lock() = tools.clone();
        Ok(tools)
    }

    pub fn catalog(&self) -> Vec<McpTool> {
        self.catalog.lock().clone()
    }

    /// Invoke a server-side tool. If `cancel` is `Some`, the call races the cancel signal —
    /// when it fires before the server responds, the in-flight entry is dropped, a best-effort
    /// `notifications/cancelled` frame is sent so the server can stop work, and the call
    /// returns [`McpError::Cancelled`]. Callers that don't need active cancellation can pass
    /// `None` and get the prior pure-timeout behaviour.
    pub async fn tools_call(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
        cancel: Option<CancellationToken>,
    ) -> Result<crate::protocol::McpToolCallResult, McpError> {
        if !self.is_initialized() {
            return Err(McpError::NotInitialized);
        }
        let params = ToolsCallParams {
            name: name.into(),
            arguments,
        };
        self.request("tools/call", Some(params), cancel).await
    }

    /// Shut down the transport. Subsequent calls fail with `NotInitialized` / transport error.
    pub async fn close(&self) {
        self.transport.close().await;
        *self.initialized.lock() = false;
    }

    async fn request<P, R>(
        &self,
        method: &'static str,
        params: Option<P>,
        cancel: Option<CancellationToken>,
    ) -> Result<R, McpError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = make_request(id, method, params);
        let line = serde_json::to_string(&req)?;

        let (tx, rx) = oneshot::channel();
        self.inflight.lock().insert(id, tx);
        // Guard removes the inflight entry on every exit path — explicit cancel, timeout,
        // transport error, OR caller-dropped future. The pump only cleans up on a matched
        // response; without this guard, cancelled/dropped requests would leak entries.
        let _guard = InflightGuard {
            id,
            inflight: self.inflight.clone(),
        };

        self.transport.send_line(line).await?;

        let wait = async {
            match tokio::time::timeout(self.request_timeout, rx).await {
                Ok(Ok(Ok(value))) => Ok::<_, McpError>(serde_json::from_value(value)?),
                Ok(Ok(Err(rpc_err))) => Err(McpError::ServerError {
                    code: rpc_err.code,
                    message: rpc_err.message,
                }),
                Ok(Err(_)) => Err(McpError::Transport("response channel closed".into())),
                Err(_) => Err(McpError::Timeout {
                    seconds: self.request_timeout.as_secs(),
                }),
            }
        };

        match cancel {
            Some(token) => {
                tokio::select! {
                    biased;
                    // Bias towards the response branch: if the response and the cancel race
                    // and both are ready, prefer returning the result the server already
                    // produced rather than spuriously emitting a cancel notification.
                    r = wait => r,
                    _ = token.cancelled() => {
                        self.send_cancelled_notification(id).await;
                        Err(McpError::Cancelled)
                    }
                }
            }
            None => wait.await,
        }
    }

    /// Best-effort: tell the server we no longer need request `id`. Bounded by
    /// [`CANCEL_NOTIFY_SEND_BUDGET`] so a stuck transport can't keep the cancel path open.
    /// Failures are swallowed — our side has already dropped the inflight entry, so a late
    /// response will simply be unmatched and discarded by the read pump.
    async fn send_cancelled_notification(&self, id: u64) {
        let note = make_notification(
            "notifications/cancelled",
            Some(CancelledNotificationParams {
                request_id: id,
                reason: Some("client cancelled".into()),
            }),
        );
        let line = match serde_json::to_string(&note) {
            Ok(l) => l,
            Err(_) => return,
        };
        let _ =
            tokio::time::timeout(CANCEL_NOTIFY_SEND_BUDGET, self.transport.send_line(line)).await;
    }
}
