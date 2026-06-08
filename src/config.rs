//! Startup configuration for an MCP server.

use clap::ValueEnum;

/// The transport an MCP server can speak over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum TransportKind {
    /// STDIN/STDOUT framing (the default; what `claude mcp add` uses).
    Stdio,
    /// WebSocket (`ws://host:port/ws`) — for hosted/remote use.
    Websocket,
    /// Unix-domain socket — for local co-located clients.
    Unix,
}

impl std::fmt::Display for TransportKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TransportKind::Stdio => "stdio",
            TransportKind::Websocket => "websocket",
            TransportKind::Unix => "unix",
        };
        f.write_str(s)
    }
}

/// Which transports a given server is willing to serve. Defaults to
/// stdio + websocket (the historical behaviour); unix is opt-in.
#[derive(Clone, Copy, Debug)]
pub struct EnabledTransports {
    /// Allow the stdio transport.
    pub stdio: bool,
    /// Allow the websocket transport.
    pub websocket: bool,
    /// Allow the unix-socket transport.
    pub unix: bool,
}

impl Default for EnabledTransports {
    fn default() -> Self {
        Self {
            stdio: true,
            websocket: true,
            unix: false,
        }
    }
}

impl EnabledTransports {
    /// Whether the given transport is permitted.
    pub fn allows(&self, kind: TransportKind) -> bool {
        match kind {
            TransportKind::Stdio => self.stdio,
            TransportKind::Websocket => self.websocket,
            TransportKind::Unix => self.unix,
        }
    }
}

/// MCP protocol versions this core knows how to negotiate, newest last.
pub const DEFAULT_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// Default upper bound on a single framed message (64 MiB).
pub const DEFAULT_MAX_CONTENT_LENGTH: usize = 64 * 1024 * 1024;

/// Everything mcp-core needs to know about a server at startup. Construct with
/// [`ServerConfig::new`] and tweak via the builder methods when the defaults
/// aren't right (e.g. [`ServerConfig::without_websocket`]).
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// `serverInfo.name` reported in the initialize response.
    pub name: String,
    /// `serverInfo.version` reported in the initialize response.
    pub version: String,
    /// Optional `instructions` string returned from initialize.
    pub instructions: Option<String>,
    /// Transports this server is allowed to serve.
    pub transports: EnabledTransports,
    /// Transport used when the CLI doesn't specify one.
    pub default_transport: TransportKind,
    /// Protocol versions to negotiate against (newest last).
    pub protocol_versions: Vec<String>,
    /// Whether `tools/list` can change at runtime (advertised as
    /// `capabilities.tools.listChanged`).
    pub tools_list_changed: bool,
    /// Upper bound on a single framed message, in bytes.
    pub max_content_length: usize,
}

impl ServerConfig {
    /// Create a config for a server named `name` at `version`
    /// (typically `env!("CARGO_PKG_VERSION")`), with sensible defaults.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            instructions: None,
            transports: EnabledTransports::default(),
            default_transport: TransportKind::Stdio,
            protocol_versions: DEFAULT_PROTOCOL_VERSIONS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            tools_list_changed: false,
            max_content_length: DEFAULT_MAX_CONTENT_LENGTH,
        }
    }

    /// Set the `instructions` string returned from initialize.
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    /// Disable the websocket transport for this server. If websocket was the
    /// default transport it falls back to stdio.
    pub fn without_websocket(mut self) -> Self {
        self.transports.websocket = false;
        if self.default_transport == TransportKind::Websocket {
            self.default_transport = TransportKind::Stdio;
        }
        self
    }

    /// Enable the unix-socket transport for this server.
    pub fn with_unix(mut self) -> Self {
        self.transports.unix = true;
        self
    }

    /// Override the transport used when the CLI doesn't specify one.
    pub fn default_transport(mut self, kind: TransportKind) -> Self {
        self.default_transport = kind;
        self
    }

    /// Declare that this server's tool list can change at runtime.
    pub fn tools_list_changed(mut self, yes: bool) -> Self {
        self.tools_list_changed = yes;
        self
    }

    /// Override the maximum accepted framed-message size.
    pub fn max_content_length(mut self, bytes: usize) -> Self {
        self.max_content_length = bytes;
        self
    }

    /// The newest supported protocol version (returned when a client requests
    /// one we don't recognise).
    pub(crate) fn latest_protocol_version(&self) -> &str {
        self.protocol_versions
            .last()
            .map(|s| s.as_str())
            .unwrap_or("2025-06-18")
    }
}
