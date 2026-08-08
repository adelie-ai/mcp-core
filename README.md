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
- **Process lifecycle.** The stop signals `run` listens for, so a server that
  Kubernetes or a terminal stops flushes its telemetry before it exits.

### What it refuses

- Any single server's domain logic. A server implements `McpService` and owns
  its own tools, its own schemas and its own configuration.
- Deciding a tool's schema dialect. Schemas reach the wire verbatim; the
  crate never injects, strips or rewrites a key.
- Installing a subscriber or a signal handler anywhere except `run`. A server
  library hosted inside another binary inherits that binary's subscriber and
  that binary's signal handling.

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

### Continuing the caller's trace

A caller that already has a trace puts its W3C trace context in the request, as
`params._meta.traceparent`. The MCP specification reserves `_meta` for a field
like this one. The server reads it and continues that trace, so one trace
covers the whole turn: the client, the daemon, and every server the turn
reached. A server built on this crate does nothing to get it.

It works with `otel` off. The trace id goes on the `mcp.request` span as
`trace_id`, 32 hexadecimal characters, and the caller writes the same id on its
own lines. One search then finds the request in both logs, and neither process
exports anything.

```text
mcp.request{method=tools/call request_id=3 transport=stdio trace_id=4bf92f3577b34da6a3ce929d0e0e4736}
```

With `otel` on, the request span also takes the caller's span as its parent. The
spans this server exports then join the caller's trace instead of starting one
of their own.

A caller chooses the value, so a bad one costs the request nothing. A
`traceparent` that is absent, is not a string, is too long, or is malformed
leaves the server to start its own trace, and the reason appears at DEBUG. A
request that carried no usable value carries no `trace_id` field either, because
an empty one reads as a real trace.

Nothing else in `_meta` is read, and nothing else in it reaches a log line.

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

A collector that cannot be reached costs the process its export, and while it is
running it costs nothing else. Console logging and the metrics summary continue,
and the reason is written at ERROR.

It costs one thing at the end. A stop waits for the flush to give up, which is
the telemetry guard's shutdown budget, five seconds by default. Measured against
a collector whose packets are dropped rather than refused: the process wrote its
metrics summary at once, warned that the pipelines had not shut down inside the
budget, and exited 0 after 5.1 seconds. A refused connection fails immediately
and costs nothing at all.

### When the exporters flush

The OTLP exporters buffer, and they flush when `run` stops. `run` stops for two
reasons, and both flush.

The **client ends the session**. Over stdio it closes the stream, over unix and
websocket it closes the connection. The buffer goes out, `run` returns whatever
the serve loop returned, and the process exits normally.

The **process is asked to stop**, with `SIGTERM` or `SIGINT`. `run` ends the
serve loop, flushes, and ends the process with status 0 without returning. This
works over all three transports, and it is what makes a rolling deployment keep
the window it was in: Kubernetes stops every pod with `SIGTERM`.

Console output and the periodic metrics summary are unaffected either way,
because both are written as they happen rather than buffered.

## Stopping a server

`run` listens for `SIGTERM` and `SIGINT`, and treats them identically.
`SIGTERM` is what Kubernetes, systemd and `kill` send. `SIGINT` is what a
terminal sends on Ctrl-C.

`SIGHUP` is deliberately not handled. There is no reload for it to mean, because
a server's configuration is fixed by its command line. And a handler would
replace an inherited `SIG_IGN`, so a server started under `nohup` would start
dying with its terminal instead of surviving it.

| Question | Answer |
|---|---|
| In-flight requests | Cut, not drained. The process is going away inside the grace period, and a client already handles a transport that closes under it. |
| `McpService::shutdown` | Not called. It is a de-initialize hook driven by a client `shutdown` request, after which the session keeps serving. |
| Destructors | None run, including any `Drop` on the service. The telemetry guard is flushed by hand for this reason. |
| The unix socket file | Left behind. A killed process left one too, and the next bind unlinks a stale socket. |
| Exit status | `0`. Kubernetes reports the container as Completed, and systemd counts exit 0 as success. It would also read as success for a Job or `restartPolicy: OnFailure`; no server here runs that way. |
| `SIGINT` | The same as `SIGTERM`, in every respect. |
| Where the handler lives | `run`, and nowhere else. `serve` is a plain library call and installs nothing. |

Installing does replace an inherited `SIG_IGN` for both signals. A shell without
job control ignores `SIGINT` in a backgrounded child, so `sh -c 'some-mcp serve
&'` now stops on Ctrl-C where it used to survive. That is the cost of the ticket
asking for `SIGINT`, and it is why `SIGHUP` is refused: `nohup` exists to make a
server outlive its terminal, and handling `SIGHUP` would take that away for
nothing in return.

### How long a stop takes

The stop path adds no wait of its own, but it is not free. It waits for the
flush, and the flush is bounded by the telemetry guard's shutdown budget: five
seconds by default, and **not configurable** from a server that uses `run`. That
is well inside the 30-second Kubernetes default grace period.

- No collector configured, or a collector that answers: about ten milliseconds.
- A collector whose packets are dropped rather than refused: the whole budget.
  Measured at 5.1 seconds, with the metrics summary written at once and a warning
  that the pipelines had not shut down in time.
- A refused connection: no wait at all.

A second signal that arrives during the flush is absorbed. It neither shortens
the budget nor ends the process early, so a second Ctrl-C looks ignored for as
long as the flush takes.

### Why `run` ends the process

`run` does not return on the signal path. Nothing after `run(...).await` in a
`main` runs, and no destructor runs anywhere. `tokio::io::stdin` reads on a
blocking task, and dropping that read does not end it, so the runtime's own
shutdown would wait for a stdin read that the peer is still holding open, and a
stdio server would flush and then hang. `run` is the process entry point by
contract and already ends the process on `--help`, `--version` and a bad
argument, where clap exits inside `get_matches`. A server with cleanup of its own
to do drives `serve` instead.

A server that owns its own `main` and calls `serve` gets none of this, by
design, and wires it itself. `shutdown::flush_and_exit` carries the part whose
order is load bearing, so the server writes the install and the race only:

```rust
use mcp_core::{shutdown, telemetry};

let telemetry = telemetry::init(telemetry::Config::new("example-mcp"))?;

// Install before serving. A signal that arrives before this is still fatal.
let mut stop = shutdown::StopSignals::install()?;

let signal = tokio::select! {
    biased;
    result = mcp_core::serve(core, &args) => return result,
    signal = stop.recv() => signal,
};
shutdown::flush_and_exit(signal, telemetry)
```

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

# The websocket transport is off by default, so its tests are compiled out of
# the run above. `auth` implies it.
cargo clippy --all-targets --features auth -- -D warnings
cargo test --features auth
```

`Cargo.toml` denies warnings mechanically, so a plain `cargo build` fails on
one as well.

## License

Apache-2.0.
