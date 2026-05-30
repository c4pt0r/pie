//! Transport abstraction. Stdio and Streamable HTTP both adapt to one JSON object per line.

use async_trait::async_trait;

use crate::errors::McpError;

/// A bidirectional newline-delimited JSON channel. Implementations send one JSON object per
/// line in each direction.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Write one JSON object (caller-supplied serialized string, no trailing newline).
    async fn send_line(&self, line: String) -> Result<(), McpError>;
    /// Read the next JSON line from the peer. Returns the trimmed line (no trailing newline).
    /// Returns `Ok(None)` on clean EOF.
    async fn recv_line(&self) -> Result<Option<String>, McpError>;
    /// Best-effort shutdown — drop transports, terminate subprocesses, etc.
    async fn close(&self);
}
