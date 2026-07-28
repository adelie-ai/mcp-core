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
//!
//! ## Tool schema dialect
//!
//! Tool schemas are **JSON Schema 2020-12**, and say so by *omitting* `$schema`
//! — per SEP-1613 the MCP spec defines 2020-12 as the default dialect and reads
//! a `$schema` key as an opt-*out*. Do not add one to declare 2020-12; add one
//! only if a tool genuinely needs an older draft.
//!
//! mcp-core passes `input_schema` to the wire verbatim: it never injects,
//! strips, or rewrites a key. The dialect is therefore a contract between the
//! server author and the client, and this list is the whole of it:
//!
//! - Write plain `type` / `properties` / `required` objects. They mean the same
//!   thing in every draft, which is why the fleet has never hit this.
//! - `items` takes a **single schema**. The draft-07 array form (tuple
//!   validation) became `prefixItems` in 2020-12 and an array `items` is
//!   misread by a 2020-12 validator.
//! - `exclusiveMinimum` / `exclusiveMaximum` take a **number**, not the
//!   draft-04 boolean.
//! - Use `$defs`, not `definitions`, for local subschemas.
//! - Prefer `{"type": "object", "additionalProperties": false}` for a tool that
//!   takes no arguments.
//! - Avoid a top-level combinator (`oneOf` / `anyOf` / `allOf`). It is legal
//!   JSON Schema, but some model providers reject a tool whose root is not a
//!   plain object — which has already cost us one production outage.
//!
//! Generating schemas with `schemars` 1.x is 2020-12-native; strip the
//! `$schema` and `title` keys it emits so the result follows the convention
//! above. `schemars` 0.8 emits draft-07 and is not suitable for tool schemas.

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
    WsClaimBindings,
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
