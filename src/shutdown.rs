//! The signals that stop a server, and what happens to telemetry when one
//! arrives.
//!
//! The OTLP exporters buffer, and the telemetry guard flushes them when it is
//! dropped. A process that a signal kills runs no destructor, so it drops
//! nothing and the buffer goes with it. That is the data an operator wants
//! most, because the seconds before a restart are usually what is being
//! investigated, and Kubernetes stops every pod this way.
//!
//! [`crate::run`] therefore listens for the stop signals, flushes the guard, and
//! ends the process on the normal path instead of being killed. A server that
//! owns its own `main` and calls [`crate::serve`] directly does the same:
//!
//! ```no_run
//! # async fn example(core: std::sync::Arc<mcp_core::ServerCore>) -> mcp_core::Result<()> {
//! # let args = mcp_core::CommonServeArgs::default();
//! use mcp_core::{shutdown, telemetry};
//!
//! let telemetry = telemetry::init(telemetry::Config::new("example-mcp"))?;
//!
//! // Install before serving. A signal that arrives before this is still fatal.
//! let mut stop = shutdown::StopSignals::install()?;
//!
//! let signal = tokio::select! {
//!     biased;
//!     // The client ended the session. The guard drops as this returns.
//!     result = mcp_core::serve(core, &args) => return result,
//!     signal = stop.recv() => signal,
//! };
//! shutdown::flush_and_exit(signal, telemetry)
//! # }
//! ```
//!
//! # What is handled, and what is not
//!
//! `SIGTERM` and `SIGINT` are handled, and they are handled identically.
//! `SIGTERM` is what Kubernetes, systemd and `kill` send. `SIGINT` is what a
//! terminal sends on Ctrl-C.
//!
//! `SIGHUP` is deliberately not handled. There is no reload for it to mean,
//! because a server's configuration is fixed by its command line. And a handler
//! would replace an inherited `SIG_IGN`: a server started under `nohup` survives
//! its terminal today, and would start dying with it. Nothing is gained for that.
//!
//! Installing does replace an inherited `SIG_IGN` for the two signals it does
//! handle, and that is a cost rather than an oversight. A shell without job
//! control sets `SIGINT` to `SIG_IGN` in a backgrounded child, so
//! `sh -c 'server &'` in a terminal now stops on Ctrl-C where it used to survive.
//! It is accepted because the two cases are not alike: `nohup` exists to make a
//! server outlive its terminal and is a normal way to run one, while an ignored
//! `SIGINT` is an artefact of a shell these servers are not started from - they
//! are spawned by a parent over stdio, or by systemd, or by Kubernetes. Ctrl-C is
//! also the case where the flush is worth most, because it is a person debugging.
//!
//! Installing only where the current disposition is not `SIG_IGN` would remove
//! that cost and would let `SIGHUP` be handled too. It needs `libc` and an
//! `unsafe` `sigaction` call to read the disposition, which is a high price for
//! an edge no deployment in this fleet reaches.
//!
//! Signals are POSIX, and so is this module. The crate has no Windows target: its
//! default feature set includes the unix-socket transport.
//!
//! # What stopping costs
//!
//! **Open connections are cut, not drained.** A signal means the process is going
//! away inside the termination grace period, and a client must already handle a
//! transport that closes under it.
//!
//! **[`crate::McpService::shutdown`] is not called.** It is a de-initialize hook
//! driven by a client `shutdown` request, after which the session keeps serving,
//! so a service cannot tell that meaning from "the process is stopping".
//!
//! **No destructor runs**, including any `Drop` on the service. Whatever must
//! happen before the process ends has to happen before the exit, which is why
//! [`flush_and_exit`] drops the telemetry guard by hand.
//!
//! **A unix socket file is left behind.** A killed process left one too, and
//! `serve_unix` unlinks a stale socket when it next binds, so none accumulate.
//!
//! **The exit status is 0**, rather than death by the signal. Kubernetes reports
//! the container as Completed and systemd counts exit 0 as success, which is what
//! a Deployment or a service wants. It would read as success for a Kubernetes Job
//! or `restartPolicy: OnFailure` as well, where a drained pod would be recorded as
//! having succeeded. No server in this fleet runs that way.
//!
//! # How long it takes
//!
//! The stop path adds no wait of its own, but it is not free. It waits for the
//! flush, and the flush is bounded by the telemetry guard's own shutdown budget:
//! five seconds by default, and not configurable from a server that uses
//! [`crate::run`]. That is well inside the 30-second Kubernetes default grace
//! period. With no collector, or one that answers, a stop takes about ten
//! milliseconds. Against one whose packets are dropped, it takes the whole budget.
//!
//! A second signal that arrives during the flush is absorbed: the handler is
//! still registered, and nothing is left waiting on it. It neither shortens the
//! budget nor kills the process early, so a second Ctrl-C looks ignored for as
//! long as the flush takes.

use tokio::signal::unix::{Signal, SignalKind};

use crate::error::Result;

/// Which signal asked the process to stop.
///
/// Non-exhaustive: a signal added later must not break a `match` in a server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum StopSignal {
    /// `SIGTERM`: what Kubernetes, systemd and `kill` send.
    Terminate,
    /// `SIGINT`: what a terminal sends on Ctrl-C.
    Interrupt,
}

impl StopSignal {
    /// The signal's name, as an operator reading a log would write it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Terminate => "SIGTERM",
            Self::Interrupt => "SIGINT",
        }
    }
}

impl std::fmt::Display for StopSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Listens for the signals that mean stop.
///
/// Install this before the server starts serving. Registration is what makes a
/// signal survivable, so a signal that arrives between starting and installing
/// is still fatal. Once installed, a signal that arrives before anything awaits
/// [`recv`](Self::recv) is remembered and delivered at the first await.
#[derive(Debug)]
pub struct StopSignals {
    terminate: Signal,
    interrupt: Signal,
}

impl StopSignals {
    /// Start listening for `SIGTERM` and `SIGINT`.
    ///
    /// This changes the disposition of both signals for the whole process, so
    /// only the code that owns the process may call it. In this crate that is
    /// [`crate::run`] and nothing else.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Io`] when the handler cannot be registered.
    pub fn install() -> Result<Self> {
        Ok(Self {
            terminate: tokio::signal::unix::signal(SignalKind::terminate())?,
            interrupt: tokio::signal::unix::signal(SignalKind::interrupt())?,
        })
    }

    /// Resolve when the first of the two signals arrives.
    ///
    /// Why the `Some` patterns rather than `_`: tokio documents `Signal::recv`
    /// as never returning `None`, so they cost nothing today. They are here for
    /// the day that changes, because `_` would read a closed stream as a signal,
    /// and stopping a server nobody asked to stop is worse than missing one. A
    /// closed stream disables that branch instead, and the `else` arm leaves this
    /// future pending rather than resolving wrongly.
    pub async fn recv(&mut self) -> StopSignal {
        tokio::select! {
            Some(()) = self.terminate.recv() => StopSignal::Terminate,
            Some(()) = self.interrupt.recv() => StopSignal::Interrupt,
            else => std::future::pending().await,
        }
    }
}

/// Report the stop, flush telemetry, and end the process with status 0.
///
/// This never returns. It is the ending [`crate::run`] uses, and the ending a
/// server that drives [`crate::serve`] itself should use, so the order that
/// matters is written once:
///
/// 1. The guard is dropped **here**, by hand. The exit below runs no destructor,
///    so leaving the flush to one loses exactly the telemetry this exists to
///    save.
/// 2. The process ends rather than returning. `tokio::io::stdin` reads on a
///    blocking task and dropping that read does not end it, so a stdio server
///    that returned instead would flush and then hang until something killed it.
///
/// Call it only from a binary that owns its process. A library hosting an MCP
/// service inside another binary must never end that binary.
///
/// The module documentation shows it in place, with the install that has to come
/// before it.
pub fn flush_and_exit(signal: StopSignal, telemetry: crate::telemetry::Guard) -> ! {
    tracing::info!(%signal, "stopping");
    drop(telemetry);
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The name reaches a log field, so it has to be the name an operator would
    /// search the log for.
    #[test]
    fn stop_signals_are_named_as_an_operator_writes_them() {
        assert_eq!(StopSignal::Terminate.to_string(), "SIGTERM");
        assert_eq!(StopSignal::Interrupt.to_string(), "SIGINT");
    }

    /// Installing twice must work, because a hosted server may have installed
    /// handlers of its own before `run` ever gets there.
    #[tokio::test]
    async fn installing_twice_is_not_an_error() {
        let first = StopSignals::install().expect("the first install must work");
        let second = StopSignals::install().expect("a second install must work too");
        drop((first, second));
    }

    /// Nothing resolves while no signal has arrived. A `recv` that completed on
    /// its own would stop every server the moment it started serving.
    #[tokio::test]
    async fn recv_does_not_resolve_without_a_signal() {
        let mut stop = StopSignals::install().expect("install must work");
        let waited = tokio::time::timeout(std::time::Duration::from_millis(100), stop.recv()).await;
        assert!(
            waited.is_err(),
            "recv resolved to {waited:?} with no signal delivered"
        );
    }
}
