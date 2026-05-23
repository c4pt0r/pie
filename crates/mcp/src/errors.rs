//! Error type for the MCP client.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum McpError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("server returned error {code}: {message}")]
    ServerError { code: i64, message: String },
    #[error("request timed out after {seconds}s")]
    Timeout { seconds: u64 },
    #[error("client is not initialized; call `initialize` before issuing requests")]
    NotInitialized,
    #[error("request cancelled before the server responded")]
    Cancelled,
    #[error("{0}")]
    Other(String),
}

impl From<std::io::Error> for McpError {
    fn from(e: std::io::Error) -> Self {
        McpError::Transport(e.to_string())
    }
}

impl From<serde_json::Error> for McpError {
    fn from(e: serde_json::Error) -> Self {
        McpError::Protocol(format!("json: {e}"))
    }
}
