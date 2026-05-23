//! Adapter that wraps an MCP-server-side tool as a pie_agent_core::AgentTool.
//!
//! One `McpAgentTool` corresponds to one tool name on one server. Tool calls are dispatched
//! through the supplied `Arc<McpClient>`; the result's `content` blocks are mapped to the
//! agent's `UserContentBlock` set. Errors from the MCP server become AgentToolError so the
//! agent loop synthesizes a clean tool-error response.

use std::sync::Arc;

use async_trait::async_trait;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, ToolExecutionMode,
};
use pie_ai::{Tool, UserContentBlock};
use pie_mcp::McpClient;
use pie_mcp::errors::McpError;
use pie_mcp::protocol::{McpTool, ToolContent};
use tokio_util::sync::CancellationToken;

pub struct McpAgentTool {
    client: Arc<McpClient>,
    definition: Tool,
}

impl McpAgentTool {
    /// Build an adapter for one server-side tool. The pie_ai::Tool definition is constructed
    /// from the MCP catalog entry; the input schema is forwarded verbatim.
    pub fn new(client: Arc<McpClient>, tool: &McpTool) -> Self {
        Self {
            client,
            definition: Tool {
                name: tool.name.clone(),
                description: tool.description.clone().unwrap_or_default(),
                parameters: tool.input_schema.clone(),
            },
        }
    }
}

#[async_trait]
impl AgentTool for McpAgentTool {
    fn definition(&self) -> &Tool {
        &self.definition
    }
    fn label(&self) -> &str {
        &self.definition.name
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        // MCP tool calls are individually cheap; let them run in parallel by default.
        Some(ToolExecutionMode::Parallel)
    }

    async fn execute(
        &self,
        _id: &str,
        params: serde_json::Value,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        // Plumb the harness cancel token into the MCP client so a cancelled call also
        // releases the inflight slot on the wire (and tells the server to stop via
        // `notifications/cancelled`). Previously the outer `select!` returned immediately on
        // cancel but the underlying request kept waiting for the server reply or the 30s
        // request timeout — leaking inflight entries and letting server-side work continue
        // after the user hit Ctrl-C.
        let result = match self
            .client
            .tools_call(&self.definition.name, Some(params), Some(cancel.clone()))
            .await
        {
            Ok(r) => r,
            Err(McpError::Cancelled) => {
                return Err(AgentToolError::Message("cancelled".into()));
            }
            Err(e) => return Err(AgentToolError::Message(format!("mcp call: {e}"))),
        };

        let mut content = Vec::with_capacity(result.content.len());
        for block in &result.content {
            match block {
                ToolContent::Text { text } => content.push(UserContentBlock::text(text.clone())),
                ToolContent::Image { data, mime_type } => {
                    content.push(UserContentBlock::Image(pie_ai::ImageContent {
                        data: data.clone(),
                        mime_type: mime_type.clone(),
                    }));
                }
                ToolContent::Resource { resource } => {
                    // No first-class "resource" block on the user side; render as a JSON
                    // text snippet so the LLM still sees what the server returned.
                    let s = serde_json::to_string(resource).unwrap_or_else(|_| "(resource)".into());
                    content.push(UserContentBlock::text(format!("<resource>{s}</resource>")));
                }
            }
        }
        if result.is_error {
            return Err(AgentToolError::Message(
                content
                    .into_iter()
                    .filter_map(|b| match b {
                        UserContentBlock::Text(t) => Some(t.text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
        }
        Ok(AgentToolResult {
            content,
            details: serde_json::json!({ "name": self.definition.name, "isError": false }),
            terminate: None,
        })
    }
}

#[cfg(test)]
mod tests {
    //! Plumbing proof: a `CancellationToken` cancelled by the harness must propagate through
    //! `McpAgentTool::execute` into `McpClient::tools_call` so the MCP server is notified.
    //!
    //! Wire-level cancel semantics (notifications/cancelled frame shape, bounded return,
    //! inflight cleanup) are covered in `crates/mcp/tests/client_fixture.rs`. This test only
    //! asserts the *adapter* hands the token through — i.e. before the plumbing fix, this
    //! test would have hung for ~30s (the MCP request timeout) instead of returning inside
    //! the cancel notify budget.
    use super::*;
    use pie_mcp::Transport;
    use pie_mcp::protocol::McpTool;
    use std::time::Duration;
    use tokio::sync::Mutex as AsyncMutex;
    use tokio::sync::mpsc;

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
        async fn close(&self) {}
    }

    fn pair() -> (Arc<PipeTransport>, Arc<PipeTransport>) {
        let (a_tx, b_rx) = mpsc::unbounded_channel();
        let (b_tx, a_rx) = mpsc::unbounded_channel();
        (
            Arc::new(PipeTransport {
                tx: AsyncMutex::new(a_tx),
                rx: AsyncMutex::new(a_rx),
            }),
            Arc::new(PipeTransport {
                tx: AsyncMutex::new(b_tx),
                rx: AsyncMutex::new(b_rx),
            }),
        )
    }

    /// Mock server: replies to `initialize` and then never replies to `tools/call`. The only
    /// way the test exits within seconds is if the adapter's cancel token reaches the
    /// underlying `tools_call`. Captures every inbound frame so we can also assert the
    /// cancel notification actually hits the wire.
    async fn run_never_reply_server(
        transport: Arc<PipeTransport>,
        seen: Arc<AsyncMutex<Vec<serde_json::Value>>>,
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
            seen.lock().await.push(v.clone());
            let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let id = v.get("id").and_then(|i| i.as_u64());
            if method == "initialize" {
                let _ = transport
                    .send_line(
                        serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {
                                "protocolVersion": "2025-03-26",
                                "capabilities": {},
                                "serverInfo": { "name": "never", "version": "0.0.1" }
                            }
                        })
                        .to_string(),
                    )
                    .await;
            }
            // tools/call: intentionally no response — cancel is the only exit.
        }
    }

    #[tokio::test]
    async fn execute_propagates_cancel_token_to_mcp_client() {
        let (client_side, server_side) = pair();
        let seen: Arc<AsyncMutex<Vec<serde_json::Value>>> = Arc::new(AsyncMutex::new(Vec::new()));
        tokio::spawn(run_never_reply_server(server_side, seen.clone()));

        let client = Arc::new(McpClient::new(client_side));
        client.initialize("pie-test").await.unwrap();

        let tool = McpAgentTool::new(
            client.clone(),
            &McpTool {
                name: "slow_tool".into(),
                description: Some("never replies".into()),
                input_schema: serde_json::json!({ "type": "object" }),
            },
        );

        let cancel = CancellationToken::new();
        let cancel_for_call = cancel.clone();
        let exec = tokio::spawn(async move {
            tool.execute("call-1", serde_json::json!({}), cancel_for_call, None)
                .await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        let started = std::time::Instant::now();
        let res = tokio::time::timeout(Duration::from_secs(2), exec)
            .await
            .expect("adapter must return within seconds when cancel fires")
            .expect("join");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancel must short-circuit the 30s MCP request_timeout, took {:?}",
            started.elapsed()
        );
        let err = res.expect_err("cancelled call must surface as AgentToolError");
        match err {
            AgentToolError::Message(m) => assert_eq!(m, "cancelled"),
            AgentToolError::Other(e) => panic!("expected Message(\"cancelled\"), got Other({e})"),
        }

        // Confirm the notification actually reached the server — proves plumbing went all
        // the way through adapter → McpClient → transport, not just that the future returned.
        let mut found = false;
        for _ in 0..50 {
            let frames = seen.lock().await;
            if frames.iter().any(|f| {
                f.get("method").and_then(|m| m.as_str()) == Some("notifications/cancelled")
            }) {
                found = true;
                break;
            }
            drop(frames);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            found,
            "notifications/cancelled must reach the server when the adapter's cancel token fires"
        );
    }
}
