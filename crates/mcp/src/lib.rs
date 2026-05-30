//! pie-mcp — minimal MCP (Model Context Protocol) stdio client.
//!
//! Scope: subprocess-based stdio transport plus Streamable HTTP transport, JSON-RPC 2.0
//! framing over the shared [`Transport`] line abstraction, initialize handshake, tools/list,
//! tools/call, and server-pushed notifications. Out of scope for v1: sampling, resource
//! subscriptions, and server-side mode.
//!
//! The crate intentionally does not depend on `pie-agent-core` so it can be reused from
//! places that don't carry the harness — `pie-coding-agent` provides the adapter that wraps
//! MCP tools as `AgentTool`s.

pub mod client;
pub mod errors;
pub mod http;
pub mod protocol;
pub mod stdio;
pub mod transport;

pub use client::{ClientCapabilities, McpClient};
pub use errors::McpError;
pub use http::{HttpMcpAuth, HttpMcpTransport, HttpMcpTransportOptions, ReconnectPolicy};
pub use protocol::{InitializeResult, McpTool, McpToolCallResult, ServerInfo};
pub use stdio::StdioTransport;
pub use transport::Transport;
