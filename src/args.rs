//! The common `serve` command-line arguments every MCP server shares.
//!
//! A server flattens these into its own `serve` subcommand (or lets
//! [`crate::run`] own the whole CLI) and adds its own flags alongside.

use crate::config::TransportKind;

/// Transport-selection flags common to every MCP server. Flatten this into a
/// server's clap `Serve` variant with `#[command(flatten)]`.
#[derive(clap::Args, Clone, Debug)]
pub struct CommonServeArgs {
    /// Transport to serve over. Defaults to the server's configured default
    /// (usually stdio).
    #[arg(long, value_enum)]
    pub transport: Option<TransportKind>,

    /// Host to bind for the websocket transport. `127.0.0.1` keeps it local;
    /// use `0.0.0.0` to expose it (no auth — be careful).
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Port to bind for the websocket transport.
    #[arg(long, default_value_t = 8080)]
    pub port: u16,

    /// Filesystem path for the unix-socket transport.
    #[arg(long)]
    pub socket_path: Option<String>,
}

impl Default for CommonServeArgs {
    fn default() -> Self {
        Self {
            transport: None,
            host: "127.0.0.1".to_string(),
            port: 8080,
            socket_path: None,
        }
    }
}
