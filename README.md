# mcp-core

Shared core for the adelie-ai hand-rolled MCP servers.

## Purpose

Every `*-mcp` server in the fleet speaks the same protocol over the same
transports and takes the same CLI. Before this crate each one carried its own
copy of that code, and the copies drifted. This crate holds it once, so a spec
revision or a protocol fix costs one pin bump instead of thirteen rewrites.

### What it owns

- **Protocol.** JSON-RPC 2.0 dispatch with the correct error codes, protocol
  version negotiation, and spec-compliant `tools/call` results.
- **Transports.** stdio and unix, framed and size-capped, plus an optional
  websocket transport with optional bearer-token authentication.
- **CLI.** A standard Clap `serve` command that a server extends with its own
  flags.
- **Telemetry.** The process subscriber, installed by `run`, and the spans and
  metrics over the dispatch path.

### What it refuses

- Any single server's domain logic. A server implements `McpService` and owns
  its own tools, its own schemas and its own configuration.
- Deciding a tool's schema dialect. Schemas reach the wire verbatim; the
  crate never injects, strips or rewrites a key.
- Installing a subscriber anywhere except `run`. A server library hosted
  inside another binary inherits that binary's subscriber.

## Use

```toml
[dependencies]
mcp-core = { git = "https://github.com/adelie-ai/mcp-core", rev = "..." }
```

```rust
#[tokio::main]
async fn main() -> mcp_core::Result<()> {
    let config = mcp_core::ServerConfig::new("echo-mcp", env!("CARGO_PKG_VERSION"));
    mcp_core::run_simple(config, || async { Ok(Echo) }).await
}
```

`run` parses the CLI, installs telemetry, builds the service and serves. The
crate documentation carries the full example, including the `McpService`
implementation and the tool schema conventions.

### Features

| Feature | Default | Effect |
|---|---|---|
| `unix` | yes | The unix-domain-socket transport. Gates code only, no extra crate. |
| `websocket` | no | The websocket transport. Pulls in axum. |
| `auth` | no | Bearer-token validation on websocket connections. Implies `websocket`. |
| `otel` | no | OTLP export of traces, metrics and log records. |

stdio needs no feature.

## Logging

### Where it goes

**stderr, always.** Never stdout: the stdio transport frames JSON-RPC there,
and one log line in that stream corrupts the protocol. This holds at every
level, including `RUST_LOG=trace`.

### How much of it

`RUST_LOG` sets the filter. Unset means `info`.

```sh
RUST_LOG=debug some-mcp serve
RUST_LOG=info,mcp_core=debug some-mcp serve
```

One filter governs the console and the OTLP log exporter together.

### What may appear at each level

This contract is a rule, not a preference.

| Level | Carries |
|---|---|
| INFO | ids, counts, durations, method names, tool names. **Never content.** |
| DEBUG | tool arguments, the reason a tool declined, and the detail of a failure. |

Tool arguments carry file paths, command lines and search text, so they never
reach a span field or an INFO line. `RUST_LOG=debug` is what it takes to see
them, and with a collector configured that means they leave the process.

A failed tool call writes `tool call failed` at ERROR, so an operator sees the
fault at the default level, and the surrounding span names the tool, the method,
the request id and the transport. The message inside the `CallError` goes to
DEBUG instead, because a server writing `failed to read {path}` puts a caller's
argument in it, and that value has an audience of one until something logs it.

Every value a caller reaches is capped and stripped before it reaches a field,
the arguments payload included. Left alone, a newline in a tool name produces
what reads as a second genuine log line, an ANSI escape survives into whatever
is reading the log, a line separator breaks a record for a JSON consumer, and a
bidi control reverses what a person sees while the bytes stay honest.

That covers what the dispatch path logs. A server that logs a caller's value
itself reaches the same wrapper rather than writing one of its own, because a
second copy drifts and the caps then differ between two binaries reading the
same value:

```rust
use mcp_core::telemetry::Safe;

tracing::info!(tool = %Safe::name(tool_name), "tool call finished");
tracing::debug!(reason = %Safe::message(detail), "tool returned an error");
```

`Safe::name` caps at 128 bytes, `Safe::message` at 1024, and both replace what
would deceive with U+FFFD. It wraps anything that can be displayed, so an error
or a JSON value goes through it without being rendered to a string first.

### What the dispatch path emits

One span per JSON-RPC request, and a child span per tool call:

```text
mcp.request{method=tools/call request_id=3 transport=stdio}:mcp.tools.call{tool=echo}
```

Four instruments, recorded whether or not a collector is configured:

| Metric | Labels | Meaning |
|---|---|---|
| `mcp.requests` | `method` | Requests handled. |
| `mcp.request.duration` | `method` | How long each request took. |
| `mcp.tools.call` | `tool`, `outcome` | Tool calls, by result. |
| `mcp.tools.call.duration` | `tool` | How long each tool call took. |

`outcome` is `ok`, `tool_error` for a tool that declined, or `error` for a
server fault. A method the server does not implement counts as `other`,
because the caller chooses that name and the label set must stay bounded.

With no collector these accumulate in process and are written to stderr as a
periodic summary, and again on the way out. A default build gets real numbers
in the journal.

A server records its own metrics through the re-exported facade, and never
through an opentelemetry meter directly:

```rust
use mcp_core::telemetry::metrics::{self, Label};

metrics::increment("weather.lookups", &[Label::new("provider", "example")]);
```

### Exporting to a collector

Off by default. A server adds the passthrough to its own manifest, which
reaches this crate and then `adelie-telemetry`:

```toml
[features]
otel = ["mcp-core/otel"]
```

With the feature off, no opentelemetry crate is resolved at all, so a
stdio-only server pays nothing for it. With it on, the OTLP layers are added
beside the console layer rather than in place of it, so an exporting build
still prints locally.

Everything else comes from the standard `OTEL_*` environment variables. There
are no CLI flags and no Adelie-specific variables.

| Variable | Effect |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Endpoint for all three signals. |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | Endpoint for traces. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | Endpoint for metrics. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | Endpoint for log records. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc`, `http/protobuf` or `http/json`, for all three. |
| `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` | Protocol for traces. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL` | Protocol for metrics. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_LOGS_PROTOCOL` | Protocol for log records. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_HEADERS` | Headers for all three, as `key=value,key=value`. |
| `OTEL_EXPORTER_OTLP_TRACES_HEADERS` | Headers for traces. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_METRICS_HEADERS` | Headers for metrics. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_LOGS_HEADERS` | Headers for log records. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_TIMEOUT` | Export timeout in milliseconds, for all three. |
| `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` | Timeout for traces. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT` | Timeout for metrics. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_LOGS_TIMEOUT` | Timeout for log records. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_COMPRESSION` | `gzip` or `zstd`, for all three. Per-signal forms exist too. |
| `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE` | Metric temporality. |
| `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes, as `key=value,key=value`. |

A per-signal variable beats the generic one. The generic endpoint has the
signal's path appended to it (`/v1/traces` and so on); a per-signal endpoint is
used exactly as written, so it must include the path.

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://collector.example.com:4318 \
OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf \
  some-mcp serve
```

An `otel` build always builds the three pipelines. With no endpoint set they
take the OTLP default of `http://localhost:4318`, so the build that exports
nothing is a default build, not an `otel` build with the variables left unset.

A collector that cannot be reached costs the process its export and nothing
else. Console logging and the metrics summary continue, and the reason is
written at ERROR.

### When the exporters flush

The OTLP exporters buffer, and they flush when `run` returns.

Over **stdio** that happens on its own: the client closes the stream, `run`
returns, and the buffer goes out.

Over **unix** and **websocket** the server has no exit of its own. The accept
loop runs until something kills the process, and a process killed by `SIGTERM`
or `SIGINT` runs no destructor, so whatever the exporters still held is lost.
Nothing here catches either signal today.

Console output and the periodic metrics summary are unaffected either way,
because both are written as they happen rather than buffered. An operator who
needs the exported copy of the last few seconds should read the console log for
that window instead.

Which transport to choose, what each one trusts for TLS, and what a container
image needs are covered by
[adelie-telemetry](https://github.com/adelie-ai/adelie-telemetry).

## Development

The gate is local; this repo has no CI. Run it in both configurations before
you push.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test

cargo clippy --all-targets --features otel -- -D warnings
cargo test --features otel
```

`Cargo.toml` denies warnings mechanically, so a plain `cargo build` fails on
one as well.

## License

Apache-2.0.
