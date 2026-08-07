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
//! owns its own `main` and calls [`crate::serve`] directly does the same with
//! [`StopSignals`]:
//!
//! ```no_run
//! # async fn example(core: std::sync::Arc<mcp_core::ServerCore>) -> mcp_core::Result<()> {
//! # let args = mcp_core::CommonServeArgs::default();
//! use mcp_core::telemetry;
//!
//! let telemetry = telemetry::init(telemetry::Config::new("example-mcp"))?;
//! let mut stop = mcp_core::shutdown::StopSignals::install()?;
//!
//! let signal = tokio::select! {
//!     // The client ended the session. The guard drops as this returns.
//!     result = mcp_core::serve(core, &args) => return result,
//!     signal = stop.recv() => signal,
//! };
//! tracing::info!(%signal, "stopping");
//!
//! // In this order, and both steps matter. The flush has to happen here,
//! // because the exit runs no destructor. The exit has to happen at all,
//! // because a returning stdio server hangs; see "What stopping costs".
//! drop(telemetry);
//! std::process::exit(0);
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
//! its terminal today, and would start dying with it.
//!
//! # What stopping costs
//!
//! Open connections are cut, not drained. A signal means the process is going
//! away inside the termination grace period, and a client must already handle a
//! transport that closes under it.
//!
//! [`crate::McpService::shutdown`] is **not** called. It is a de-initialize hook
//! driven by a client `shutdown` request, after which the session keeps serving,
//! so a service cannot tell that meaning from "the process is stopping".
//!
//! The process exits 0 rather than dying by the signal. Kubernetes reports the
//! container as Completed, and systemd counts exit 0 as success.
//!
//! [`crate::run`] ends the process itself once the flush is done, rather than
//! returning to `main`. `tokio::io::stdin` reads on a blocking task, and
//! dropping that read does not end it, so the runtime's own shutdown would wait
//! for a stdin read that the peer is still holding open, and a stdio server
//! would flush and then hang. A server that calls [`crate::serve`] and uses
//! [`StopSignals`] directly keeps control of its own exit, and takes on that
//! question with it.
//!
//! # How long it takes
//!
//! Stopping adds no wait of its own. The flush is bounded by the telemetry
//! guard's own shutdown budget, five seconds by default, which is well inside
//! the 30-second Kubernetes grace period.
//!
//! A second signal that arrives during the flush is absorbed: the handler is
//! still registered, and nothing is left waiting on it. It neither shortens the
//! budget nor kills the process early, so a second Ctrl-C looks ignored for as
//! long as the flush takes.

use tokio::signal::unix::{Signal, SignalKind};

use crate::error::Result;

/// Which signal asked the process to stop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    /// Why the `Some` patterns: a closed stream would otherwise read as a
    /// signal, and stopping a server that nobody asked to stop is worse than
    /// missing one. A closed stream instead disables that branch, and this
    /// future stops resolving rather than resolving wrongly.
    pub async fn recv(&mut self) -> StopSignal {
        tokio::select! {
            Some(()) = self.terminate.recv() => StopSignal::Terminate,
            Some(()) = self.interrupt.recv() => StopSignal::Interrupt,
            else => std::future::pending().await,
        }
    }
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
