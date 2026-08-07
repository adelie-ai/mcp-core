//! A minimal stdio MCP server, used by the acceptance tests to prove what
//! really reaches file descriptor 1.
//!
//! A capture writer proves what a layer was told to do. Only a real process
//! proves what arrived on stdout, and stdout is where the stdio transport
//! frames JSON-RPC, so the difference is the whole point of the check.
//!
//! Run it as `stdio_probe serve` and feed newline-delimited JSON-RPC on stdin.

use mcp_core::{CallError, McpService, ServerConfig, ToolDef, ToolReply};
use serde_json::{Value, json};

struct Probe;

#[mcp_core::async_trait]
impl McpService for Probe {
    fn tools(&self) -> Vec<ToolDef> {
        let schema = json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
        });
        vec![
            ToolDef::new("echo", "return the text it was given", schema.clone()),
            ToolDef::new("fail", "report an internal server fault", schema),
        ]
    }

    async fn call_tool(&self, name: &str, args: &Value) -> Result<ToolReply, CallError> {
        let text = args.get("text").and_then(Value::as_str).unwrap_or_default();
        match name {
            "echo" => Ok(ToolReply::text(text)),
            // The idiom a real server reaches for on an unexpected IO fault.
            // It quotes an argument back, which is what makes the level the
            // dispatcher reports it at worth proving.
            "fail" => Err(CallError::internal(format!(
                "failed to read {text}: permission denied"
            ))),
            other => Err(CallError::tool(format!("unknown tool: {other}"))),
        }
    }
}

#[tokio::main]
async fn main() -> mcp_core::Result<()> {
    let config = ServerConfig::new("stdio-probe", env!("CARGO_PKG_VERSION"));
    mcp_core::run_simple(config, || async { Ok(Probe) }).await
}
