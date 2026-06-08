//! Error types and JSON-RPC error codes shared across MCP servers.

use thiserror::Error;

/// Convenience result alias for `mcp-core` APIs.
pub type Result<T> = std::result::Result<T, Error>;

/// Top-level error returned by the server runners.
#[derive(Error, Debug)]
pub enum Error {
    /// Transport framing / connection error.
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),

    /// I/O error (binding a listener, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON (de)serialization error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// Invalid runtime configuration (e.g. a transport the server disabled).
    #[error("configuration error: {0}")]
    Config(String),
}

/// Transport-level framing and connection errors.
#[derive(Error, Debug)]
pub enum TransportError {
    /// Incoming frame was malformed or exceeded a size limit.
    #[error("invalid message: {0}")]
    InvalidMessage(String),

    /// The peer closed the stream (clean EOF).
    #[error("connection closed")]
    ConnectionClosed,

    /// I/O error while reading or writing the transport.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Standard JSON-RPC 2.0 error codes plus the MCP "not initialized" server code.
///
/// Using the correct codes lets MCP clients distinguish protocol errors
/// (retryable vs. fatal) from one another.
pub mod code {
    /// Invalid JSON was received (`-32700`).
    pub const PARSE_ERROR: i32 = -32700;
    /// The JSON sent is not a valid Request object (`-32600`).
    pub const INVALID_REQUEST: i32 = -32600;
    /// The method does not exist (`-32601`).
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// Invalid method parameters (`-32602`).
    pub const INVALID_PARAMS: i32 = -32602;
    /// Internal JSON-RPC error (`-32603`).
    pub const INTERNAL_ERROR: i32 = -32603;
    /// Server-defined: a request arrived before `initialize` (`-32002`).
    pub const NOT_INITIALIZED: i32 = -32002;
}
