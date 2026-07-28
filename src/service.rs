//! The per-server extension surface: the [`McpService`] trait each server
//! implements, plus the value types it exchanges with the core.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};

/// The behaviour a concrete MCP server provides. The core ([`crate::Session`])
/// owns the JSON-RPC protocol, framing, and CLI; an implementor only describes
/// its tools and executes them.
#[async_trait]
pub trait McpService: Send + Sync + 'static {
    /// The tools advertised via `tools/list`.
    fn tools(&self) -> Vec<ToolDef>;

    /// Execute a tool call.
    ///
    /// Return [`ToolReply`] on success. Every failure the model could plausibly
    /// correct — bad arguments, unknown tool, upstream error — becomes
    /// `isError: true` content rather than a JSON-RPC protocol error, because
    /// the model never sees a protocol error: it surfaces as a failed call and
    /// takes the turn with it. Reserve [`CallError::Internal`] for faults the
    /// model cannot act on.
    ///
    /// Mapping guide:
    /// - missing/unparseable argument → [`CallError::InvalidParams`]
    /// - valid input but no result (e.g. "not found", upstream `429`/`5xx`) →
    ///   [`CallError::Tool`]
    /// - unknown tool name → [`CallError::Tool`]
    /// - bug / reply-serialize failure → [`CallError::Internal`] (`-32603`)
    ///
    /// The first three all reach the client as `isError` content; the variant
    /// records *why* at the call site. Make the message name the offending
    /// field and what was expected — it is the only thing the model has to
    /// correct against.
    ///
    /// `serde_json::Error` converts into [`CallError::Internal`], so
    /// `ToolReply::json(&value)?` can be used directly in this method. If you
    /// deserialize *arguments* with `?`, that lands in `Internal` too — which
    /// is wrong for model-supplied input; map it to
    /// [`CallError::invalid_params`] explicitly.
    async fn call_tool(&self, name: &str, arguments: &Value) -> Result<ToolReply, CallError>;

    /// Optional shutdown hook.
    ///
    /// Note: `shutdown` is **not** part of the MCP spec (it's borrowed from
    /// LSP). mcp-core accepts a JSON-RPC `shutdown` request as a convenience
    /// extension and calls this hook; standard MCP clients close the transport
    /// instead and never send it. Don't rely on it for correctness — treat it
    /// as a best-effort cleanup signal.
    async fn shutdown(&self) {}
}

/// A tool definition as advertised in `tools/list`.
#[derive(Clone, Debug)]
pub struct ToolDef {
    /// Unique tool name.
    pub name: String,
    /// Human/model-facing description.
    pub description: String,
    /// JSON Schema for the tool's `arguments` object.
    pub input_schema: Value,
    /// Optional MCP tool annotations (`readOnlyHint`, `title`, …).
    pub annotations: Option<Value>,
}

impl ToolDef {
    /// Build a tool definition. `input_schema` should be a JSON Schema object.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            annotations: None,
        }
    }

    /// Attach MCP tool annotations.
    pub fn with_annotations(mut self, annotations: Value) -> Self {
        self.annotations = Some(annotations);
        self
    }

    /// Serialize to the `tools/list` wire shape.
    pub(crate) fn to_json(&self) -> Value {
        let mut v = json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
        });
        if let Some(ann) = &self.annotations {
            v["annotations"] = ann.clone();
        }
        v
    }
}

/// A single content block in a tool result. MCP defines `text`, `image`, and
/// `resource`; this core supports `text` (the common case) and a raw escape
/// hatch for anything else.
#[derive(Clone, Debug)]
pub enum Content {
    /// A `{"type":"text","text":...}` block.
    Text(String),
    /// A pre-built content object, passed through verbatim.
    Raw(Value),
}

impl Content {
    /// A text content block.
    pub fn text(s: impl Into<String>) -> Self {
        Content::Text(s.into())
    }

    pub(crate) fn to_json(&self) -> Value {
        match self {
            Content::Text(t) => json!({ "type": "text", "text": t }),
            Content::Raw(v) => v.clone(),
        }
    }
}

/// The successful result of a tool call.
#[derive(Clone, Debug)]
pub struct ToolReply {
    /// Content blocks returned to the client.
    pub content: Vec<Content>,
    /// Whether this represents a tool-level error (`isError: true`).
    pub is_error: bool,
    /// Optional machine-readable `structuredContent` (2025 spec).
    pub structured_content: Option<Value>,
    /// If true, the core emits `notifications/tools/list_changed` after this
    /// call (for servers whose tool set changes at runtime).
    pub tools_list_changed: bool,
}

impl ToolReply {
    /// A successful text reply.
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(s)],
            is_error: false,
            structured_content: None,
            tools_list_changed: false,
        }
    }

    /// A successful reply carrying a JSON value: it is pretty-printed into a
    /// text block *and* attached as `structuredContent` so both plain and
    /// structured clients get it.
    pub fn json<T: Serialize>(value: &T) -> Result<Self, serde_json::Error> {
        let v = serde_json::to_value(value)?;
        let text = serde_json::to_string_pretty(&v)?;
        Ok(Self {
            content: vec![Content::text(text)],
            is_error: false,
            structured_content: Some(v),
            tools_list_changed: false,
        })
    }

    /// A tool-level error reply (`isError: true`) carrying a message.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(message)],
            is_error: true,
            structured_content: None,
            tools_list_changed: false,
        }
    }

    /// Replace the content blocks.
    pub fn with_content(mut self, content: Vec<Content>) -> Self {
        self.content = content;
        self
    }

    /// Attach explicit `structuredContent`.
    pub fn with_structured(mut self, value: Value) -> Self {
        self.structured_content = Some(value);
        self
    }

    /// Request that the core emit a `tools/list_changed` notification.
    pub fn tools_changed(mut self) -> Self {
        self.tools_list_changed = true;
        self
    }

    pub(crate) fn to_result_json(&self) -> Value {
        let mut v = json!({
            "content": self.content.iter().map(Content::to_json).collect::<Vec<_>>(),
            "isError": self.is_error,
        });
        if let Some(sc) = &self.structured_content {
            v["structuredContent"] = sc.clone();
        }
        v
    }
}

/// Why a tool call failed.
///
/// The split that matters is *can the model do anything about it*. `Tool` and
/// `InvalidParams` both reach the client as `isError: true` content the model
/// can read and retry against; only `Internal` is a JSON-RPC protocol error.
#[derive(Debug)]
pub enum CallError {
    /// A tool-execution failure — surfaced as `isError: true` content (a
    /// successful JSON-RPC response). The right variant for almost all
    /// failures, including "unknown tool".
    Tool(String),
    /// The model supplied arguments the tool could not use.
    ///
    /// Also `isError` content, and on the wire indistinguishable from
    /// [`CallError::Tool`] — per SEP-1303, argument validation is a
    /// tool-execution error so the model can self-correct. The variant is kept
    /// because it records the cause at the call site, and because the wire
    /// treatment is a property of the negotiated protocol dialect rather than
    /// of the error itself.
    InvalidParams(String),
    /// An internal server fault — JSON-RPC `-32603`. Not something the model
    /// supplied or can fix, so it stays a protocol error.
    Internal(String),
}

impl CallError {
    /// A tool-level error (becomes `isError` content).
    pub fn tool(message: impl Into<String>) -> Self {
        CallError::Tool(message.into())
    }
    /// Bad model-supplied arguments (becomes `isError` content). Name the field
    /// and what was expected.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        CallError::InvalidParams(message.into())
    }
    /// Internal error (becomes JSON-RPC `-32603`).
    pub fn internal(message: impl Into<String>) -> Self {
        CallError::Internal(message.into())
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Tool(m) | CallError::InvalidParams(m) | CallError::Internal(m) => {
                f.write_str(m)
            }
        }
    }
}

impl std::error::Error for CallError {}

impl From<serde_json::Error> for CallError {
    /// A (de)serialization failure is an internal fault.
    fn from(e: serde_json::Error) -> Self {
        CallError::Internal(e.to_string())
    }
}
