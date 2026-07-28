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
    /// JSON Schema for the tool's `arguments` object, emitted verbatim.
    ///
    /// Expected to be **JSON Schema 2020-12**, declared by omitting `$schema`
    /// (the MCP default). See the crate docs for the keyword rules; the ones
    /// that bite are array-form `items`, boolean `exclusiveMinimum`,
    /// `definitions`, and a top-level combinator.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_tool() -> ToolDef {
        ToolDef::new(
            "echo",
            "echo the input",
            json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            }),
        )
    }

    /// SEP-1613: 2020-12 is the default dialect, declared by *absence* of
    /// `$schema`. Emitting it would be an opt-out, so mcp-core must not add one.
    #[test]
    fn tool_input_schemas_declare_expected_dialect() {
        let wire = plain_tool().to_json();
        assert!(
            wire["inputSchema"].get("$schema").is_none(),
            "2020-12 is the MCP default; a `$schema` key opts *out* of it"
        );
    }

    /// The escape hatch has to keep working: a server that genuinely needs an
    /// older draft says so with `$schema`, and mcp-core must not strip it.
    #[test]
    fn explicit_schema_dialect_is_passed_through() {
        let draft07 = "http://json-schema.org/draft-07/schema#";
        let tool = ToolDef::new(
            "legacy",
            "declares an older draft",
            json!({ "$schema": draft07, "type": "object" }),
        );
        assert_eq!(tool.to_json()["inputSchema"]["$schema"], draft07);
    }

    /// mcp-core is dialect-neutral: the schema reaches the wire byte-identical.
    /// This is what lets the fleet absorb a dialect change without touching 13
    /// servers, per ADR 0001.
    #[test]
    fn tool_input_schema_reaches_the_wire_verbatim() {
        let schema = json!({
            "type": "object",
            "properties": {
                "tags": { "type": "array", "items": { "type": "string" } },
                "mode": { "enum": ["fast", "slow"] },
            },
            "additionalProperties": false,
        });
        let tool = ToolDef::new("t", "d", schema.clone());
        assert_eq!(tool.to_json()["inputSchema"], schema);
    }

    /// Guards the *core*, not the fleet: `ToolDef` takes an opaque `Value`, so
    /// this proves only that mcp-core introduces none of the keywords whose
    /// meaning differs between draft-07 and 2020-12. The fleet-wide audit lives
    /// in the PR for this change.
    #[test]
    fn tool_input_schemas_avoid_ambiguous_draft_keywords() {
        const AMBIGUOUS: &[&str] = &["definitions", "prefixItems", "$defs", "$ref"];
        let wire = plain_tool().to_json().to_string();
        for keyword in AMBIGUOUS {
            assert!(
                !wire.contains(keyword),
                "mcp-core must not introduce `{keyword}` into a tool definition"
            );
        }

        // `exclusiveMinimum` flipped from boolean (draft-04) to number (draft-06
        // onward). A server passing the boolean form is passed through
        // unchanged rather than silently "fixed" — quietly rewriting a server's
        // schema would be worse than emitting what it asked for.
        let legacy = json!({
            "type": "object",
            "properties": { "n": { "type": "number", "minimum": 0, "exclusiveMinimum": true } },
        });
        let tool = ToolDef::new("t", "d", legacy.clone());
        assert_eq!(tool.to_json()["inputSchema"], legacy);
    }

    /// The spec's recommended shape for a tool that takes no arguments.
    #[test]
    fn no_argument_tool_schema_round_trips() {
        let tool = ToolDef::new(
            "now",
            "current time",
            json!({ "type": "object", "additionalProperties": false }),
        );
        let wire = tool.to_json();
        assert_eq!(wire["inputSchema"]["type"], "object");
        assert_eq!(wire["inputSchema"]["additionalProperties"], false);
    }

    #[test]
    fn annotations_are_omitted_when_unset() {
        assert!(
            plain_tool().to_json().get("annotations").is_none(),
            "an unset optional key must be absent, not null"
        );
    }
}
