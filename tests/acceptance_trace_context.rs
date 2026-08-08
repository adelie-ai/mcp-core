//! Acceptance criteria for continuing the caller's trace.
//!
//! A caller that already has a trace puts its W3C trace context in the `_meta`
//! object of the request it sends, which the MCP specification reserves for a
//! field like this one. A server reads it and makes that trace the parent of
//! the work it does, so one trace covers the turn and every server it reached.
//!
//! Each test is named after the criterion it holds, so a failing run names the
//! unmet requirement rather than a line number.

mod support;

use serde_json::{Value, json};
use tracing::Level;

use mcp_core::telemetry::trace_context::MAX_TRACEPARENT_BYTES;
use support::{Recorded, capture_dispatch, capture_dispatch_replies};

/// A valid `traceparent`, and the two ids inside it. The value is the one the
/// W3C specification uses in its own examples.
const CALLER_TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const CALLER_TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const CALLER_SPAN_ID: &str = "00f067aa0ba902b7";

/// The all-zero trace id, which the specification reserves as invalid.
const NIL_TRACE_ID: &str = "00000000000000000000000000000000";

/// The three values a caller can put where this path reads: a `_meta` key it
/// chose for itself, the tool argument beside it, and a `traceparent` that is
/// not a header. Each carries its own marker, so a leak names which one escaped.
const META_SENTINEL: &str = "/home/someone/private/meta-MARKER-a1b2.txt";
const ARGUMENT_SENTINEL: &str = "/home/someone/private/args-MARKER-c3d4.txt";
const TRACEPARENT_SENTINEL: &str = "/home/someone/private/header-MARKER-e5f6.txt";

/// The handshake a session needs before it answers a tool call.
fn initialize() -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
}

/// A `tools/call` carrying the `_meta` object a caller chose.
fn call_with_meta(meta: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": "echo", "arguments": {"text": "hello"}, "_meta": meta},
    })
}

/// Every `trace_id` the run put on a span, whichever span carried it.
fn recorded_trace_ids(recorded: &Recorded) -> Vec<&str> {
    recorded
        .spans
        .iter()
        .filter_map(|span| span.fields.get("trace_id"))
        .map(String::as_str)
        .collect()
}

/// AC: a `tools/call` carrying a valid `params._meta.traceparent` puts that
/// trace id on its `mcp.request` span.
///
/// The field is recorded whether or not the `otel` feature is on, which is what
/// makes a default build correlatable: an operator greps one id across the
/// caller's log and this server's log, and neither process exports anything.
#[test]
fn stdio_request_meta_traceparent_reaches_the_request_span() {
    let recorded = capture_dispatch(&[
        initialize(),
        call_with_meta(json!({"traceparent": CALLER_TRACEPARENT})),
    ]);

    let carried: Vec<&str> = recorded
        .spans
        .iter()
        .filter(|span| span.name == "mcp.request")
        .filter_map(|span| span.fields.get("trace_id"))
        .map(String::as_str)
        .collect();

    assert!(
        carried.contains(&CALLER_TRACE_ID),
        "a request carrying params._meta.traceparent must put the caller's trace id on its \
         mcp.request span; the spans were {:?}",
        recorded.span_summary()
    );
}

/// AC: a request that carried no `_meta` carries no `trace_id` field either.
///
/// Absent, not empty. An empty or sentinel value reads as a real trace, and an
/// operator who greps for it finds every request that never had one.
#[test]
fn a_request_without_meta_records_no_trace_id() {
    let recorded = capture_dispatch(&[
        initialize(),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"text": "hello"}},
        }),
    ]);

    assert!(
        recorded_trace_ids(&recorded).is_empty(),
        "a request that carried no traceparent must carry no trace_id field; the spans were {:?}",
        recorded.span_summary()
    );
}

/// AC: a malformed `traceparent` costs the request its trace and nothing else.
///
/// A caller sets the value, so a bad one must never fail the work. The request
/// is answered as usual and the server starts a trace of its own.
#[test]
fn a_malformed_traceparent_does_not_fail_the_request() {
    // A trace id one character short: the shape a truncating caller produces.
    let malformed = "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01";

    let (recorded, replies) = capture_dispatch_replies(&[
        initialize(),
        call_with_meta(json!({"traceparent": malformed})),
    ]);

    let reply = replies
        .iter()
        .find(|reply| reply.get("id").and_then(Value::as_i64) == Some(2))
        .unwrap_or_else(|| {
            panic!("the tool call must still be answered; the replies were {replies:?}")
        });
    assert!(
        reply.get("error").is_none(),
        "a traceparent a caller chose must not fail the request it arrived on: {reply:?}"
    );
    assert!(
        reply.get("result").is_some(),
        "the tool call must still return its result: {reply:?}"
    );
    assert!(
        recorded_trace_ids(&recorded).is_empty(),
        "an unusable traceparent must leave no trace_id field; the spans were {:?}",
        recorded.span_summary()
    );
}

/// AC: a `traceparent` that is not a string is passed over in silence.
///
/// `_meta` is a caller's own object, so the value under any key there can be
/// any JSON. None of these is a header. None of them may fail the request,
/// none may reach a span, and none may write a line: the documented contract
/// says a value of the wrong type costs nothing at all, and a line about every
/// such request is a cost.
#[test]
fn a_non_string_traceparent_is_ignored() {
    for value in [
        json!(1234),
        json!(null),
        json!(true),
        json!({"version": "00"}),
        json!([CALLER_TRACEPARENT]),
    ] {
        let (recorded, replies) = capture_dispatch_replies(&[
            initialize(),
            call_with_meta(json!({"traceparent": value.clone()})),
        ]);

        let reply = replies
            .iter()
            .find(|reply| reply.get("id").and_then(Value::as_i64) == Some(2))
            .unwrap_or_else(|| {
                panic!(
                    "the tool call must still be answered for {value}; the replies were {replies:?}"
                )
            });
        assert!(
            reply.get("error").is_none(),
            "a traceparent of {value} must not fail the request it arrived on: {reply:?}"
        );
        assert!(
            recorded_trace_ids(&recorded).is_empty(),
            "a traceparent of {value} must leave no trace_id field; the spans were {:?}",
            recorded.span_summary()
        );

        // Silence, at every level. Only a header that parsed and failed earns a
        // line; a value of the wrong type is not a header at all.
        for event in &recorded.events {
            for (key, rendered) in &event.fields {
                assert!(
                    !rendered.contains("traceparent"),
                    "a traceparent of {value} must write no line about itself, but a {} line \
                     field {key:?} says: {rendered:?}",
                    event.level
                );
            }
        }
    }
}

/// AC: a `traceparent` past `MAX_TRACEPARENT_BYTES` is rejected.
///
/// The header arrives inside a client frame, and a frame has no field limit of
/// its own, so length is what bounds the work this parse costs.
#[test]
fn an_oversized_traceparent_is_rejected() {
    // Valid in every field, and one byte past the limit. A future version may
    // append fields, so the parser ignores what it does not know and length is
    // the only thing left to reject it by.
    let padding = MAX_TRACEPARENT_BYTES + 1 - CALLER_TRACEPARENT.len() - 1;
    let oversized = format!("{CALLER_TRACEPARENT}-{}", "0".repeat(padding));
    assert_eq!(
        oversized.len(),
        MAX_TRACEPARENT_BYTES + 1,
        "this test only proves anything if its input is over the limit"
    );

    let recorded = capture_dispatch(&[
        initialize(),
        call_with_meta(json!({"traceparent": oversized})),
    ]);

    assert!(
        recorded_trace_ids(&recorded).is_empty(),
        "a traceparent over the byte limit must leave no trace_id field; the spans were {:?}",
        recorded.span_summary()
    );
}

/// AC: an all-zero trace id is rejected.
///
/// The specification reserves it as the "no trace" sentinel, and a backend
/// drops a span that carries it.
#[test]
fn a_nil_trace_id_traceparent_is_rejected() {
    let nil = format!("00-{NIL_TRACE_ID}-{CALLER_SPAN_ID}-01");

    let recorded = capture_dispatch(&[initialize(), call_with_meta(json!({"traceparent": nil}))]);

    assert!(
        recorded_trace_ids(&recorded).is_empty(),
        "an all-zero trace id must leave no trace_id field; the spans were {:?}",
        recorded.span_summary()
    );
    for span in &recorded.spans {
        for (key, value) in &span.fields {
            assert!(
                !value.contains(NIL_TRACE_ID),
                "the invalid sentinel reached span {:?} field {key:?}: {value:?}",
                span.name
            );
        }
    }
}

/// AC: reading the `traceparent` carries no other value with it (D10).
///
/// The `_meta` object holds whatever keys a caller chose, and `params` holds
/// the tool arguments. Only the trace id may leave this path, and it is 32
/// hexadecimal characters by construction.
#[test]
fn traceparent_handling_records_no_caller_content() {
    let recorded = capture_dispatch(&[
        initialize(),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "echo",
                "arguments": {"path": ARGUMENT_SENTINEL},
                "_meta": {
                    "traceparent": CALLER_TRACEPARENT,
                    "example.com/caller-note": META_SENTINEL,
                },
            },
        }),
        // The `traceparent` value itself is a caller's, and it is only a header
        // when it is a string. Anything else is content like any other.
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "echo",
                "arguments": {"text": "hello"},
                "_meta": {"traceparent": {"note": TRACEPARENT_SENTINEL}},
            },
        }),
    ]);

    assert!(
        recorded_trace_ids(&recorded).contains(&CALLER_TRACE_ID),
        "the run must read the traceparent, or this test holds nothing; the spans were {:?}",
        recorded.span_summary()
    );

    for (source, marker) in [
        ("a caller's own _meta key", "MARKER-a1b2"),
        ("a tool argument", "MARKER-c3d4"),
        ("a traceparent that is not a header", "MARKER-e5f6"),
    ] {
        for span in &recorded.spans {
            for (key, value) in &span.fields {
                assert!(
                    !value.contains(marker),
                    "{source} reached span {:?} field {key:?}: {value:?}",
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
                    !value.contains(marker),
                    "{source} reached a {} line, field {key:?}: {value:?}",
                    event.level
                );
            }
        }
    }
}

#[cfg(feature = "otel")]
mod otel {
    use std::sync::{Arc, Mutex};

    use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use serde_json::{Value, json};
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    use tracing_subscriber::layer::SubscriberExt;

    use mcp_core::{CallError, McpService, ServerConfig, ServerCore, Session, ToolDef, ToolReply};

    use super::{CALLER_SPAN_ID, CALLER_TRACE_ID, CALLER_TRACEPARENT, call_with_meta, initialize};

    /// The trace and span a tool call ran inside.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Ran {
        trace_id: String,
        span_id: String,
    }

    /// A service that reports the OpenTelemetry context it was called inside.
    struct TraceProbe(Arc<Mutex<Vec<Ran>>>);

    #[mcp_core::async_trait]
    impl McpService for TraceProbe {
        fn tools(&self) -> Vec<ToolDef> {
            vec![ToolDef::new(
                "echo",
                "report the trace it runs in",
                json!({"type": "object", "additionalProperties": true}),
            )]
        }

        async fn call_tool(&self, _name: &str, _args: &Value) -> Result<ToolReply, CallError> {
            let context = tracing::Span::current().context();
            let span_context = context.span().span_context().clone();
            let ran = Ran {
                trace_id: span_context.trace_id().to_string(),
                span_id: span_context.span_id().to_string(),
            };
            self.0
                .lock()
                .expect("the probe lock is only held to push one record")
                .push(ran.clone());
            Ok(ToolReply::text(ran.trace_id))
        }
    }

    /// AC: with the OpenTelemetry layer installed, the server's spans join the
    /// caller's trace instead of starting one of their own.
    ///
    /// The span id must differ from the caller's, because the server opens its
    /// own span inside that trace rather than reporting the caller's back.
    #[test]
    fn with_otel_the_request_span_joins_the_callers_trace() {
        let provider = SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer().with_tracer(provider.tracer("mcp-core-acceptance")),
        );

        let seen = Arc::new(Mutex::new(Vec::new()));
        let probe = Arc::clone(&seen);
        tracing::subscriber::with_default(subscriber, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a current-thread runtime");
            runtime.block_on(async move {
                let core = ServerCore::new(
                    ServerConfig::new("demo-mcp", "0.0.0"),
                    Arc::new(TraceProbe(probe)),
                );
                let mut session = Session::new(core);
                session.handle_message(initialize()).await;
                session
                    .handle_message(call_with_meta(json!({"traceparent": CALLER_TRACEPARENT})))
                    .await;
            });
        });

        let seen = seen
            .lock()
            .expect("the probe lock is only held to push one record")
            .clone();
        let [ran] = seen.as_slice() else {
            panic!("the tool must have run exactly once, but it recorded {seen:?}");
        };
        assert_eq!(
            ran.trace_id, CALLER_TRACE_ID,
            "the server's work must belong to the trace the caller sent"
        );
        assert_ne!(
            ran.span_id, CALLER_SPAN_ID,
            "the server must open its own span inside that trace, not report the caller's back"
        );
    }
}
