//! Telemetry, re-exported for the servers that depend on this crate.
//!
//! A server reaches traces, metrics and logs through here and never adds
//! `adelie-telemetry` or any opentelemetry crate to its own manifest. Two
//! reasons, and the second is the one that bites:
//!
//! - A direct opentelemetry dependency is unconditional. It would be compiled
//!   into every stdio-only server whether or not the `otel` feature is on,
//!   which is exactly what a default build must not pay for.
//! - The version of `adelie-telemetry` a server records into has to be the
//!   version this crate installed the subscriber from. Two copies in one
//!   process means two metric registries, and half the numbers go nowhere.
//!
//! # Recording a metric
//!
//! ```
//! use mcp_core::telemetry::metrics::{self, Label};
//!
//! metrics::increment("weather.lookups", &[Label::new("provider", "example")]);
//! ```
//!
//! Metric names are `&'static str`; variation goes in labels, where the
//! cardinality cap can bound it. A label value is a name, never content: a
//! tool argument used as a label is both a data leak and a memory leak.
//!
//! # Putting a caller's value on a log line
//!
//! [`Safe`] is the one way to do it. A tool name, a search term or an error
//! message that quotes the input back can end the log line and start one that
//! reads as genuine, drive the terminal with an escape, or reverse what a
//! reader sees with a bidi control. None of them is bounded in length either,
//! and with `otel` on a span field leaves the process verbatim.
//!
//! ```
//! use mcp_core::telemetry::Safe;
//!
//! # let tool_name = "search";
//! # let detail = "not found";
//! tracing::info!(tool = %Safe::name(tool_name), "tool call finished");
//! tracing::debug!(reason = %Safe::message(detail), "tool returned an error");
//! ```
//!
//! It wraps anything that can be displayed, so a JSON value or an error goes
//! through it without being rendered to a `String` first. A server reaches it
//! from here rather than writing its own: a second copy drifts, and the caps
//! then differ between two binaries reading the same value.
//!
//! [`Safe::name`] caps at 128 bytes and [`Safe::message`] at 1024.
//!
//! # Installing the subscriber
//!
//! [`crate::run`] already calls [`init`] with the server's own name, so a
//! server built on the standard entry point does nothing. [`init`] is
//! re-exported for the server that owns its own `main` and never reaches
//! `run`. Calling it twice in one process is a no-op, and a library hosted
//! inside another binary must not call it at all.
//!
//! # Turning export on
//!
//! Off by default. A server adds the passthrough to its own manifest, which
//! reaches this crate and then `adelie-telemetry`:
//!
//! ```toml
//! [features]
//! otel = ["mcp-core/otel"]
//! ```
//!
//! Everything else comes from the standard `OTEL_*` environment variables.

pub use adelie_telemetry::{Config, Error, Guard, Safe, init, metrics, trace_context};
