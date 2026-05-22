//! MCP client. Dispatches requests over a [`Transport`], routes responses back to the
//! caller via a per-id one-shot channel, owns the initialize handshake, and exposes
//! `tools/list` + `tools/call`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, oneshot};

use crate::errors::McpError;
use crate::protocol::{
    ClientCapabilitiesSpec, ClientInfo, InitializeParams, InitializeResult, McpTool,
    PROTOCOL_VERSION, RpcError, ToolsCallParams, ToolsListResult, make_notification, make_request,
};
use crate::transport::Transport;

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
    inflight: Arc<
        Mutex<std::collections::HashMap<u64, oneshot::Sender<Result<serde_json::Value, RpcError>>>>,
    >,
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
        let inflight: Arc<
            Mutex<
                std::collections::HashMap<
                    u64,
                    oneshot::Sender<Result<serde_json::Value, RpcError>>,
                >,
            >,
        > = Arc::new(Mutex::new(std::collections::HashMap::new()));
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
        let result: InitializeResult = self.request("initialize", Some(params)).await?;
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
        let result: ToolsListResult = self.request::<(), _>("tools/list", None).await?;
        let tools = result.tools;
        *self.catalog.lock() = tools.clone();
        Ok(tools)
    }

    pub fn catalog(&self) -> Vec<McpTool> {
        self.catalog.lock().clone()
    }

    pub async fn tools_call(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<crate::protocol::McpToolCallResult, McpError> {
        if !self.is_initialized() {
            return Err(McpError::NotInitialized);
        }
        let params = ToolsCallParams {
            name: name.into(),
            arguments,
        };
        self.request("tools/call", Some(params)).await
    }

    /// Shut down the transport. Subsequent calls fail with `NotInitialized` / transport error.
    pub async fn close(&self) {
        self.transport.close().await;
        *self.initialized.lock() = false;
    }

    async fn request<P, R>(&self, method: &'static str, params: Option<P>) -> Result<R, McpError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = make_request(id, method, params);
        let line = serde_json::to_string(&req)?;

        let (tx, rx) = oneshot::channel();
        self.inflight.lock().insert(id, tx);
        if let Err(e) = self.transport.send_line(line).await {
            self.inflight.lock().remove(&id);
            return Err(e);
        }
        let resp = tokio::time::timeout(self.request_timeout, rx).await;
        match resp {
            Ok(Ok(Ok(value))) => Ok(serde_json::from_value(value)?),
            Ok(Ok(Err(rpc_err))) => Err(McpError::ServerError {
                code: rpc_err.code,
                message: rpc_err.message,
            }),
            Ok(Err(_)) => Err(McpError::Transport("response channel closed".into())),
            Err(_) => {
                self.inflight.lock().remove(&id);
                Err(McpError::Timeout {
                    seconds: self.request_timeout.as_secs(),
                })
            }
        }
    }
}
