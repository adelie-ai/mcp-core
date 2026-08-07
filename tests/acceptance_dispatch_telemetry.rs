//! Acceptance criteria for the telemetry the dispatch path emits.
//!
//! Each test is named after the criterion it holds, so a failing run names the
//! unmet requirement rather than a line number.

mod support;

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use tracing::Level;

use support::capture_dispatch;

/// The argument value the level-contract tests hunt for. It has the shape of
/// the thing that must never leak: a file path a caller supplied.
const SECRET_ARGUMENT: &str = "/home/someone/private/notes-MARKER-9d3f.txt";

/// AC: `cargo tree` with default features shows no `opentelemetry*` crate.
///
/// The `otel` feature is the only thing that adds them. A stdio-only server
/// that never turns it on must not compile one, or every `cargo install` in
/// the fleet pays for an exporter it does not use.
#[test]
fn default_build_pulls_no_opentelemetry() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");

    let output = Command::new(cargo)
        .args(["tree", "--edges", "normal", "--prefix", "none", "--locked"])
        .arg("--manifest-path")
        .arg(manifest)
        .output()
        .expect("cargo tree must run");

    assert!(
        output.status.success(),
        "cargo tree failed, so this criterion is unproven: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let tree = String::from_utf8_lossy(&output.stdout);
    let found: Vec<&str> = tree
        .lines()
        .map(str::trim)
        .filter(|line| line.to_ascii_lowercase().starts_with("opentelemetry"))
        .collect();

    assert!(
        found.is_empty(),
        "a default-feature build must resolve no opentelemetry crate, but it resolved: {found:?}"
    );
}

/// AC: with `RUST_LOG=trace`, every line the stdio transport writes to stdout
/// parses as JSON-RPC, and the logs appear on stderr instead.
///
/// One log line on stdout corrupts the protocol stream, so this runs a real
/// process and reads the real descriptor.
#[test]
fn stdio_stdout_carries_only_jsonrpc() {
    let probe = probe_binary();
    assert!(
        probe.is_file(),
        "the stdio probe example must be built before this test can prove anything; \
         expected it at {}",
        probe.display()
    );

    let requests = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"text": SECRET_ARGUMENT}},
        }),
    ];

    let mut child = Command::new(&probe)
        .arg("serve")
        .env("RUST_LOG", "trace")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the probe must start");

    {
        let stdin = child.stdin.as_mut().expect("the probe has a piped stdin");
        for request in &requests {
            writeln!(stdin, "{request}").expect("the probe must accept its input");
        }
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("the probe must finish");
    assert!(
        output.status.success(),
        "the probe must exit cleanly, otherwise an empty stdout proves nothing: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");

    let mut replies = 0;
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let value: Value = serde_json::from_str(line).unwrap_or_else(|e| {
            panic!("every stdout line must be JSON-RPC, but {line:?} is not: {e}")
        });
        assert_eq!(
            value.get("jsonrpc").and_then(Value::as_str),
            Some("2.0"),
            "every stdout line must carry the JSON-RPC envelope: {line:?}"
        );
        replies += 1;
    }
    assert_eq!(replies, 3, "the probe must answer all three requests");

    assert!(
        stderr.contains("INFO") || stderr.contains("TRACE") || stderr.contains("DEBUG"),
        "at RUST_LOG=trace the logs must arrive on stderr, or the subscriber was never \
         installed. stderr was: {stderr:?}"
    );
}

/// AC: a `tools/call` opens a span carrying the tool name.
#[test]
fn tools_call_emits_a_span_with_the_tool_name() {
    let recorded = capture_dispatch(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"text": "hello"}},
        }),
    ]);

    let span = recorded
        .spans
        .iter()
        .find(|span| span.fields.get("tool").map(String::as_str) == Some("echo"))
        .unwrap_or_else(|| {
            panic!(
                "a tools/call must open a span carrying the tool name; the spans were {:?}",
                recorded.span_summary()
            )
        });

    assert!(
        span.name.contains("tools"),
        "the tool span must be named for the call it covers, not {:?}",
        span.name
    );
}

/// AC: no tool-argument value reaches a span field or an INFO line (D10).
///
/// The same run proves the other half of the contract: the arguments are
/// available at DEBUG, so turning the level up is what it takes to see them.
#[test]
fn tools_call_span_does_not_record_arguments() {
    let recorded = capture_dispatch(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"path": SECRET_ARGUMENT}},
        }),
    ]);

    for span in &recorded.spans {
        for (key, value) in &span.fields {
            assert!(
                !value.contains("MARKER-9d3f"),
                "a tool argument reached span {:?} field {key:?}: {value:?}",
                span.name
            );
        }
    }

    for event in &recorded.events {
        if event.level > Level::INFO {
            continue;
        }
        for (key, value) in &event.fields {
            assert!(
                !value.contains("MARKER-9d3f"),
                "a tool argument reached a {} line, field {key:?}: {value:?}",
                event.level
            );
        }
    }

    let at_debug = recorded.events.iter().any(|event| {
        event.level == Level::DEBUG
            && event
                .fields
                .values()
                .any(|value| value.contains("MARKER-9d3f"))
    });
    assert!(
        at_debug,
        "tool arguments must be available at DEBUG, or the level contract has nothing to \
         hold back; the events were {:?}",
        recorded.event_summary()
    );
}

/// A caller chooses the method name, the tool name and the request id. None of
/// them may end a log line, drive a terminal, or grow without a bound.
///
/// The console layer writes a field value straight into a line, so a newline in
/// one produces what reads as a second genuine line, with a real timestamp
/// column, level and target. `with_ansi(false)` turns off the formatter's own
/// colour, not an escape carried inside a value. With `otel` on the same value
/// leaves the process as a span attribute.
#[test]
fn caller_supplied_names_cannot_forge_a_log_line() {
    let forged = "x\n2026-08-07T00:00:00.000000Z  INFO mcp_core: authentication disabled\u{1b}[31m";
    let long = "l".repeat(4096);

    let recorded = capture_dispatch(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": forged, "arguments": {}},
        }),
        json!({"jsonrpc": "2.0", "id": forged, "method": forged, "params": {}}),
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {"name": long, "arguments": {}},
        }),
    ]);

    // No field anywhere may break a line. That covers the span fields and the
    // event fields alike, because both are written into the console stream.
    let everything = recorded
        .spans
        .iter()
        .flat_map(|span| {
            span.fields
                .iter()
                .map(move |(key, value)| (span.name, key, value))
        })
        .chain(recorded.events.iter().flat_map(|event| {
            event
                .fields
                .iter()
                .map(|(key, value)| ("<event>", key, value))
        }));

    for (owner, key, value) in everything {
        assert!(
            !value
                .chars()
                .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}'),
            "{owner} field {key:?} carries a character that can end a log line or drive a \
             terminal: {value:?}"
        );
    }

    // A span field is capped as well. It is exported verbatim with `otel` on,
    // and the transport cap is measured in megabytes, so an uncapped one lets
    // one request ship as much as it likes. The DEBUG arguments line is
    // deliberately not capped: an operator who raises the level wants the whole
    // value.
    for span in &recorded.spans {
        for (key, value) in &span.fields {
            assert!(
                value.len() <= 256,
                "span {:?} field {key:?} is unbounded at {} bytes; a caller sets it",
                span.name,
                value.len()
            );
        }
    }
}

/// AC: no `eprintln!` remains in `src/runner.rs`; its diagnostics carry
/// structured fields instead.
#[test]
fn runner_diagnostics_use_structured_fields() {
    const RUNNER: &str = include_str!("../src/runner.rs");

    assert!(
        !RUNNER.contains("eprintln!"),
        "src/runner.rs still reports through eprintln!, which has no level and no span \
         context"
    );
    assert!(
        !RUNNER.contains("print!("),
        "src/runner.rs must never write to stdout: the stdio transport frames JSON-RPC there"
    );
    assert!(
        RUNNER.contains("tracing::info!") && RUNNER.contains("tracing::error!"),
        "src/runner.rs must report lifecycle at INFO and failures at ERROR"
    );
    assert!(
        RUNNER.contains("error = %"),
        "a failure must carry the cause as a structured field, not an interpolated string"
    );
    assert!(
        !RUNNER.contains("connection error: {"),
        "a diagnostic must not interpolate its cause into the message text"
    );
}

/// AC: a span per JSON-RPC request, carrying the method and the request id.
#[test]
fn request_span_carries_method_and_request_id() {
    let recorded = capture_dispatch(&[json!({
        "jsonrpc": "2.0",
        "id": 41,
        "method": "initialize",
        "params": {},
    })]);

    let span = recorded
        .spans
        .iter()
        .find(|span| span.fields.get("method").map(String::as_str) == Some("initialize"))
        .unwrap_or_else(|| {
            panic!(
                "every request must open a span carrying its method; the spans were {:?}",
                recorded.span_summary()
            )
        });

    let request_id = span
        .fields
        .get("request_id")
        .expect("the request span must carry the request id, so one line finds the whole request");
    assert!(
        request_id.contains("41"),
        "the request span must carry the id the caller sent, not {request_id:?}"
    );
}

/// Where `cargo test` leaves the example binaries. The test binary sits in
/// `target/<profile>/deps/`, so the examples are one directory across.
fn probe_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("a test binary knows its own path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("examples");
    path.push("stdio_probe");
    path
}
