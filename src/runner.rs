//! Server runners: wire a [`ServerCore`] to a transport and pump messages.

use std::sync::Arc;

use serde_json::Value;

use crate::args::CommonServeArgs;
use crate::config::TransportKind;
use crate::error::{Error, Result, TransportError, code};
use crate::server::{Dispatch, ServerCore, Session, error_response};
use crate::service::McpService;
use crate::transport::FramedTransport;

/// Serve over the transport selected by `common`, validated against the
/// server's [`crate::ServerConfig`]. This is the building block a server calls
/// after parsing its own CLI.
pub async fn serve(core: Arc<ServerCore>, common: &CommonServeArgs) -> Result<()> {
    let kind = common.transport.unwrap_or(core.config().default_transport);
    if !core.config().transports.allows(kind) {
        return Err(Error::Config(format!(
            "{kind} transport is not supported by this server"
        )));
    }
    match kind {
        TransportKind::Stdio => serve_stdio(core).await,
        TransportKind::Websocket => {
            #[cfg(feature = "websocket")]
            {
                serve_websocket(core, &common.host, common.port).await
            }
            #[cfg(not(feature = "websocket"))]
            {
                let _ = common;
                Err(Error::Config(
                    "this binary was built without the `websocket` feature".into(),
                ))
            }
        }
        TransportKind::Unix => {
            #[cfg(feature = "unix")]
            {
                let path = common.socket_path.as_deref().ok_or_else(|| {
                    Error::Config("--socket-path is required for the unix transport".into())
                })?;
                serve_unix(core, path).await
            }
            #[cfg(not(feature = "unix"))]
            {
                Err(Error::Config(
                    "this binary was built without the `unix` feature".into(),
                ))
            }
        }
    }
}

/// Serve a single client over stdio (the common case).
pub async fn serve_stdio(core: Arc<ServerCore>) -> Result<()> {
    let max = core.config().max_content_length;
    let mut transport = FramedTransport::stdio(max);
    let mut session = Session::new(core);
    pump(&mut transport, &mut session).await
}

/// Drive one framed transport with one session until EOF.
async fn pump<R, W>(transport: &mut FramedTransport<R, W>, session: &mut Session) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        let raw = match transport.read_message().await {
            Ok(m) => m,
            Err(Error::Transport(TransportError::ConnectionClosed)) => return Ok(()),
            // MC-4: a genuine read error (malformed framing, an oversize frame)
            // must propagate so the caller can exit non-zero — not be swallowed
            // as a clean shutdown, which would make the server silently go dark.
            Err(e) => return Err(e),
        };
        if raw.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                let resp = error_response(None, code::PARSE_ERROR, &format!("parse error: {e}"));
                transport
                    .write_message(&serde_json::to_string(&resp)?)
                    .await?;
                continue;
            }
        };
        let Dispatch {
            response,
            notifications,
        } = session.handle_message(value).await;
        if let Some(resp) = response {
            transport
                .write_message(&serde_json::to_string(&resp)?)
                .await?;
        }
        for notif in notifications {
            transport
                .write_message(&serde_json::to_string(&notif)?)
                .await?;
        }
    }
}

/// Prepare `path` for binding a unix socket: if something already exists there,
/// remove it **only if it is itself a socket** (a stale socket from a prior
/// run). Refuse to delete a regular file, directory, or other node — that would
/// be silent data loss if a server is pointed at the wrong path. A non-existent
/// path is fine.
#[cfg(feature = "unix")]
fn prepare_unix_socket_path(path: &str) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;

    // `symlink_metadata` does not follow symlinks, so a symlink is reported as
    // a symlink (not a socket) and is therefore refused rather than followed.
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            std::fs::remove_file(path)?;
            Ok(())
        }
        Ok(_) => Err(Error::Config(format!(
            "refusing to bind unix socket: {path} exists and is not a socket"
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Restrict a freshly-bound unix socket to the owner (`0600`) so other local
/// users can't connect to a server that trusts local callers.
#[cfg(feature = "unix")]
fn restrict_socket_perms(path: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Serve over a unix-domain socket, one [`Session`] per connection.
#[cfg(feature = "unix")]
pub async fn serve_unix(core: Arc<ServerCore>, path: &str) -> Result<()> {
    use tokio::io::BufReader;
    use tokio::net::UnixListener;

    // MC-8: only unlink a stale socket, never a regular file; then lock the
    // socket down to the owner.
    prepare_unix_socket_path(path)?;
    let listener = UnixListener::bind(path)?;
    restrict_socket_perms(path)?;
    eprintln!("mcp-core: listening on unix socket {path}");
    loop {
        let (stream, _addr) = listener.accept().await?;
        let core = Arc::clone(&core);
        let max = core.config().max_content_length;
        tokio::spawn(async move {
            let (read_half, write_half) = stream.into_split();
            let mut transport = FramedTransport::new(BufReader::new(read_half), write_half, max);
            let mut session = Session::new(core);
            if let Err(e) = pump(&mut transport, &mut session).await {
                eprintln!("mcp-core: unix connection error: {e}");
            }
        });
    }
}

/// Shared state for the websocket router: the core, plus (with the `auth`
/// feature) the authenticator checked on every connection.
#[cfg(feature = "websocket")]
#[derive(Clone)]
struct WsState {
    core: Arc<ServerCore>,
    #[cfg(feature = "auth")]
    auth: Arc<crate::auth::Authenticator>,
}

/// Serve over websocket at `ws://host:port/ws`, one [`Session`] per connection.
/// When the server's [`crate::config::WsAuth`] is not `None`, each connection's
/// `Authorization: Bearer <jwt>` header is validated before the upgrade (this
/// requires the `auth` feature).
#[cfg(feature = "websocket")]
pub async fn serve_websocket(core: Arc<ServerCore>, host: &str, port: u16) -> Result<()> {
    use axum::Router;
    use axum::routing::get;
    use tokio::net::TcpListener;

    #[cfg(not(feature = "auth"))]
    if !matches!(core.config().ws_auth, crate::config::WsAuth::None) {
        return Err(Error::Config(
            "websocket_auth is configured but mcp-core was built without the `auth` feature".into(),
        ));
    }

    #[cfg(feature = "auth")]
    let state = {
        let auth = Arc::new(
            crate::auth::Authenticator::from(
                core.config().ws_auth.clone(),
                core.config().ws_claim_bindings.clone(),
            )
            .await?,
        );
        if auth.is_enabled() {
            eprintln!("mcp-core: websocket Bearer-token authentication enabled");
        }
        WsState {
            core: Arc::clone(&core),
            auth,
        }
    };
    #[cfg(not(feature = "auth"))]
    let state = WsState {
        core: Arc::clone(&core),
    };

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .with_state(state);
    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).await?;
    eprintln!("mcp-core: websocket listening on ws://{addr}/ws");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(all(feature = "websocket", feature = "auth"))]
async fn ws_handler(
    axum::extract::State(state): axum::extract::State<WsState>,
    headers: axum::http::HeaderMap,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Err(e) = state.auth.check(&headers).await {
        return (axum::http::StatusCode::UNAUTHORIZED, e.to_string()).into_response();
    }
    let core = Arc::clone(&state.core);
    // MC-6: cap inbound frame size at the configured max so a huge websocket
    // message can't exhaust memory (mirrors the framed-transport cap).
    let max = core.config().max_content_length;
    ws.max_message_size(max)
        .on_upgrade(move |socket| ws_connection(socket, core))
}

#[cfg(all(feature = "websocket", not(feature = "auth")))]
async fn ws_handler(
    axum::extract::State(state): axum::extract::State<WsState>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> axum::response::Response {
    let core = Arc::clone(&state.core);
    // MC-6: cap inbound frame size at the configured max (see auth variant).
    let max = core.config().max_content_length;
    ws.max_message_size(max)
        .on_upgrade(move |socket| ws_connection(socket, core))
}

#[cfg(feature = "websocket")]
async fn ws_connection(socket: axum::extract::ws::WebSocket, core: Arc<ServerCore>) {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};

    let (mut sender, mut receiver) = socket.split();
    let mut session = Session::new(core);

    while let Some(msg) = receiver.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        let value: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                let resp = error_response(None, code::PARSE_ERROR, &format!("parse error: {e}"));
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = sender.send(Message::Text(s.into())).await;
                }
                continue;
            }
        };
        let Dispatch {
            response,
            notifications,
        } = session.handle_message(value).await;
        if let Some(resp) = response
            && let Ok(s) = serde_json::to_string(&resp)
            && sender.send(Message::Text(s.into())).await.is_err()
        {
            break;
        }
        for notif in notifications {
            if let Ok(s) = serde_json::to_string(&notif)
                && sender.send(Message::Text(s.into())).await.is_err()
            {
                break;
            }
        }
    }
}

/// Top-level convenience entrypoint that owns the whole CLI.
///
/// `mcp-core` defines a `serve` subcommand carrying [`CommonServeArgs`]
/// flattened together with the server's own `L: clap::Args`. It parses, hands
/// the server's args to `build` to construct the service, then serves. Use the
/// lower-level [`serve`] + [`ServerCore`] directly if you need extra
/// subcommands.
pub async fn run<L, S, Build, Fut>(config: crate::config::ServerConfig, build: Build) -> Result<()>
where
    L: clap::Args,
    S: McpService,
    Build: FnOnce(L) -> Fut,
    Fut: std::future::Future<Output = Result<S>>,
{
    use clap::{CommandFactory, FromArgMatches};

    #[derive(clap::Parser)]
    struct Cli<L: clap::Args> {
        #[command(subcommand)]
        command: Cmd<L>,
    }

    #[derive(clap::Subcommand)]
    enum Cmd<L: clap::Args> {
        /// Run the MCP server.
        Serve {
            #[command(flatten)]
            common: CommonServeArgs,
            #[command(flatten)]
            local: L,
        },
    }

    // MC-10: report the *server's* name/version (from ServerConfig) for
    // `--version` and in `--help`, rather than mcp-core's own crate metadata.
    let command = Cli::<L>::command()
        .name(config.name.clone())
        .version(config.version.clone());
    let matches = command.get_matches();
    let Cli {
        command: Cmd::Serve { common, local },
    } = Cli::<L>::from_arg_matches(&matches)
        .map_err(|e| Error::Config(format!("argument parsing: {e}")))?;

    let service = build(local).await?;
    let core = ServerCore::new(config, Arc::new(service));
    serve(core, &common).await
}

/// Like [`run`], but for servers with no extra CLI flags — the common case.
/// Avoids the turbofish and the empty `clap::Args` boilerplate:
///
/// ```no_run
/// # use mcp_core::{ServerConfig, McpService, ToolDef, ToolReply, CallError};
/// # use serde_json::Value;
/// # struct Svc;
/// # #[mcp_core::async_trait] impl McpService for Svc {
/// #   fn tools(&self) -> Vec<ToolDef> { vec![] }
/// #   async fn call_tool(&self, _: &str, _: &Value) -> Result<ToolReply, CallError> { Ok(ToolReply::text("")) }
/// # }
/// # async fn run() -> mcp_core::Result<()> {
/// let config = ServerConfig::new("demo-mcp", env!("CARGO_PKG_VERSION"));
/// mcp_core::run_simple(config, || async { Ok(Svc) }).await
/// # }
/// ```
pub async fn run_simple<S, Build, Fut>(
    config: crate::config::ServerConfig,
    build: Build,
) -> Result<()>
where
    S: McpService,
    Build: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<S>>,
{
    #[derive(clap::Args)]
    struct NoArgs {}

    run::<NoArgs, S, _, _>(config, |_no_args| build()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use crate::service::{CallError, ToolDef, ToolReply};
    use async_trait::async_trait;
    use serde_json::json;
    use tokio::io::BufReader;

    struct Demo;

    #[async_trait]
    impl McpService for Demo {
        fn tools(&self) -> Vec<ToolDef> {
            vec![ToolDef::new("echo", "echo", json!({"type": "object"}))]
        }
        async fn call_tool(
            &self,
            _name: &str,
            args: &Value,
        ) -> std::result::Result<ToolReply, CallError> {
            Ok(ToolReply::text(args.to_string()))
        }
    }

    fn core() -> Arc<ServerCore> {
        ServerCore::new(ServerConfig::new("demo", "0.0.0"), Arc::new(Demo))
    }

    fn core_capped(max: usize) -> Arc<ServerCore> {
        ServerCore::new(
            ServerConfig::new("demo", "0.0.0").max_content_length(max),
            Arc::new(Demo),
        )
    }

    /// MC-10(c): the stdio pump drives a real session over an in-memory
    /// transport end-to-end — initialize then a tool call get responses, and a
    /// clean EOF returns Ok(()).
    #[tokio::test]
    async fn pump_handles_initialize_and_tool_call_then_clean_eof() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo","arguments":{"x":1}}}"#,
            "\n",
        )
        .as_bytes()
        .to_vec();
        let mut out: Vec<u8> = Vec::new();
        let mut transport = FramedTransport::new(BufReader::new(&input[..]), &mut out, 1024);
        let mut session = Session::new(core());
        let result = pump(&mut transport, &mut session).await;
        assert!(result.is_ok(), "clean EOF must return Ok: {result:?}");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"protocolVersion\""), "init reply: {text}");
        assert!(text.contains("\"isError\":false"), "tool reply: {text}");
    }

    /// MC-4: a transport read error (here an oversize, newline-free frame that
    /// exceeds the cap) must propagate as Err, not be swallowed as Ok(()) — so
    /// the process can exit non-zero instead of silently going dark.
    #[tokio::test]
    async fn pump_propagates_transport_read_error() {
        let input = vec![b'a'; 10_000]; // no newline, far over the cap, then EOF
        let mut out: Vec<u8> = Vec::new();
        let mut transport = FramedTransport::new(BufReader::new(&input[..]), &mut out, 64);
        let mut session = Session::new(core_capped(64));
        let result = pump(&mut transport, &mut session).await;
        assert!(
            matches!(result, Err(Error::Transport(_))),
            "oversize frame must propagate as Err, got {result:?}"
        );
    }

    /// A malformed JSON line is recoverable: the pump writes a PARSE_ERROR
    /// response and keeps going to a clean EOF (Ok).
    #[tokio::test]
    async fn pump_replies_parse_error_and_continues() {
        let input = b"{not json}\n".to_vec();
        let mut out: Vec<u8> = Vec::new();
        let mut transport = FramedTransport::new(BufReader::new(&input[..]), &mut out, 1024);
        let mut session = Session::new(core());
        let result = pump(&mut transport, &mut session).await;
        assert!(result.is_ok(), "malformed JSON is recoverable: {result:?}");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(&code::PARSE_ERROR.to_string()),
            "expected parse-error reply: {text}"
        );
    }

    /// MC-8: socket-path safety — refuse to clobber a non-socket file, but
    /// happily replace a stale socket or use a fresh path.
    #[cfg(feature = "unix")]
    #[tokio::test]
    async fn prepare_unix_socket_path_refuses_regular_file() {
        let dir = std::env::temp_dir().join(format!("mcp-core-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("not-a-socket");
        std::fs::write(&file, b"important data").unwrap();

        let err = prepare_unix_socket_path(file.to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().contains("not a socket"),
            "must refuse to delete a regular file: {err}"
        );
        // The file must be untouched.
        assert_eq!(std::fs::read(&file).unwrap(), b"important data");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "unix")]
    #[tokio::test]
    async fn prepare_unix_socket_path_allows_fresh_and_stale_socket() {
        let dir = std::env::temp_dir().join(format!("mcp-core-test-sock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Fresh path (nothing there): OK, nothing removed.
        let fresh = dir.join("fresh.sock");
        prepare_unix_socket_path(fresh.to_str().unwrap()).unwrap();
        assert!(!fresh.exists());

        // A stale socket left by a previous run: OK to remove.
        let stale = dir.join("stale.sock");
        let listener = tokio::net::UnixListener::bind(&stale).unwrap();
        drop(listener);
        assert!(stale.exists());
        prepare_unix_socket_path(stale.to_str().unwrap()).unwrap();
        assert!(!stale.exists(), "stale socket should have been removed");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
