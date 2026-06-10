//! Protocol core: per-connection [`Session`] dispatch and shared [`ServerCore`].

use std::sync::Arc;

use serde_json::{Value, json};

use crate::config::ServerConfig;
use crate::error::code;
use crate::service::{CallError, McpService};

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
    /// Start a fresh session bound to the shared core.
    pub fn new(core: Arc<ServerCore>) -> Self {
        Self {
            core,
            initialized: false,
        }
    }

    /// Handle one parsed JSON-RPC message and produce the response (if any) and
    /// any notifications to flush.
    pub async fn handle_message(&mut self, message: Value) -> Dispatch {
        // MC-5: a JSON-RPC payload must be a single Request/Notification object.
        // An array (batch) or any non-object scalar is not a valid Request — and
        // we don't support batching despite advertising protocol versions that
        // define it — so answer INVALID_REQUEST with a null id rather than
        // silently dropping it (an array has no `id`, so the old code treated it
        // as a notification and never replied, hanging the client).
        if !message.is_object() {
            let msg = if message.is_array() {
                "batch requests (JSON arrays) are not supported"
            } else {
                "request must be a JSON object"
            };
            return Dispatch {
                response: Some(error_response(
                    Some(Value::Null),
                    code::INVALID_REQUEST,
                    msg,
                )),
                notifications: Vec::new(),
            };
        }

        let id = message.get("id").cloned();
        // Per JSON-RPC, a message with no `id` member is a notification and
        // must never receive a response — not even an error.
        let is_request = message.get("id").is_some();

        if let Some(version) = message.get("jsonrpc").and_then(Value::as_str)
            && version != "2.0"
        {
            return Self::finish(
                is_request,
                id,
                Outcome::Error(
                    code::INVALID_REQUEST,
                    format!("invalid jsonrpc version: {version}"),
                ),
                Vec::new(),
            );
        }

        let method = message.get("method").and_then(Value::as_str);
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let mut notifications = Vec::new();

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
                Outcome::Error(code::METHOD_NOT_FOUND, format!("method not found: {other}"))
            }
            None => Outcome::Error(code::INVALID_REQUEST, "missing method".into()),
        };

        Self::finish(is_request, id, outcome, notifications)
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

        let mut result = json!({
            "protocolVersion": negotiated,
            "serverInfo": {
                "name": self.core.config.name,
                "version": self.core.config.version,
            },
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

        match self.core.service.call_tool(name, &arguments).await {
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
            Err(CallError::Tool(msg)) => Outcome::Result(tool_error_result(&msg)),
            Err(CallError::InvalidParams(msg)) => Outcome::Error(code::INVALID_PARAMS, msg),
            Err(CallError::Internal(msg)) => Outcome::Error(code::INTERNAL_ERROR, msg),
        }
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
                _ => Err(CallError::tool(format!("unknown tool: {name}"))),
            }
        }
    }

    fn session() -> Session {
        let core = ServerCore::new(ServerConfig::new("demo", "0.0.0"), Arc::new(Demo));
        Session::new(core)
    }

    #[tokio::test]
    async fn initialize_has_no_top_level_tools_key_and_negotiates() {
        let mut s = session();
        let d = s
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-03-26" }
            }))
            .await;
        let result = &d.response.unwrap()["result"];
        assert!(
            result.get("tools").is_none(),
            "must not embed tools in initialize"
        );
        assert_eq!(result["protocolVersion"], "2025-03-26");
        assert_eq!(result["serverInfo"]["name"], "demo");
        assert!(s.initialized);
    }

    #[tokio::test]
    async fn unknown_protocol_version_falls_back_to_latest() {
        let mut s = session();
        let d = s
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "1999-01-01" }
            }))
            .await;
        assert_eq!(
            d.response.unwrap()["result"]["protocolVersion"],
            "2025-06-18"
        );
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

    #[tokio::test]
    async fn missing_tool_name_is_invalid_params() {
        let mut s = session();
        s.initialized = true;
        let d = s
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {}
            }))
            .await;
        assert_eq!(d.response.unwrap()["error"]["code"], code::INVALID_PARAMS);
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
        // response (null id), not be silently treated as a notification — we
        // advertise protocol versions that define batching but don't support it.
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
