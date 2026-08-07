//! Acceptance criteria for what a server does when a signal stops it.
//!
//! Every test here drives a real process, spawns it, signals it, and reads its
//! real stderr. An in-process test can prove what the telemetry guard does when
//! it is dropped. Only a real process can prove that a process the operating
//! system stopped got as far as dropping it, and that is the whole question.
//!
//! Each test is named after the criterion it holds, so a failing run names the
//! unmet requirement rather than a line number.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long a signalled server may take to stop before the test gives up.
///
/// The flush itself is bounded by the telemetry guard's own shutdown budget,
/// five seconds by default, so anything past this is a hang rather than a slow
/// collector.
const STOP_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait for a line to appear on the probe's stderr. Only the
/// transports that have no stdin to drive wait for one.
#[cfg(any(feature = "unix", feature = "websocket"))]
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// The line the telemetry guard writes when it closes the last window. Its
/// absence is exactly the loss this ticket is about.
const SUMMARY: &str = "metrics summary";

/// AC: a server stopped by `SIGTERM` over stdio flushes its telemetry.
#[test]
fn sigterm_over_stdio_flushes_the_final_metrics_summary() {
    let stderr = stdio_probe_stopped_by("TERM").stderr;
    assert!(
        stderr.contains(SUMMARY),
        "a SIGTERM over stdio must still write the final metrics summary, \
         but stderr was: {stderr:?}"
    );
}

/// AC: `SIGINT` behaves the same way as `SIGTERM`.
///
/// One transport is enough for this one. The signal and the transport are
/// orthogonal in the code: both signals resolve the same `select!` arm, and what
/// follows knows nothing about which transport was serving. `SIGTERM` covers all
/// three transports, and this covers both signals, so the pair crosses the whole
/// surface without a test per combination.
#[test]
fn sigint_over_stdio_flushes_the_final_metrics_summary() {
    let stderr = stdio_probe_stopped_by("INT").stderr;
    assert!(
        stderr.contains(SUMMARY),
        "a SIGINT must be treated exactly as a SIGTERM, but stderr was: {stderr:?}"
    );
}

/// AC: the flushed summary carries the numbers the run really recorded, not an
/// empty shell of a summary.
///
/// The server answered one request before the signal, so `mcp.requests` has to
/// be in the dump. A summary with no counters in it would prove the guard ran
/// and prove nothing about the data.
#[test]
fn the_flushed_summary_carries_the_counters_the_run_recorded() {
    let stderr = stdio_probe_stopped_by("TERM").stderr;
    assert!(
        stderr.contains("mcp.requests"),
        "the flushed summary must carry the request counter the run recorded, \
         but stderr was: {stderr:?}"
    );
}

/// AC: a signalled server reports a clean exit status rather than dying by the
/// signal.
///
/// `code()` is `None` for a process killed by a signal, so this distinguishes
/// "handled the signal and returned" from "was killed by it".
#[test]
fn a_signalled_server_exits_zero_rather_than_dying_by_signal() {
    let status = stdio_probe_stopped_by("TERM").status;
    assert_eq!(
        status.code(),
        Some(0),
        "a signalled server must exit 0, not die by the signal; status was {status:?}"
    );
}

/// AC: two signals in quick succession neither panic nor flush twice.
///
/// Whether the second signal lands during the flush or just after it depends on
/// how long the flush takes, and with no collector configured that is a fraction
/// of a millisecond. This test does not control which of the two happens, so it
/// asserts what must hold either way: the process still exits 0, nothing panics,
/// and exactly one summary is written.
#[test]
fn a_second_signal_during_shutdown_neither_panics_nor_double_flushes() {
    let mut probe = Probe::start(&["serve"], Stdio::piped());
    probe.request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);

    probe.signal("TERM");
    // The second signal is best effort: the process may already have gone, and
    // `kill` then reports no such process. The point is that a second one is
    // survivable, not that it is delivered.
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(probe.pid().to_string())
        .status();

    let stopped = probe.finish();
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "a second signal must not stop the process exiting cleanly; status was {:?}. \
         stderr was: {:?}",
        stopped.status,
        stopped.stderr
    );
    assert!(
        !stopped.stderr.contains("panicked"),
        "a second signal must not panic: {:?}",
        stopped.stderr
    );
    let summaries = stopped.stderr.matches(SUMMARY).count();
    assert_eq!(
        summaries, 1,
        "the guard must flush exactly once however many signals arrive, but stderr \
         held {summaries} summaries: {:?}",
        stopped.stderr
    );
}

/// AC: the stop path writes nothing to stdout.
///
/// The stdio transport frames JSON-RPC on stdout. One stray line from a signal
/// handler corrupts the stream for the client that is still reading it.
#[test]
fn the_stop_path_writes_nothing_to_stdout() {
    // `request` already read the one reply the server owed, so everything here
    // is what the stop path added. It has to be nothing at all: a stray line
    // that happened to parse as JSON would corrupt the stream just as surely as
    // one that did not.
    let stopped = stdio_probe_stopped_by("TERM");
    assert!(
        stopped.stdout.is_empty(),
        "the stop path must write nothing to stdout, but it wrote: {:?}",
        stopped.stdout
    );
}

/// AC: a server stopped by `SIGTERM` over the unix transport flushes its
/// telemetry.
///
/// The unix accept loop has no end of its own, so before this ticket a signal
/// was the only way it ever stopped, and it lost everything buffered each time.
#[cfg(feature = "unix")]
#[test]
fn sigterm_over_unix_flushes_the_final_metrics_summary() {
    let socket = temp_socket_path("unix-flush");
    let mut probe = Probe::start(
        &[
            "serve",
            "--transport",
            "unix",
            "--socket-path",
            socket.to_str().expect("a UTF-8 socket path"),
        ],
        Stdio::null(),
    );
    probe.wait_until_listening();

    // One real request, so the summary has something in it to lose.
    {
        use std::os::unix::net::UnixStream;
        let mut stream = UnixStream::connect(&socket).expect("the probe must be listening");
        stream
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .expect("the probe must accept a request");
        let mut reply = String::new();
        BufReader::new(&stream)
            .read_line(&mut reply)
            .expect("the probe must answer it");
        assert!(reply.contains("\"result\""), "unexpected reply: {reply:?}");
    }

    probe.signal("TERM");
    let stopped = probe.finish();
    let _ = std::fs::remove_file(&socket);

    assert_eq!(
        stopped.status.code(),
        Some(0),
        "a signalled unix server must exit 0; status was {:?}, stderr was {:?}",
        stopped.status,
        stopped.stderr
    );
    assert!(
        stopped.stderr.contains(SUMMARY),
        "a SIGTERM over unix must still write the final metrics summary, \
         but stderr was: {:?}",
        stopped.stderr
    );
}

/// AC: a server stopped by `SIGTERM` over the websocket transport flushes its
/// telemetry.
///
/// No request is driven through this one, because a websocket client is a
/// dependency this crate has no other use for. The probe records a counter as
/// it starts, so the summary has content whichever transport it was serving,
/// and the criterion here is the flush rather than the dispatch path.
#[cfg(feature = "websocket")]
#[test]
fn sigterm_over_websocket_flushes_the_final_metrics_summary() {
    // Port 0 takes whatever is free, so parallel runs never collide.
    let mut probe = Probe::start(
        &["serve", "--transport", "websocket", "--port", "0"],
        Stdio::null(),
    );
    probe.wait_until_listening();

    probe.signal("TERM");
    let stopped = probe.finish();

    assert_eq!(
        stopped.status.code(),
        Some(0),
        "a signalled websocket server must exit 0; status was {:?}, stderr was {:?}",
        stopped.status,
        stopped.stderr
    );
    assert!(
        stopped.stderr.contains(SUMMARY),
        "a SIGTERM over websocket must still write the final metrics summary, \
         but stderr was: {:?}",
        stopped.stderr
    );
}

/// The flush that already worked has to keep working: a client that closes the
/// stdio stream still gets the final summary, and the process still exits 0.
#[test]
fn a_clean_eof_still_flushes_the_final_metrics_summary() {
    let mut probe = Probe::start(&["serve"], Stdio::piped());
    probe.request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    drop(probe.child.stdin.take());

    let stopped = probe.finish();
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "a clean EOF must still exit 0; status was {:?}",
        stopped.status
    );
    assert!(
        stopped.stderr.contains(SUMMARY),
        "a clean EOF must still write the final metrics summary, but stderr was: {:?}",
        stopped.stderr
    );
}

/// A real failure still fails, and still flushes.
///
/// The `select!` that races the serve loop against a signal put a second exit
/// path into `run`. No signal is sent here: the point is that the first path
/// still works, and that racing it did not turn a transport error into a clean
/// exit. An oversize declared frame ends the connection, and the process has to
/// say so with a non-zero status while still writing its summary.
#[test]
fn a_transport_failure_still_exits_non_zero_and_still_flushes() {
    let mut probe = Probe::start(&["serve"], Stdio::piped());
    {
        let stdin = probe
            .child
            .stdin
            .as_mut()
            .expect("the probe has a piped stdin");
        stdin
            .write_all(b"Content-Length: 9999999999\r\n\r\n")
            .expect("the probe must accept its input");
    }
    drop(probe.child.stdin.take());

    let stopped = probe.finish();
    assert_eq!(
        stopped.status.code(),
        Some(1),
        "a framing error must still exit non-zero; status was {:?}, stderr was {:?}",
        stopped.status,
        stopped.stderr
    );
    assert!(
        stopped.stderr.contains(SUMMARY),
        "a failing server must still write the final metrics summary, but stderr was: {:?}",
        stopped.stderr
    );
}

/// Start a stdio probe, drive one request through it so the metrics registry
/// has something in it, then stop it with `signal_name`.
fn stdio_probe_stopped_by(signal_name: &str) -> Stopped {
    let mut probe = Probe::start(&["serve"], Stdio::piped());
    probe.request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    probe.signal(signal_name);
    probe.finish()
}

/// A running probe process, with its stderr being drained on another thread.
struct Probe {
    child: Child,
    stdout: BufReader<std::process::ChildStdout>,
    stderr: StderrTail,
}

/// What a stopped probe left behind.
struct Stopped {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Probe {
    /// Spawn the probe example with `args`, and start draining its stderr.
    fn start(args: &[&str], stdin: Stdio) -> Self {
        let binary = probe_binary();
        assert!(
            binary.is_file(),
            "the probe example must be built before this test can prove anything; \
             expected it at {}",
            binary.display()
        );
        let mut child = Command::new(&binary)
            .args(args)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the probe must start");
        let stdout = BufReader::new(child.stdout.take().expect("the probe has a piped stdout"));
        let stderr = StderrTail::attach(child.stderr.take().expect("the probe has a piped stderr"));
        Self {
            child,
            stdout,
            stderr,
        }
    }

    /// Send one JSON-RPC request and read its reply, so the caller knows the
    /// server is up and has recorded a metric.
    fn request(&mut self, request: &str) {
        let stdin = self
            .child
            .stdin
            .as_mut()
            .expect("the probe has a piped stdin");
        writeln!(stdin, "{request}").expect("the probe must accept its input");
        stdin.flush().expect("the request must reach the probe");
        let mut reply = String::new();
        self.stdout
            .read_line(&mut reply)
            .expect("the probe must answer the request");
        assert!(
            reply.contains("\"result\""),
            "the probe must be serving before it is signalled, but replied {reply:?}"
        );
    }

    /// Block until the probe reports that it is listening.
    ///
    /// This watches the process as well as the stream. A probe that stops before
    /// it listens has written the reason on its stderr, and reporting that reason
    /// at once beats waiting out the timeout to say nothing. The reason is
    /// usually a probe built with different features from this test, which is
    /// what `cargo test --test <name>` leaves behind: it reuses the example
    /// binary on disk and never rebuilds it.
    #[cfg(any(feature = "unix", feature = "websocket"))]
    fn wait_until_listening(&mut self) {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let seen = self.stderr.snapshot();
            if seen.contains("listening") {
                return;
            }
            if let Some(status) = self
                .child
                .try_wait()
                .expect("the probe's state must be readable")
            {
                panic!(
                    "the probe stopped with {status:?} before it listened, so this test never \
                     ran. Its own report was: {seen:?}. A missing feature there means the \
                     example binary on disk was built with different features from this test; \
                     run `cargo test`, which rebuilds it."
                );
            }
            assert!(
                Instant::now() < deadline,
                "the probe never reported listening within {READY_TIMEOUT:?}; \
                 what it did write was: {seen:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Send `signal_name` (as `kill` names it, so `TERM` or `INT`).
    fn signal(&self, signal_name: &str) {
        let status = Command::new("kill")
            .arg(format!("-{signal_name}"))
            .arg(self.pid().to_string())
            .status()
            .expect("kill must run, or this test proves nothing");
        assert!(
            status.success(),
            "kill -{signal_name} on the probe failed, so no signal was delivered"
        );
    }

    /// Wait for the probe to stop, then collect everything it wrote.
    fn finish(mut self) -> Stopped {
        let status = wait_for_exit(&mut self.child);
        let mut stdout = String::new();
        // The child has exited, so this reads to EOF without blocking.
        std::io::Read::read_to_string(&mut self.stdout, &mut stdout).unwrap_or_default();
        Stopped {
            status,
            stdout,
            stderr: self.stderr.finish(),
        }
    }
}

/// Wait for `child` to exit, and fail the test rather than hang if it does not.
fn wait_for_exit(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        match child
            .try_wait()
            .expect("the probe's state must be readable")
        {
            Some(status) => return status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!(
                    "the probe did not stop within {STOP_TIMEOUT:?}, so the shutdown is \
                     not bounded"
                );
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Drains a child's stderr on its own thread.
///
/// Reading it only at the end would deadlock a server that fills the pipe while
/// the test is waiting on something else, and the websocket test has to read a
/// line from a process that is still running.
struct StderrTail {
    text: Arc<Mutex<String>>,
    reader: Option<JoinHandle<()>>,
}

impl StderrTail {
    fn attach(stderr: ChildStderr) -> Self {
        let text = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&text);
        let reader = std::thread::spawn(move || {
            let mut lines = BufReader::new(stderr).lines();
            while let Some(Ok(line)) = lines.next() {
                let mut sink = sink.lock().unwrap_or_else(|e| e.into_inner());
                sink.push_str(&line);
                sink.push('\n');
            }
        });
        Self {
            text,
            reader: Some(reader),
        }
    }

    fn snapshot(&self) -> String {
        self.text.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Wait for stderr to reach EOF, then return everything it carried.
    fn finish(mut self) -> String {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        self.snapshot()
    }
}

/// A socket path in the temporary directory, unique to this process and label.
#[cfg(feature = "unix")]
fn temp_socket_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "mcp-core-signal-{label}-{}.sock",
        std::process::id()
    ))
}

/// The probe example, as `cargo test` built it.
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
