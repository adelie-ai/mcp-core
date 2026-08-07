//! Acceptance criteria for the surface mcp-core offers its servers, and for
//! the metrics the dispatch path records.
//!
//! Thirteen MCP servers reach telemetry through mcp-core. What this file
//! asserts is the contract they compile against.

mod support;

use std::sync::Arc;

use mcp_core::telemetry::metrics::{self, Label};
use mcp_core::{ServerCore, Session, TransportKind};
use serde_json::{Value, json};

use support::{capture, demo_core};

/// AC: a server records metrics through the re-exported facade, so it takes no
/// direct dependency on adelie-telemetry or on opentelemetry.
///
/// Reaching for `opentelemetry::global::meter()` instead would make every
/// crate that records a metric depend on opentelemetry whether or not the
/// `otel` feature is on, which is what epic AC2 forbids.
#[test]
fn metrics_facade_is_reexported_for_servers() {
    let labels = [Label::new("tool", "facade_probe")];
    let before = counter_total("mcp.tools.call", &labels);

    metrics::increment("mcp.tools.call", &labels);
    metrics::add("mcp.tools.call", 2, &labels);
    metrics::record_duration(
        "mcp.request.duration",
        std::time::Duration::from_millis(5),
        &labels,
    );

    assert_eq!(
        counter_total("mcp.tools.call", &labels),
        before + 3,
        "the re-exported facade must record into the same registry the crate uses"
    );
}

/// AC: with default features, a `tools/call` increments the counter in the
/// in-process registry. No collector, no `otel` feature, real numbers.
#[test]
fn metrics_recorded_without_otel() {
    let tool = "metrics_probe";
    let call_labels = [Label::new("tool", tool), Label::new("outcome", "ok")];
    let request_labels = [Label::new("method", "tools/call")];

    let calls_before = counter_total("mcp.tools.call", &call_labels);
    let requests_before = counter_total("mcp.requests", &request_labels);

    drive(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": tool, "arguments": {"text": "hello"}},
        }),
    ]);

    assert_eq!(
        counter_total("mcp.tools.call", &call_labels),
        calls_before + 1,
        "a tools/call must increment the tool counter, labelled by tool and outcome"
    );
    assert_eq!(
        counter_total("mcp.requests", &request_labels),
        requests_before + 1,
        "every request must increment the request counter, labelled by method"
    );
    assert!(
        histogram_count("mcp.request.duration", &request_labels) > 0,
        "every request must record its latency into the duration histogram"
    );
}

/// AC: the request span carries the transport kind, so a failed request can be
/// told apart from one that arrived over a different door.
#[test]
fn request_span_carries_the_transport_kind() {
    let recorded = capture(|| async {
        let mut session = Session::new(demo_core()).on_transport(TransportKind::Unix);
        session
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "initialize",
                "params": {},
            }))
            .await;
    });

    let transports: Vec<&String> = recorded
        .spans
        .iter()
        .filter_map(|span| span.fields.get("transport"))
        .collect();

    assert!(
        transports.iter().any(|value| value.as_str() == "unix"),
        "the request span must carry the transport it arrived on; the spans were {:?}",
        recorded.span_summary()
    );
}

/// Drive messages through one session, without capturing anything.
fn drive(messages: &[Value]) {
    let messages = messages.to_vec();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime");
    runtime.block_on(async move {
        let mut session = Session::new(core());
        for message in messages {
            session.handle_message(message).await;
        }
    });
}

fn core() -> Arc<ServerCore> {
    demo_core()
}

/// The lifetime total of one counter series, or zero when it has never been
/// recorded. The registry is process-wide, so every assertion here is a delta.
fn counter_total(name: &str, labels: &[Label]) -> u64 {
    metrics::global()
        .snapshot()
        .counters
        .iter()
        .find(|counter| counter.name == name && same_labels(&counter.labels, labels))
        .map_or(0, |counter| counter.total)
}

/// The lifetime measurement count of one histogram series.
fn histogram_count(name: &str, labels: &[Label]) -> u64 {
    metrics::global()
        .snapshot()
        .histograms
        .iter()
        .find(|histogram| histogram.name == name && same_labels(&histogram.labels, labels))
        .map_or(0, |histogram| histogram.total.count)
}

fn same_labels(recorded: &[Label], wanted: &[Label]) -> bool {
    recorded.len() == wanted.len()
        && wanted.iter().all(|want| {
            recorded
                .iter()
                .any(|have| have.key() == want.key() && have.value() == want.value())
        })
}
