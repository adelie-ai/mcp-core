//! # mcp-core
//!
//! Shared core for the adelie-ai hand-rolled MCP servers. It owns the parts
//! that were previously copy-pasted (and silently drifted) across every
//! `*-mcp` repo:
//!
//! - **Protocol** — JSON-RPC 2.0 dispatch with correct error codes, protocol
//!   version negotiation, spec-compliant `tools/call` results (tool failures as
//!   `isError` content, not protocol errors), notifications that never get a
//!   response, and an `initialize` result that does *not* leak a top-level
//!   `tools` key.
//! - **Transports** — stdio + unix (framed, with a size cap) and an optional
//!   feature-gated websocket transport. `initialized` state is per-connection.
//! - **CLI** — a standard Clap `serve` setup ([`CommonServeArgs`]); a server
//!   adds its own flags and hands mcp-core a [`ServerConfig`] plus an
//!   [`McpService`] implementation.
//!
//! ## Minimal server
//!
//! ```no_run
//! use std::sync::Arc;
//! use mcp_core::{async_trait, CallError, McpService, ServerConfig, ToolDef, ToolReply};
//! use serde_json::{json, Value};
//!
//! struct Echo;
//!
//! #[async_trait]
//! impl McpService for Echo {
//!     fn tools(&self) -> Vec<ToolDef> {
//!         vec![ToolDef::new("echo", "echo the input", json!({
//!             "type": "object",
//!             "properties": { "text": { "type": "string" } },
//!             "required": ["text"],
//!         }))]
//!     }
//!     async fn call_tool(&self, name: &str, args: &Value) -> Result<ToolReply, CallError> {
//!         match name {
//!             "echo" => Ok(ToolReply::text(
//!                 args.get("text").and_then(Value::as_str).unwrap_or_default(),
//!             )),
//!             other => Err(CallError::tool(format!("unknown tool: {other}"))),
//!         }
//!     }
//! }
//!
//! // The server's own `serve` flags (mcp-core flattens CommonServeArgs in).
//! #[derive(clap::Args)]
//! struct Local {
//!     /// example server-specific flag
//!     #[arg(long)]
//!     greeting: Option<String>,
//! }
//!
//! #[tokio::main]
//! async fn main() -> mcp_core::Result<()> {
//!     let config = ServerConfig::new("echo-mcp", env!("CARGO_PKG_VERSION"));
//!     mcp_core::run::<Local, _, _, _>(config, |_local| async { Ok(Echo) }).await
//! }
//! ```
//!
//! Stdio and unix transports work with the default features; enable websocket
//! with `features = ["websocket"]`. A server with no extra flags can use
//! [`run_simple`] (no turbofish, no empty args struct) instead of [`run`].

pub mod args;
#[cfg(feature = "auth")]
pub mod auth;
pub mod config;
pub mod error;
mod runner;
mod server;
pub mod service;
pub mod transport;

pub use args::CommonServeArgs;
pub use config::{
    DEFAULT_MAX_CONTENT_LENGTH, EnabledTransports, ServerConfig, TransportKind, WsAuth,
};
pub use error::{Error, Result, TransportError, code};
pub use server::{Dispatch, ServerCore, Session};
pub use service::{CallError, Content, McpService, ToolDef, ToolReply};

#[cfg(feature = "unix")]
pub use runner::serve_unix;
#[cfg(feature = "websocket")]
pub use runner::serve_websocket;
pub use runner::{run, run_simple, serve, serve_stdio};

/// Re-exported so servers can write `#[mcp_core::async_trait]` without adding
/// `async-trait` to their own dependencies.
pub use async_trait::async_trait;
