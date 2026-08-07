//! Protocol core: per-connection [`Session`] dispatch and shared [`ServerCore`].

use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};
use tracing::Instrument;

use crate::config::{ServerConfig, TransportKind};
use crate::error::code;
use crate::service::{CallError, McpService};
use crate::telemetry::metrics::{self, Label};

/// The `method` label used for a payload that never named a method: a batch
/// array, a scalar, a wrong `jsonrpc` version, or a request with no `method`
/// member.
const METHOD_INVALID: &str = "invalid";

/// The `method` label used for a method this server does not implement.
///
/// A caller chooses the method name, so labelling the counter with it verbatim
/// would let a probing client fill the registry's cardinality budget and push
/// the real methods into the overflow series. The name itself still reaches the
/// span, which costs no series, bounded there by [`Safe`] instead.
const METHOD_OTHER: &str = "other";

/// The `request_id` span field used for a notification, which has no id.
const NO_REQUEST_ID: &str = "-";

/// The most bytes of a caller-chosen name a log field keeps.
///
/// The same limit the metrics facade puts on a label value, so one name reads
/// the same way whichever signal an operator looks at.
const MAX_NAME_BYTES: usize = 128;

/// The most bytes of a diagnostic message a log field keeps.
///
/// Wider than a name, because the text is mostly what this crate or the server
/// wrote and is worth keeping whole.
const MAX_MESSAGE_BYTES: usize = 1024;

/// What replaces a character that could end a log line.
const REPLACEMENT: char = '\u{fffd}';

/// What marks a value the cap cut short.
const TRUNCATED: &str = "...";

/// A value a caller can influence, rendered safely into a log field.
///
/// Two things make a raw value unsafe here. The console layer writes a field
/// value straight into a line, so a newline in one produces what reads as a
/// second genuine line, with a real timestamp column, level and target; and an
/// ANSI escape survives, because turning the formatter's own colour off does
/// not strip an escape carried inside a value. Control characters are replaced
/// rather than dropped, so the field still shows that something was there.
///
/// Length is the second problem. Nothing bounds a method name, a tool name or
/// a request id short of the transport's frame cap, which is measured in
/// megabytes, and with the `otel` feature on a span field leaves the process
/// verbatim. One request could otherwise ship as much as it liked.
///
/// This wraps what a *caller* reaches. It does not wrap the socket path or the
/// listen address, which come from the operator's own command line and are
/// written once at startup.
pub(crate) struct Safe<'a> {
    value: &'a str,
    cap: usize,
}

impl<'a> Safe<'a> {
    /// A name the caller chose: a method, a tool, a request id. Short by
    /// nature, so the tight cap costs nothing real.
    pub(crate) fn name(value: &'a str) -> Self {
        Self {
            value,
            cap: MAX_NAME_BYTES,
        }
    }

    /// A diagnostic message. Mostly text this crate or the server wrote, but a
    /// server routinely quotes the caller's own input back inside it.
    pub(crate) fn message(value: &'a str) -> Self {
        Self {
            value,
            cap: MAX_MESSAGE_BYTES,
        }
    }
}

impl std::fmt::Display for Safe<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::fmt::Write;

        let mut written = 0;
        for character in self.value.chars() {
            let safe = if is_deceptive(character) {
                REPLACEMENT
            } else {
                character
            };
            let length = safe.len_utf8();
            if written + length > self.cap {
                return f.write_str(TRUNCATED);
            }
            f.write_char(safe)?;
            written += length;
        }
        Ok(())
    }
}

/// Whether this character could make a field render as something other than
/// what it is.
///
/// Three kinds, and each defeats a different reader.
///
/// - `char::is_control` covers C0, C1 and DEL. Those end a line or drive a
///   terminal.
/// - U+2028 LINE SEPARATOR and U+2029 PARAGRAPH SEPARATOR are categories Zl
///   and Zp. `is_control` does not cover them, and some log viewers, and every
///   JSON consumer, treat them as a line break.
/// - The bidi controls leave the bytes alone and reverse what a person sees,
///   so a name renders as something it is not. This is the trojan-source
///   class, and the set is the one rustc's own
///   `text_direction_codepoint_in_literal` lint covers.
///
/// The wider Cf category is deliberately not swept. A zero-width joiner is Cf
/// and carries the emoji sequences a person does want to read, and hiding text
/// is a weaker problem than reversing it.
fn is_deceptive(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{2028}'
                | '\u{2029}'
                | '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

/// A JSON value as a log field, rendered only if a subscriber asks for it.
///
/// A JSON rendering escapes the C0 controls on its own, which is what made
/// this path look safe. It does not escape U+2028, U+2029 or a bidi control,
/// and it bounds nothing, so it goes through [`Safe`] like any other value a
/// caller reaches.
struct SafeJson<'a>(&'a Value);

impl std::fmt::Display for SafeJson<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", Safe::message(&self.0.to_string()))
    }
}

/// A JSON-RPC id as a span field, rendered only if a subscriber asks for it.
struct RequestId<'a>(Option<&'a Value>);

impl std::fmt::Display for RequestId<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            // A JSON rendering escapes a control character on its own. The cap
            // is what [`Safe`] adds here, because a caller sets the id and
            // nothing else bounds how long it is.
            Some(id) => write!(f, "{}", Safe::name(&id.to_string())),
            None => f.write_str(NO_REQUEST_ID),
        }
    }
}

/// Immutable, shared server state: the config and the service implementation.
/// Cheap to clone (it's behind an `Arc`); one is shared by every connection.
pub struct ServerCore {
    config: ServerConfig,
    service: Arc<dyn McpService>,
}

impl ServerCore {
    /// Build a shared core from a config and a service implementation.
    pub fn new(config: ServerConfig, service: Arc<dyn McpService>) -> Arc<Self> {
        Arc::new(Self { config, service })
    }

    /// The server configuration.
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }
}

/// The output of handling one JSON-RPC message: an optional response (absent
/// for notifications) and any server-initiated notifications to flush.
#[derive(Debug, Default)]
pub struct Dispatch {
    /// The response to send back, if the message was a request.
    pub response: Option<Value>,
    /// Notifications to emit after the response (e.g. `tools/list_changed`).
    pub notifications: Vec<Value>,
}

/// Per-connection session. Holds the `initialized` handshake state so that two
/// concurrent connections (e.g. websocket clients) don't share it. Create one
/// per stdio process / per websocket or unix connection.
pub struct Session {
    core: Arc<ServerCore>,
    initialized: bool,
    transport: TransportKind,
}

enum Outcome {
    /// A result value for a request.
    Result(Value),
    /// A JSON-RPC error for a request.
    Error(i32, String),
    /// Nothing to send (a notification was handled).
    None,
}

impl Session {
    /// Start a fresh session bound to the shared core, served over stdio.
    /// Use [`Self::on_transport`] for a session on another transport.
    pub fn new(core: Arc<ServerCore>) -> Self {
        Self {
            core,
            initialized: false,
            transport: TransportKind::Stdio,
        }
    }

    /// Record which transport this session arrived on.
    ///
    /// The value reaches every request span, so a failing request can be told
    /// apart from one that came through a different door. It changes nothing
    /// about how the session behaves.
    #[must_use]
    pub fn on_transport(mut self, transport: TransportKind) -> Self {
        self.transport = transport;
        self
    }

    /// Handle one parsed JSON-RPC message and produce the response (if any) and
    /// any notifications to flush.
    pub async fn handle_message(&mut self, message: Value) -> Dispatch {
        // The borrows end with the span, so nothing here allocates. A field is
        // rendered only if a subscriber asks for it, and a server with logging
        // off must not pay for a string it will never print.
        let span = {
            let method = message
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or(METHOD_INVALID);
            // D10: the method name, the request id and the transport are names
            // and ids, so they belong at INFO. The params never join them.
            tracing::info_span!(
                "mcp.request",
                method = %Safe::name(method),
                request_id = %RequestId(message.get("id")),
                transport = %self.transport,
            )
        };
        self.dispatch(message).instrument(span).await
    }

    /// Route one message and record what it cost.
    async fn dispatch(&mut self, message: Value) -> Dispatch {
        let started = Instant::now();
        let (dispatch, method) = self.route(message).await;
        let labels = [Label::new("method", method)];
        metrics::increment("mcp.requests", &labels);
        metrics::record_duration("mcp.request.duration", started.elapsed(), &labels);
        dispatch
    }

    /// Handle one parsed JSON-RPC message, and report the `method` label its
    /// counters belong under.
    async fn route(&mut self, message: Value) -> (Dispatch, String) {
        // MC-5: a JSON-RPC payload must be a single Request/Notification object.
        // An array (batch) or any non-object scalar is not a valid Request —
        // batching was removed from the spec in 2025-06-18 and every version we
        // negotiate is at or above that — so answer INVALID_REQUEST with a null
        // id rather than silently dropping it (an array has no `id`, so the old
        // code treated it as a notification and never replied, hanging the
        // client).
        if !message.is_object() {
            let msg = if message.is_array() {
                "batch requests (JSON arrays) are not supported"
            } else {
                "request must be a JSON object"
            };
            return (
                Dispatch {
                    response: Some(error_response(
                        Some(Value::Null),
                        code::INVALID_REQUEST,
                        msg,
                    )),
                    notifications: Vec::new(),
                },
                METHOD_INVALID.to_string(),
            );
        }

        let id = message.get("id").cloned();
        // Per JSON-RPC, a message with no `id` member is a notification and
        // must never receive a response — not even an error.
        let is_request = message.get("id").is_some();

        if let Some(version) = message.get("jsonrpc").and_then(Value::as_str)
            && version != "2.0"
        {
            return (
                Self::finish(
                    is_request,
                    id,
                    Outcome::Error(
                        code::INVALID_REQUEST,
                        format!("invalid jsonrpc version: {version}"),
                    ),
                    Vec::new(),
                ),
                METHOD_INVALID.to_string(),
            );
        }

        let method = message.get("method").and_then(Value::as_str);
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let mut notifications = Vec::new();
        // A method this server does not implement is a name the caller chose,
        // so it is counted under `other` rather than under itself.
        let mut method_label = method.unwrap_or(METHOD_INVALID).to_string();

        let outcome = match method {
            Some("initialize") => Outcome::Result(self.handle_initialize(&params)),
            Some("notifications/initialized") | Some("initialized") => {
                self.initialized = true;
                Outcome::None
            }
            Some("ping") => Outcome::Result(json!({})),
            Some("tools/list") => {
                if !self.initialized {
                    Outcome::Error(code::NOT_INITIALIZED, "server not initialized".into())
                } else {
                    Outcome::Result(json!({ "tools": self.tools_json() }))
                }
            }
            Some("tools/call") => self.handle_tools_call(&params, &mut notifications).await,
            // `shutdown` is a non-spec (LSP-style) convenience extension — see
            // `McpService::shutdown`. Standard MCP clients close the transport.
            Some("shutdown") => {
                self.core.service.shutdown().await;
                self.initialized = false;
                Outcome::Result(Value::Null)
            }
            Some(other) => {
                method_label = METHOD_OTHER.to_string();
                Outcome::Error(code::METHOD_NOT_FOUND, format!("method not found: {other}"))
            }
            None => {
                method_label = METHOD_INVALID.to_string();
                Outcome::Error(code::INVALID_REQUEST, "missing method".into())
            }
        };

        (
            Self::finish(is_request, id, outcome, notifications),
            method_label,
        )
    }

    fn handle_initialize(&mut self, params: &Value) -> Value {
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or_else(|| self.core.config.latest_protocol_version());
        let negotiated = self.negotiate_version(requested);

        // Set initialized here (not only on the `initialized` notification):
        // some clients issue tools/list immediately after initialize without
        // sending the notification.
        self.initialized = true;

        let mut server_info = json!({
            "name": self.core.config.name,
            "version": self.core.config.version,
        });

        // SEP-973 added title/description/websiteUrl to `Implementation` in
        // 2025-11-25, so an older session must see the shape it expects. The
        // gate is cheap here because `negotiated` is already in hand; revision
        // strings are `YYYY-MM-DD`, so string order is chronological order.
        if negotiated.as_str() >= "2025-11-25" {
            for (key, value) in [
                ("title", &self.core.config.title),
                ("description", &self.core.config.description),
                ("websiteUrl", &self.core.config.website_url),
            ] {
                if let Some(value) = value {
                    server_info[key] = json!(value);
                }
            }
        }

        let mut result = json!({
            "protocolVersion": negotiated,
            "serverInfo": server_info,
            "capabilities": {
                "tools": { "listChanged": self.core.config.tools_list_changed },
            },
        });
        if let Some(instructions) = &self.core.config.instructions {
            result["instructions"] = json!(instructions);
        }
        result
    }

    async fn handle_tools_call(&self, params: &Value, notifications: &mut Vec<Value>) -> Outcome {
        if !self.initialized {
            return Outcome::Error(code::NOT_INITIALIZED, "server not initialized".into());
        }
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Outcome::Error(code::INVALID_PARAMS, "missing tool name".into());
        };
        let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

        // The tool name is a name, so it belongs at INFO and on the span. The
        // arguments are content — file paths, command lines, search text — so
        // D10 keeps them off both, and puts them on a DEBUG line instead.
        let span = tracing::info_span!("mcp.tools.call", tool = %Safe::name(name));
        async move {
            tracing::debug!(arguments = %SafeJson(&arguments), "tool call arguments");
            let started = Instant::now();
            let result = self.core.service.call_tool(name, &arguments).await;
            let outcome = match &result {
                Ok(_) => "ok",
                Err(CallError::Internal(_)) => "error",
                Err(CallError::Tool(_) | CallError::InvalidParams(_)) => "tool_error",
            };
            // The tool name is chosen by the caller, so the registry's
            // cardinality cap is what bounds this series.
            metrics::increment(
                "mcp.tools.call",
                &[Label::new("tool", name), Label::new("outcome", outcome)],
            );
            metrics::record_duration(
                "mcp.tools.call.duration",
                started.elapsed(),
                &[Label::new("tool", name)],
            );

            match result {
                Ok(reply) => {
                    if reply.tools_list_changed {
                        notifications.push(json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/tools/list_changed",
                        }));
                    }
                    Outcome::Result(reply.to_result_json())
                }
                // Tool failures are a successful response with isError content.
                //
                // Why InvalidParams lands here too (SEP-1303): bad arguments are
                // something the model supplied and can correct, so it has to see
                // them. A JSON-RPC error is invisible to the model and takes the
                // turn with it. Only faults the model cannot act on stay protocol
                // errors — a malformed request (no tool name, above) and an
                // internal server fault (below).
                //
                // A tool that declines is a normal outcome, not a fault, so it
                // is reported at DEBUG. Its message can quote the arguments,
                // which is the other reason it cannot go higher.
                // A server routinely quotes the caller's own tool name or
                // arguments back in the message, so it is wrapped like any
                // other caller-supplied value.
                Err(CallError::Tool(msg) | CallError::InvalidParams(msg)) => {
                    tracing::debug!(reason = %Safe::message(&msg), "tool returned an error result");
                    Outcome::Result(tool_error_result(&msg))
                }
                // A fault, so the line stays at ERROR: an operator has to see
                // that a tool broke without raising the level first. The
                // message does not stay with it. `CallError::internal` is as
                // free-form as the other two variants, and the idiom a server
                // reaches for on an unexpected IO fault quotes an argument back
                // (`failed to read {path}`), which D10 keeps off the INFO band.
                // The span around this line carries the tool, the method, the
                // request id and the transport, so the ERROR is still
                // actionable and still correlated without it.
                Err(CallError::Internal(msg)) => {
                    tracing::error!("tool call failed");
                    tracing::debug!(error = %Safe::message(&msg), "tool call failure detail");
                    Outcome::Error(code::INTERNAL_ERROR, msg)
                }
            }
        }
        .instrument(span)
        .await
    }

    fn tools_json(&self) -> Value {
        Value::Array(
            self.core
                .service
                .tools()
                .iter()
                .map(crate::service::ToolDef::to_json)
                .collect(),
        )
    }

    /// Resolve the version for this session: echo the request when we support
    /// it, otherwise answer with the newest we do. Answering with a supported
    /// version rather than echoing an unsupported one is the spec's required
    /// behaviour, so retiring a revision degrades a client rather than breaking
    /// it.
    ///
    /// Trap for whoever adds `MCP-Protocol-Version` header handling: the
    /// Streamable HTTP rules say a server seeing no header SHOULD assume
    /// `2025-03-26` and MUST answer `400` for a version it does not support.
    /// Composed literally, and now that `2025-03-26` is retired, that 400s every
    /// header-less client. Treat an absent header as "unknown — use the version
    /// negotiated for this session", never as a literal `2025-03-26`.
    fn negotiate_version(&self, requested: &str) -> String {
        if self
            .core
            .config
            .protocol_versions
            .iter()
            .any(|v| v == requested)
        {
            requested.to_string()
        } else {
            self.core.config.latest_protocol_version().to_string()
        }
    }

    fn finish(
        is_request: bool,
        id: Option<Value>,
        outcome: Outcome,
        notifications: Vec<Value>,
    ) -> Dispatch {
        let response = match outcome {
            Outcome::Result(result) if is_request => Some(success_response(id, result)),
            Outcome::Error(c, msg) if is_request => Some(error_response(id, c, &msg)),
            // Notifications, and the `initialized` no-op, get no response.
            _ => None,
        };
        Dispatch {
            response,
            notifications,
        }
    }
}

/// Build a JSON-RPC success response.
pub fn success_response(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Build a JSON-RPC error response.
pub fn error_response(id: Option<Value>, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

/// A `tools/call` result that signals failure via `isError: true` content
/// rather than a JSON-RPC protocol error.
fn tool_error_result(message: &str) -> Value {
    json!({
        "isError": true,
        "content": [{ "type": "text", "text": message }],
    })
}

/// The shared sanitiser must agree with this crate's own, character for
/// character, before this crate stops using its own.
///
/// Thirteen servers inherit whatever this crate decides here, so the swap has
/// to be proved rather than argued. Each test drives the same input through
/// both and compares the rendered output. A disagreement is a defect in one of
/// them, not something to reconcile at the call site.
#[cfg(test)]
mod shared_safe_agreement {
    use super::{Safe, SafeJson};
    use adelie_telemetry::Safe as Shared;
    use serde_json::json;

    /// Every character this crate replaces, and why.
    ///
    /// Category Cc is C0, DEL and C1. Categories Zl and Zp are U+2028 and
    /// U+2029. The rest are the bidi controls: the marks, the embeddings, the
    /// overrides, the isolates and the two pops.
    fn deceptive_characters() -> Vec<char> {
        let mut set: Vec<char> = Vec::new();
        set.extend((0x00..=0x1f).filter_map(char::from_u32));
        set.push('\u{7f}');
        set.extend((0x80..=0x9f).filter_map(char::from_u32));
        set.push('\u{2028}');
        set.push('\u{2029}');
        set.extend(['\u{061c}', '\u{200e}', '\u{200f}']);
        set.extend((0x202a..=0x202e).filter_map(char::from_u32));
        set.extend((0x2066..=0x2069).filter_map(char::from_u32));
        set
    }

    #[test]
    fn the_two_agree_on_every_deceptive_character() {
        for character in deceptive_characters() {
            let input = format!("a{character}b");
            assert_eq!(
                Safe::name(&input).to_string(),
                Shared::name(&input).to_string(),
                "the two disagree on U+{:04X} in a name",
                character as u32
            );
            assert_eq!(
                Safe::message(&input).to_string(),
                Shared::message(&input).to_string(),
                "the two disagree on U+{:04X} in a message",
                character as u32
            );
        }
    }

    /// The deceptive set is only half the question. A character this crate
    /// keeps must also be kept by the shared one, or the swap silently damages
    /// every value the fleet logs.
    #[test]
    fn the_two_agree_on_every_character_below_the_astral_planes() {
        for code_point in 0x00..=0xffff {
            let Some(character) = char::from_u32(code_point) else {
                continue;
            };
            let input = format!("a{character}b");
            assert_eq!(
                Safe::name(&input).to_string(),
                Shared::name(&input).to_string(),
                "the two disagree on U+{code_point:04X}"
            );
        }
    }

    /// A four-byte character is where a byte cap and a character cap diverge.
    #[test]
    fn the_two_agree_on_the_astral_planes() {
        for code_point in [0x1f600, 0x1f468, 0x1f469, 0x10ffff, 0x10000] {
            let character = char::from_u32(code_point).expect("a valid scalar value");
            let input = format!("a{character}b");
            assert_eq!(
                Safe::name(&input).to_string(),
                Shared::name(&input).to_string(),
                "the two disagree on U+{code_point:04X}"
            );
        }
    }

    /// Both caps, on both sides of the boundary, including the two ways a cap
    /// can be overrun: a replacement that is wider than what it replaced, and
    /// a multi-byte character that straddles the limit.
    #[test]
    fn the_two_agree_on_every_cap_boundary() {
        let mut cases: Vec<String> = Vec::new();
        for length in [0, 1, 127, 128, 129, 1023, 1024, 1025, 4096] {
            cases.push("x".repeat(length));
            cases.push("\n".repeat(length));
            cases.push(format!("{}\u{1f600}", "x".repeat(length)));
            cases.push(format!("{}\u{202e}x", "x".repeat(length)));
        }

        for input in cases {
            assert_eq!(
                Safe::name(&input).to_string(),
                Shared::name(&input).to_string(),
                "the two disagree on a {}-byte name",
                input.len()
            );
            assert_eq!(
                Safe::message(&input).to_string(),
                Shared::message(&input).to_string(),
                "the two disagree on a {}-byte message",
                input.len()
            );
        }
    }

    /// The JSON wrapper renders the value to a string and caps that. The
    /// shared one takes the value itself and caps the stream its `Display`
    /// writes in pieces. The two must produce the same field.
    #[test]
    fn the_two_agree_on_a_json_value() {
        let cases = [
            json!({ "path": format!("/tmp/a{}b", '\u{202e}') }),
            json!({ "note": format!("x{}y", '\u{2028}'), "n": 1 }),
            json!({ "bulk": "b".repeat(64 * 1024) }),
            json!([1, "two", null, true]),
            json!(null),
            json!({}),
        ];

        for value in cases {
            assert_eq!(
                SafeJson(&value).to_string(),
                Shared::message(&value).to_string(),
                "the two disagree on the JSON value {value}"
            );
        }
    }

    /// The two caps must be the same numbers, not merely the same shape.
    #[test]
    fn the_two_agree_on_the_cap_sizes() {
        assert_eq!(super::MAX_NAME_BYTES, adelie_telemetry::MAX_NAME_BYTES);
        assert_eq!(
            super::MAX_MESSAGE_BYTES,
            adelie_telemetry::MAX_MESSAGE_BYTES
        );
        assert_eq!(super::REPLACEMENT, adelie_telemetry::REPLACEMENT);
        assert_eq!(super::TRUNCATED, adelie_telemetry::TRUNCATED);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{CallError, ToolDef, ToolReply};
    use async_trait::async_trait;

    struct Demo;

    #[async_trait]
    impl McpService for Demo {
        fn tools(&self) -> Vec<ToolDef> {
            vec![ToolDef::new("echo", "echo back", json!({"type": "object"}))]
        }
        async fn call_tool(&self, name: &str, args: &Value) -> Result<ToolReply, CallError> {
            match name {
                "echo" => Ok(ToolReply::text(args.to_string())),
                "boom" => Err(CallError::tool("kaboom")),
                "badargs" => Err(CallError::invalid_params(
                    "argument `path` must be a string, got number",
                )),
                "faulty" => Err(CallError::internal("connection pool exhausted")),
                _ => Err(CallError::tool(format!("unknown tool: {name}"))),
            }
        }
    }

    fn session() -> Session {
        let core = ServerCore::new(ServerConfig::new("demo", "0.0.0"), Arc::new(Demo));
        Session::new(core)
    }

    /// Drive `initialize` and return the negotiated version. `requested` of
    /// `None` omits the `protocolVersion` param entirely.
    async fn negotiated(requested: Option<&str>) -> String {
        let mut s = session();
        let params = match requested {
            Some(v) => json!({ "protocolVersion": v }),
            None => json!({}),
        };
        let d = s
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params
            }))
            .await;
        d.response.expect("initialize must produce a response")["result"]["protocolVersion"]
            .as_str()
            .expect("initialize must report a protocolVersion")
            .to_string()
    }

    #[tokio::test]
    async fn initialize_has_no_top_level_tools_key_and_negotiates() {
        let mut s = session();
        let d = s
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-11-25" }
            }))
            .await;
        let result = &d.response.unwrap()["result"];
        assert!(
            result.get("tools").is_none(),
            "must not embed tools in initialize"
        );
        assert_eq!(result["protocolVersion"], "2025-11-25");
        assert_eq!(result["serverInfo"]["name"], "demo");
        assert!(s.initialized);
    }

    /// Drive `initialize` against `config` and return the `serverInfo` object.
    async fn server_info(config: ServerConfig, requested: &str) -> Value {
        let core = ServerCore::new(config, Arc::new(Demo));
        let mut s = Session::new(core);
        let d = s
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": requested }
            }))
            .await;
        d.response.expect("initialize must produce a response")["result"]["serverInfo"].clone()
    }

    /// A config declaring all three optional SEP-973 fields.
    fn described() -> ServerConfig {
        ServerConfig::new("demo", "0.0.0")
            .title("Demo Server")
            .description("Echoes text back for testing")
            .website_url("https://example.com/demo")
    }

    #[tokio::test]
    async fn server_info_omits_optional_metadata_when_unset() {
        let info = server_info(ServerConfig::new("demo", "0.0.0"), "2025-11-25").await;
        let keys: Vec<&String> = info
            .as_object()
            .expect("serverInfo must be an object")
            .keys()
            .collect();
        assert_eq!(
            keys.len(),
            2,
            "an unset optional key must be absent, not null: {info}"
        );
        for absent in ["title", "description", "websiteUrl"] {
            assert!(info.get(absent).is_none(), "{absent} must not be emitted");
        }
    }

    #[tokio::test]
    async fn server_info_includes_metadata_when_set() {
        let info = server_info(described(), "2025-11-25").await;
        assert_eq!(info["title"], "Demo Server");
        assert_eq!(info["description"], "Echoes text back for testing");
        assert_eq!(
            info["websiteUrl"], "https://example.com/demo",
            "the spec spells this camelCase"
        );
    }

    /// The version gate: these fields arrived in 2025-11-25, so an older session
    /// must see the pre-2025-11-25 `Implementation` shape.
    #[tokio::test]
    async fn server_info_metadata_withheld_on_pre_2025_11_25_sessions() {
        let info = server_info(described(), "2025-06-18").await;
        for absent in ["title", "description", "websiteUrl"] {
            assert!(
                info.get(absent).is_none(),
                "{absent} must be withheld from a 2025-06-18 session: {info}"
            );
        }
    }

    #[tokio::test]
    async fn server_info_metadata_partially_set_omits_only_the_unset_keys() {
        let config = ServerConfig::new("demo", "0.0.0").description("Just a description");
        let info = server_info(config, "2025-11-25").await;
        assert_eq!(info["description"], "Just a description");
        assert!(info.get("title").is_none());
        assert!(info.get("websiteUrl").is_none());
    }

    #[tokio::test]
    async fn server_info_always_carries_name_and_version() {
        for (config, requested) in [
            (ServerConfig::new("demo", "0.0.0"), "2025-11-25"),
            (described(), "2025-11-25"),
            (described(), "2025-06-18"),
        ] {
            let info = server_info(config, requested).await;
            assert_eq!(info["name"], "demo");
            assert_eq!(info["version"], "0.0.0");
        }
    }

    #[tokio::test]
    async fn negotiates_current_revision_when_requested() {
        assert_eq!(negotiated(Some("2025-11-25")).await, "2025-11-25");
    }

    /// A client on the previous revision is not forced up.
    #[tokio::test]
    async fn negotiates_previous_revision_when_requested() {
        assert_eq!(negotiated(Some("2025-06-18")).await, "2025-06-18");
    }

    #[tokio::test]
    async fn unknown_version_falls_back_to_current_revision() {
        assert_eq!(negotiated(Some("1999-01-01")).await, "2025-11-25");
    }

    /// Retiring a version is a correct negotiation outcome, not a hard break:
    /// the client gets a supported version back rather than an error or an echo
    /// of something we no longer speak.
    #[tokio::test]
    async fn retired_versions_are_not_echoed_back() {
        for retired in ["2024-11-05", "2025-03-26"] {
            assert_eq!(
                negotiated(Some(retired)).await,
                "2025-11-25",
                "{retired} is retired and must not be echoed back"
            );
        }
    }

    #[tokio::test]
    async fn missing_protocol_version_defaults_to_current_revision() {
        assert_eq!(negotiated(None).await, "2025-11-25");
    }

    #[tokio::test]
    async fn tool_failure_is_iserror_content_not_jsonrpc_error() {
        let mut s = session();
        s.initialized = true;
        let d = s
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "boom", "arguments": {} }
            }))
            .await;
        let resp = d.response.unwrap();
        assert!(resp.get("error").is_none());
        assert_eq!(resp["result"]["isError"], true);
        assert_eq!(resp["result"]["content"][0]["text"], "kaboom");
    }

    /// SEP-1303: bad tool arguments are a *tool* failure the model can read and
    /// correct, not a protocol error that kills the turn.
    #[tokio::test]
    async fn invalid_tool_arguments_return_tool_error_not_protocol_error() {
        let mut s = session();
        s.initialized = true;
        let d = s
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 10, "method": "tools/call",
                "params": { "name": "badargs", "arguments": { "path": 7 } }
            }))
            .await;
        let resp = d.response.expect("tools/call must produce a response");
        assert!(
            resp.get("error").is_none(),
            "validation failure must not be a JSON-RPC error: {resp}"
        );
        assert_eq!(resp["result"]["isError"], true);
    }

    #[tokio::test]
    async fn invalid_tool_arguments_message_reaches_the_model() {
        let mut s = session();
        s.initialized = true;
        let d = s
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 11, "method": "tools/call",
                "params": { "name": "badargs", "arguments": { "path": 7 } }
            }))
            .await;
        let resp = d.response.expect("tools/call must produce a response");
        assert_eq!(
            resp["result"]["content"][0]["text"], "argument `path` must be a string, got number",
            "the model needs the validation detail to correct against"
        );
    }

    /// The boundary that must not move: a `tools/call` with no `name` is a
    /// malformed JSON-RPC request, not bad tool input.
    #[tokio::test]
    async fn missing_tool_name_is_still_a_protocol_error() {
        let mut s = session();
        s.initialized = true;
        let d = s
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {}
            }))
            .await;
        let resp = d.response.expect("tools/call must produce a response");
        assert_eq!(resp["error"]["code"], code::INVALID_PARAMS);
        assert!(resp.get("result").is_none());
    }

    #[tokio::test]
    async fn internal_tool_failure_is_still_a_protocol_error() {
        let mut s = session();
        s.initialized = true;
        let d = s
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 12, "method": "tools/call",
                "params": { "name": "faulty", "arguments": {} }
            }))
            .await;
        let resp = d.response.expect("tools/call must produce a response");
        assert_eq!(
            resp["error"]["code"],
            code::INTERNAL_ERROR,
            "a server fault is not something the model can correct"
        );
        assert!(resp.get("result").is_none());
    }

    /// The model must see one consistent failure mode, so a `Tool` failure and
    /// an `InvalidParams` failure differ only in their message.
    #[tokio::test]
    async fn tool_error_and_invalid_params_are_indistinguishable_on_the_wire() {
        async fn call(tool: &str) -> Value {
            let mut s = session();
            s.initialized = true;
            let d = s
                .handle_message(json!({
                    "jsonrpc": "2.0", "id": 13, "method": "tools/call",
                    "params": { "name": tool, "arguments": {} }
                }))
                .await;
            d.response.expect("tools/call must produce a response")["result"].clone()
        }

        let tool_failure = call("boom").await;
        let bad_arguments = call("badargs").await;

        let shape = |v: &Value| {
            let mut keys: Vec<String> = v
                .as_object()
                .expect("result must be an object")
                .keys()
                .cloned()
                .collect();
            keys.sort();
            keys
        };
        assert_eq!(shape(&tool_failure), shape(&bad_arguments));
        assert_eq!(tool_failure["isError"], bad_arguments["isError"]);
        assert_eq!(
            tool_failure["content"][0]["type"],
            bad_arguments["content"][0]["type"]
        );
        assert_ne!(
            tool_failure["content"][0]["text"], bad_arguments["content"][0]["text"],
            "same shape, different message"
        );
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let mut s = session();
        let d = s
            .handle_message(json!({"jsonrpc": "2.0", "id": 4, "method": "nope"}))
            .await;
        assert_eq!(d.response.unwrap()["error"]["code"], code::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn notification_gets_no_response() {
        let mut s = session();
        // No `id` => notification => never a response, even for unknown method.
        let d = s
            .handle_message(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        assert!(d.response.is_none());
        assert!(s.initialized);

        let d2 = s
            .handle_message(json!({"jsonrpc": "2.0", "method": "some/unknown"}))
            .await;
        assert!(d2.response.is_none());
    }

    #[tokio::test]
    async fn batch_array_is_invalid_request_not_silently_dropped() {
        // MC-5: a JSON-RPC batch (array) payload must get an INVALID_REQUEST
        // response (null id), not be silently treated as a notification. Since
        // 2024-11-05 and 2025-03-26 were retired this is unconditionally
        // correct — no version we negotiate defines batching.
        let mut s = session();
        let d = s
            .handle_message(json!([
                {"jsonrpc": "2.0", "id": 1, "method": "ping"},
                {"jsonrpc": "2.0", "id": 2, "method": "ping"}
            ]))
            .await;
        let resp = d.response.expect("batch array must produce a response");
        assert_eq!(resp["error"]["code"], code::INVALID_REQUEST);
        assert_eq!(resp["id"], Value::Null);
    }

    #[tokio::test]
    async fn non_object_payload_is_invalid_request() {
        // A bare scalar (not an object/array) is also not a valid Request.
        let mut s = session();
        let d = s.handle_message(json!("hello")).await;
        let resp = d.response.expect("scalar must produce a response");
        assert_eq!(resp["error"]["code"], code::INVALID_REQUEST);
        assert_eq!(resp["id"], Value::Null);
    }

    #[tokio::test]
    async fn tools_list_requires_initialize() {
        let mut s = session();
        let d = s
            .handle_message(json!({"jsonrpc": "2.0", "id": 5, "method": "tools/list"}))
            .await;
        assert_eq!(d.response.unwrap()["error"]["code"], code::NOT_INITIALIZED);
    }
}
