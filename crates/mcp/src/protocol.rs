//! MCP protocol types — a hand-picked subset of the 2025-03-26 spec, just enough for the
//! initialize handshake and tools list/call. Everything serializes to camelCase JSON-RPC.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: &str = "2025-03-26";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: ClientCapabilitiesSpec,
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientCapabilitiesSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: serde_json::Value,
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsListResult {
    pub tools: Vec<McpTool>,
    #[serde(
        rename = "nextCursor",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsCallParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
}

/// `params` for the MCP `notifications/cancelled` frame (spec 2025-03-26 §basic/utilities/cancellation).
/// Sent by the client when a previously issued request id is no longer needed; the server SHOULD
/// stop work for that id. `requestId` matches the original JSON-RPC request id.
#[derive(Debug, Clone, Serialize)]
pub struct CancelledNotificationParams {
    #[serde(rename = "requestId")]
    pub request_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolCallResult {
    pub content: Vec<ToolContent>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToolContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    #[serde(rename = "resource")]
    Resource { resource: serde_json::Value },
}

/// JSON-RPC 2.0 envelope. Generic over the params/result payloads.
#[derive(Debug, Clone, Serialize)]
pub struct RpcRequest<P: Serialize> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<P>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcResponse<R> {
    #[allow(dead_code)]
    pub jsonrpc: Option<String>,
    pub id: Option<u64>,
    #[serde(default)]
    pub result: Option<R>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcNotification<P: Serialize> {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<P>,
}

pub fn make_request<P: Serialize>(
    id: u64,
    method: &'static str,
    params: Option<P>,
) -> RpcRequest<P> {
    RpcRequest {
        jsonrpc: "2.0",
        id,
        method,
        params,
    }
}

pub fn make_notification<P: Serialize>(
    method: &'static str,
    params: Option<P>,
) -> RpcNotification<P> {
    RpcNotification {
        jsonrpc: "2.0",
        method,
        params,
    }
}
