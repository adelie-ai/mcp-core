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
            Err(e) => {
                eprintln!("mcp-core: transport read error: {e}");
                return Ok(());
            }
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

/// Serve over a unix-domain socket, one [`Session`] per connection.
#[cfg(feature = "unix")]
pub async fn serve_unix(core: Arc<ServerCore>, path: &str) -> Result<()> {
    use tokio::io::BufReader;
    use tokio::net::UnixListener;

    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
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

/// Serve over websocket at `ws://host:port/ws`, one [`Session`] per connection.
#[cfg(feature = "websocket")]
pub async fn serve_websocket(core: Arc<ServerCore>, host: &str, port: u16) -> Result<()> {
    use axum::Router;
    use axum::extract::{State, ws::WebSocketUpgrade};
    use axum::response::Response;
    use axum::routing::get;
    use tokio::net::TcpListener;

    async fn ws_handler(ws: WebSocketUpgrade, State(core): State<Arc<ServerCore>>) -> Response {
        ws.on_upgrade(move |socket| ws_connection(socket, core))
    }

    let app = Router::new().route("/ws", get(ws_handler)).with_state(core);
    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).await?;
    eprintln!("mcp-core: websocket listening on ws://{addr}/ws");
    axum::serve(listener, app).await?;
    Ok(())
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
    use clap::Parser;

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

    let Cli {
        command: Cmd::Serve { common, local },
    } = Cli::<L>::parse();

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
