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

/// Which transports a given server is willing to serve. Defaults to **stdio
/// only**: both the network-facing websocket transport and the unix-socket
/// transport are opt-in.
///
/// MC-7: websocket used to be on by default, so every server exposed an
/// (often unauthenticated) network transport unless it remembered
/// `.without_websocket()` — half the fleet forgot. It now fails closed: a
/// server must explicitly call [`ServerConfig::with_websocket`] to serve over
/// the network.
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
            websocket: false,
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

/// WebSocket authentication strategy. Validated before the upgrade. Anything
/// other than [`WsAuth::None`] requires mcp-core to be built with the `auth`
/// feature; it is ignored by the stdio/unix transports (local = trusted).
///
/// Issuer/audience claim bindings (MC-2) are configured separately, via
/// [`ServerConfig::websocket_expected_issuer`] /
/// [`ServerConfig::websocket_expected_audience`], so they apply uniformly to
/// every strategy. The [`WsAuth::OidcIssuer`] strategy additionally binds the
/// token's `iss` to the discovered issuer automatically.
#[derive(Clone, Debug, Default)]
pub enum WsAuth {
    /// No authentication (default) — anyone who can reach the socket.
    #[default]
    None,
    /// Validate a Bearer JWT signed with this HMAC shared secret (HS256).
    Secret(String),
    /// Validate a Bearer JWT against the JWKS document at this URL.
    Jwks(String),
    /// Validate a Bearer JWT via OIDC discovery from this issuer URL (fetches
    /// `<issuer>/.well-known/openid-configuration` to find the JWKS).
    OidcIssuer(String),
}

/// Issuer/audience claim bindings applied on top of a [`WsAuth`] strategy
/// (MC-2). An empty set leaves the corresponding claim unchecked; when set, the
/// token's `iss` must equal `issuer` and its `aud` must contain `audience`.
#[derive(Clone, Debug, Default)]
pub struct WsClaimBindings {
    /// Required `iss` claim. `None` leaves the issuer unchecked (except for
    /// [`WsAuth::OidcIssuer`], which always binds to its discovered issuer).
    pub issuer: Option<String>,
    /// Required `aud` claim (the token's `aud` must contain this value).
    pub audience: Option<String>,
}

/// MCP protocol versions this core knows how to negotiate, newest last.
pub const DEFAULT_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// Default upper bound on a single framed message (64 MiB).
pub const DEFAULT_MAX_CONTENT_LENGTH: usize = 64 * 1024 * 1024;

/// Everything mcp-core needs to know about a server at startup. Construct with
/// [`ServerConfig::new`] and tweak via the builder methods when the defaults
/// aren't right (e.g. [`ServerConfig::with_websocket`]).
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
    /// WebSocket authentication strategy (default [`WsAuth::None`]). Requires
    /// the `auth` feature when not `None`.
    pub ws_auth: WsAuth,
    /// Issuer/audience bindings layered on top of [`Self::ws_auth`] (MC-2).
    pub ws_claim_bindings: WsClaimBindings,
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
            ws_auth: WsAuth::None,
            ws_claim_bindings: WsClaimBindings::default(),
        }
    }

    /// Set the `instructions` string returned from initialize.
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    /// Enable the network-facing websocket transport for this server (MC-7:
    /// opt-in). Until called, requesting `--transport websocket` fails closed
    /// with a clear config error. Pair it with [`Self::websocket_auth`] for any
    /// untrusted network.
    pub fn with_websocket(mut self) -> Self {
        self.transports.websocket = true;
        self
    }

    /// Disable the websocket transport for this server. If websocket was the
    /// default transport it falls back to stdio.
    ///
    /// MC-7: websocket is now off by default, so this is rarely needed; it is
    /// retained so a server that toggles transports dynamically can still turn
    /// it back off.
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

    /// Require Bearer-token authentication on the websocket transport. Needs the
    /// `auth` feature; the stdio/unix transports ignore it.
    pub fn websocket_auth(mut self, auth: WsAuth) -> Self {
        self.ws_auth = auth;
        self
    }

    /// Require the validated token's `iss` claim to equal `issuer` (MC-2).
    /// Applies to every [`WsAuth`] strategy. Without this, any token from the
    /// same IdP — issued for a different service — would authenticate.
    pub fn websocket_expected_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.ws_claim_bindings.issuer = Some(issuer.into());
        self
    }

    /// Require the validated token's `aud` claim to contain `audience` (MC-2).
    /// Applies to every [`WsAuth`] strategy.
    pub fn websocket_expected_audience(mut self, audience: impl Into<String>) -> Self {
        self.ws_claim_bindings.audience = Some(audience.into());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_off_by_default() {
        // MC-7: a fresh config exposes stdio only; the network transport is
        // opt-in and fails closed until `with_websocket()`.
        let cfg = ServerConfig::new("x", "1.0");
        assert!(cfg.transports.allows(TransportKind::Stdio));
        assert!(
            !cfg.transports.allows(TransportKind::Websocket),
            "websocket must be off by default (MC-7)"
        );
        assert!(!cfg.transports.allows(TransportKind::Unix));
    }

    #[test]
    fn with_websocket_opts_in() {
        let cfg = ServerConfig::new("x", "1.0").with_websocket();
        assert!(cfg.transports.allows(TransportKind::Websocket));
    }

    #[test]
    fn without_websocket_still_works() {
        // Idempotent / round-trips: opt in then back out.
        let cfg = ServerConfig::new("x", "1.0")
            .with_websocket()
            .without_websocket();
        assert!(!cfg.transports.allows(TransportKind::Websocket));
    }

    #[test]
    fn claim_binding_builders() {
        let cfg = ServerConfig::new("x", "1.0")
            .websocket_expected_issuer("https://idp.example")
            .websocket_expected_audience("mcp-core");
        assert_eq!(
            cfg.ws_claim_bindings.issuer.as_deref(),
            Some("https://idp.example")
        );
        assert_eq!(cfg.ws_claim_bindings.audience.as_deref(), Some("mcp-core"));
    }
}
