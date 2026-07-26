//! Framed JSON-RPC transport over any byte stream (stdio, unix socket).
//!
//! Supports both newline-delimited JSON and LSP-style
//! `Content-Length: N\r\n\r\n<bytes>` framing, auto-detected from the first
//! line.
//!
//! Nothing a peer declares about a frame is trusted before it is checked. A
//! configurable size cap (`max_len`) bounds memory in both modes: a declared
//! `Content-Length` is range-checked against the cap before any body buffer
//! exists and the body then grows with the bytes that actually arrive, while
//! newline lines are read incrementally and rejected after at most
//! `max_len + 1` bytes. So neither a huge `Content-Length`, nor a lie about one,
//! nor an endless newline-free line can exhaust memory. The header block is
//! bounded too ([`MAX_HEADER_LINES`]), so one frame cannot cost unbounded reads.
//!
//! A refused frame ends the stream. Every violation is detected *before* the
//! offending frame has been consumed, so the position of the next frame is
//! unknowable; carrying on would let a peer hide a well-formed frame behind a
//! refused one. The transport therefore marks itself desynchronised and every
//! later read fails, leaving the caller to close the connection.

use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, Stdin,
    Stdout,
};

use crate::error::{Error, Result, TransportError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    Auto,
    Newline,
    ContentLength,
}

/// Upper bound on the header lines one `Content-Length` frame may carry.
///
/// Why: real peers send one or two (`Content-Length`, occasionally
/// `Content-Type`). Each line is individually bounded by `max_len`, but the
/// header block is not, so without a count a peer can keep a server reading
/// headers for a single frame forever.
pub const MAX_HEADER_LINES: usize = 64;

/// Upper bound on the peer-supplied characters an error message echoes back.
/// Enough to diagnose a bad header. Short enough that a refused frame cannot
/// turn our error path into a carrier for the peer's own bytes.
const ERROR_ECHO_LIMIT: usize = 64;

fn trim_crlf(s: &str) -> &str {
    s.trim_end_matches(['\r', '\n'])
}

/// `s` shortened to [`ERROR_ECHO_LIMIT`] characters, marked when shortened.
fn for_error(s: &str) -> String {
    match s.char_indices().nth(ERROR_ECHO_LIMIT) {
        Some((end, _)) => format!("{}...", &s[..end]),
        None => s.to_string(),
    }
}

/// The raw value of `line` if it is a `Content-Length` header, else `None`.
///
/// Why recognition is split from parsing: a value that does not parse has to be
/// a framing *error*, not an unrecognised header. Folding the two together made
/// `Content-Length: 99999999999999999999999999` (too large for `usize`) look
/// like an ordinary line, which silently downgraded the stream to newline
/// framing and handed the frame's own body back to the caller as messages.
fn content_length_value(line: &str) -> Option<&str> {
    let (name, value) = trim_crlf(line).trim().split_once(':')?;
    name.trim()
        .eq_ignore_ascii_case("content-length")
        .then(|| value.trim())
}

/// A framed transport over a buffered reader and a writer.
pub struct FramedTransport<R, W> {
    reader: R,
    writer: W,
    framing: Framing,
    max_len: usize,
    /// Why framing was lost, once it has been. Set by the first violation and
    /// never cleared; while set, every read fails. See [`Self::desync_error`].
    desync: Option<String>,
}

impl FramedTransport<BufReader<Stdin>, Stdout> {
    /// A transport over the process's stdin/stdout.
    pub fn stdio(max_len: usize) -> Self {
        Self::new(
            BufReader::new(tokio::io::stdin()),
            tokio::io::stdout(),
            max_len,
        )
    }
}

impl<R, W> FramedTransport<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Wrap a reader/writer pair (e.g. the halves of a unix stream).
    pub fn new(reader: R, writer: W, max_len: usize) -> Self {
        Self {
            reader,
            writer,
            framing: Framing::Auto,
            max_len,
            desync: None,
        }
    }

    /// Message for a length that is past the cap, phrased the same way wherever
    /// the length came from. `shown` may be raw peer text (a declared value can
    /// carry any number of leading zeros), so it is truncated here rather than at
    /// each call site.
    fn too_large(&self, what: &str, shown: &str) -> String {
        format!(
            "{what} {} exceeds maximum of {} bytes",
            for_error(shown),
            self.max_len
        )
    }

    /// Record that framing has been lost, and build the error to return.
    ///
    /// Why the transport stays refused: every violation we detect is refused
    /// before its frame has been consumed - an oversize body is never read, an
    /// oversize line is abandoned mid-line - so the next frame boundary is
    /// unknown. The protocol offers no resynchronisation point, and reading on
    /// regardless would let a peer hide a well-formed frame behind a refused one
    /// and have it dispatched. The first reason is kept: later reads report the
    /// original cause, not a cascade.
    fn desync_error(&mut self, reason: String) -> Error {
        let reason = self.desync.get_or_insert(reason);
        TransportError::InvalidMessage(reason.clone()).into()
    }

    /// Refuse `len` if it is past the cap. A violation is unrecoverable, so this
    /// also desynchronises the transport ([`Self::desync_error`]).
    fn check_len(&mut self, len: usize, what: &str) -> Result<()> {
        if len > self.max_len {
            let reason = self.too_large(what, &len.to_string());
            return Err(self.desync_error(reason));
        }
        Ok(())
    }

    /// Read one JSON-RPC message. Returns
    /// `Err(TransportError::ConnectionClosed)` on clean EOF, and
    /// `Err(TransportError::InvalidMessage)` for every framing violation -
    /// including every later read once one has happened.
    pub async fn read_message(&mut self) -> Result<String> {
        if let Some(reason) = &self.desync {
            return Err(TransportError::InvalidMessage(format!(
                "transport desynchronised by an earlier framing violation ({reason}) \
                 and cannot be resynchronised"
            ))
            .into());
        }
        match self.framing {
            Framing::Auto => self.read_auto().await,
            Framing::Newline => self.read_newline().await,
            Framing::ContentLength => {
                let first = self.read_line().await?;
                self.read_content_length(&first).await
            }
        }
    }

    /// Write one JSON-RPC message using the detected framing.
    pub async fn write_message(&mut self, message: &str) -> Result<()> {
        match self.framing {
            Framing::ContentLength => {
                let header = format!("Content-Length: {}\r\n\r\n", message.len());
                self.writer.write_all(header.as_bytes()).await?;
                self.writer.write_all(message.as_bytes()).await?;
            }
            Framing::Auto | Framing::Newline => {
                self.writer.write_all(message.as_bytes()).await?;
                self.writer.write_all(b"\n").await?;
            }
        }
        self.writer.flush().await?;
        Ok(())
    }

    /// Read one `\n`-terminated line (newline included), enforcing `max_len`
    /// *while* reading so a peer that never sends a newline can't exhaust
    /// memory: we stop and error after at most `max_len + 1` bytes. The result
    /// is UTF-8-validated and surfaced as [`TransportError::InvalidMessage`] on
    /// failure (not an opaque io error).
    async fn read_line(&mut self) -> Result<String> {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                // Clean EOF. A trailing line without a final newline is still a
                // complete message; only a truly empty read is "closed".
                if buf.is_empty() {
                    return Err(TransportError::ConnectionClosed.into());
                }
                break;
            }
            // How much of this chunk to consume, bounded so we never buffer
            // more than `max_len + 1` bytes total before checking the cap.
            let remaining_budget = self.max_len.saturating_sub(buf.len()) + 1;
            let take = available.len().min(remaining_budget);
            if let Some(nl) = available[..take].iter().position(|&b| b == b'\n') {
                buf.extend_from_slice(&available[..=nl]);
                self.reader.consume(nl + 1);
                break;
            }
            buf.extend_from_slice(&available[..take]);
            self.reader.consume(take);
            // No newline within the budget we just consumed → the line is at
            // least `max_len + 1` bytes. Reject without reading further.
            self.check_len(buf.len(), "line length")?;
        }
        // Belt-and-suspenders: a line that ends exactly at the cap is fine, but
        // anything longer (shouldn't happen given the loop) is rejected.
        self.check_len(buf.len(), "line length")?;
        String::from_utf8(buf)
            .map_err(|e| TransportError::InvalidMessage(format!("invalid UTF-8: {e}")).into())
    }

    async fn read_newline(&mut self) -> Result<String> {
        let line = self.read_line().await?;
        Ok(trim_crlf(&line).to_string())
    }

    async fn read_auto(&mut self) -> Result<String> {
        loop {
            let line = self.read_line().await?;
            let trimmed = trim_crlf(&line);
            if trimmed.trim().is_empty() {
                continue;
            }
            if content_length_value(trimmed).is_some() {
                self.framing = Framing::ContentLength;
                return self.read_content_length(trimmed).await;
            }
            self.framing = Framing::Newline;
            return Ok(trimmed.to_string());
        }
    }

    /// Turn a declared `Content-Length` value into a length we are willing to
    /// read, or the reason we are not. Rejects anything that is not a plain
    /// in-range byte count, so no arithmetic downstream can be surprised.
    fn declared_length(&self, value: &str) -> std::result::Result<usize, String> {
        if value.is_empty() {
            return Err("Content-Length header has an empty value".to_string());
        }
        if !value.bytes().all(|b| b.is_ascii_digit()) {
            // Catches signs, whitespace, units, and anything else non-numeric;
            // a negative value is refused here rather than wrapping.
            return Err(format!(
                "Content-Length value {} is not a byte count",
                for_error(value)
            ));
        }
        match value.parse::<usize>() {
            Ok(len) if len > self.max_len => Err(self.too_large("Content-Length", value)),
            Ok(len) => Ok(len),
            // Digits alone, yet unparseable: larger than `usize` can hold, so
            // past any cap by definition.
            Err(_) => Err(self.too_large("Content-Length", value)),
        }
    }

    async fn read_content_length(&mut self, first: &str) -> Result<String> {
        // Parsed in its own statement so the immutable borrow of `self` ends
        // before the arms need it mutably to record a desync.
        let parsed = content_length_value(first).map(|value| self.declared_length(value));
        let declared = match parsed {
            Some(Ok(len)) => len,
            Some(Err(reason)) => return Err(self.desync_error(reason)),
            None => {
                let reason = format!(
                    "expected a Content-Length header, got: {}",
                    for_error(first)
                );
                return Err(self.desync_error(reason));
            }
        };

        // Consume any remaining headers up to the blank line, bounded so one
        // frame cannot keep us reading headers indefinitely.
        let mut lines = 1;
        loop {
            let line = self.read_line().await?;
            if trim_crlf(&line).is_empty() {
                break;
            }
            lines += 1;
            if lines > MAX_HEADER_LINES {
                let reason = format!("frame declares more than {MAX_HEADER_LINES} header lines");
                return Err(self.desync_error(reason));
            }
        }

        // `declared` is within the cap, but it is still only a claim: read at
        // most that many bytes into a buffer that grows with what actually
        // arrives, so a peer that declares the cap and sends five bytes costs
        // five bytes.
        let mut buf = Vec::new();
        (&mut self.reader)
            .take(declared as u64)
            .read_to_end(&mut buf)
            .await?;
        if buf.len() != declared {
            let reason = format!(
                "frame body ended after {} of {declared} declared bytes",
                buf.len()
            );
            return Err(self.desync_error(reason));
        }
        String::from_utf8(buf)
            .map_err(|e| TransportError::InvalidMessage(format!("invalid UTF-8: {e}")).into())
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    use super::*;

    /// Header *recognition* is by name only, case-insensitively — whether the
    /// value is usable is a separate question, answered by `declared_length`.
    #[test]
    fn recognises_content_length_header_by_name() {
        assert_eq!(content_length_value("Content-Length: 10\r\n"), Some("10"));
        assert_eq!(content_length_value("content-length:  0"), Some("0"));
        assert_eq!(content_length_value("Content-Length:"), Some(""));
        assert_eq!(content_length_value("Content-Length: nope"), Some("nope"));
        assert_eq!(content_length_value("Content-Type: x"), None);
        assert_eq!(content_length_value("garbage"), None);
    }

    #[test]
    fn trims_crlf() {
        assert_eq!(trim_crlf("a\r\n"), "a");
        assert_eq!(trim_crlf("a\n"), "a");
        assert_eq!(trim_crlf("a"), "a");
    }

    #[tokio::test]
    async fn reads_newline_framed() {
        let input = b"{\"a\":1}\n{\"b\":2}\n".to_vec();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 1024);
        assert_eq!(t.read_message().await.unwrap(), "{\"a\":1}");
        assert_eq!(t.read_message().await.unwrap(), "{\"b\":2}");
        assert!(matches!(
            t.read_message().await,
            Err(crate::error::Error::Transport(
                TransportError::ConnectionClosed
            ))
        ));
    }

    #[tokio::test]
    async fn reads_content_length_framed() {
        let body = "{\"hi\":true}";
        let input = format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 1024);
        assert_eq!(t.read_message().await.unwrap(), body);
    }

    #[tokio::test]
    async fn rejects_oversize_content_length_without_allocating() {
        let input = b"Content-Length: 999999999\r\n\r\n".to_vec();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 1024);
        let err = t.read_message().await.unwrap_err();
        assert!(err.to_string().contains("exceeds maximum"), "{err}");
    }

    // --- MC-1: newline framing must bound memory, not just check after the fact ---

    /// An infinite reader that yields `b'a'` forever and never a newline.
    /// If `read_message` buffers the whole "line" before checking the cap,
    /// reading from this never terminates (and memory grows without bound).
    ///
    /// It returns `Pending` (after re-waking) every few chunks so the test's
    /// `tokio::time::timeout` can actually fire if the read isn't bounded —
    /// otherwise a tight always-`Ready` loop would starve the timer and the
    /// failing test would hang the harness instead of failing cleanly.
    #[derive(Default)]
    struct EndlessLine {
        chunks: usize,
    }

    impl tokio::io::AsyncRead for EndlessLine {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.chunks += 1;
            if self.chunks.is_multiple_of(8) {
                // Yield so the runtime can poll other tasks (e.g. the timeout).
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
            const CHUNK: [u8; 4096] = [b'a'; 4096];
            let n = buf.remaining().min(CHUNK.len());
            buf.put_slice(&CHUNK[..n]);
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// MC-1 acceptance: a line exceeding `max_len` errors out after reading at
    /// most `max_len + 1` bytes — it must not buffer the line first.
    #[tokio::test]
    async fn newline_line_exceeding_max_errors_with_bounded_memory() {
        let mut t = FramedTransport::new(BufReader::new(EndlessLine::default()), Vec::new(), 64);
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), t.read_message())
            .await
            .expect("read_message must terminate on an endless line (bounded read)");
        let err = result.unwrap_err();
        assert!(err.to_string().contains("exceeds maximum"), "{err}");
    }

    /// MC-1 acceptance: a finite oversize line is rejected, not returned.
    #[tokio::test]
    async fn newline_finite_oversize_line_is_rejected() {
        let mut input = vec![b'x'; 10_000];
        input.push(b'\n');
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 1024);
        let err = t.read_message().await.unwrap_err();
        assert!(err.to_string().contains("exceeds maximum"), "{err}");
    }

    /// A line of exactly the cap (content + newline ≤ max) still reads fine.
    #[tokio::test]
    async fn newline_line_at_cap_is_accepted() {
        let body = "y".repeat(63); // 63 + '\n' = 64 = max
        let input = format!("{body}\n").into_bytes();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 64);
        assert_eq!(t.read_message().await.unwrap(), body);
    }

    /// Bounded reads must not eat into the *next* line when the current line
    /// fits: framing stays intact across messages.
    #[tokio::test]
    async fn bounded_read_preserves_framing_across_messages() {
        let input = b"{\"a\":1}\n{\"b\":2}\n".to_vec();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 8);
        assert_eq!(t.read_message().await.unwrap(), "{\"a\":1}");
        assert_eq!(t.read_message().await.unwrap(), "{\"b\":2}");
    }

    /// Non-UTF-8 bytes in a line are an InvalidMessage error, not a panic.
    #[tokio::test]
    async fn newline_invalid_utf8_is_invalid_message() {
        let input = [0xff, 0xfe, b'\n'];
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 64);
        let err = t.read_message().await.unwrap_err();
        assert!(err.to_string().contains("invalid UTF-8"), "{err}");
    }

    // --- A peer-declared length must never size an allocation, and a rejected
    // frame must never leave the stream desynchronised (terminal-mcp#4). ---

    thread_local! {
        /// Largest single allocation requested on this thread while a probe is
        /// armed, or `None` when none is. Scoping the measurement to one thread
        /// and one armed window is what makes it a *per-test* signal: a shared
        /// high-water mark would carry another test's allocations into this
        /// one's assertion (and this one's into the next test that ran later).
        ///
        /// `const`-initialised so reading it never allocates and registers no
        /// TLS destructor - otherwise recording an allocation could re-enter the
        /// allocator.
        static ALLOC_PROBE: Cell<Option<usize>> = const { Cell::new(None) };
    }

    fn record_alloc(size: usize) {
        // `try_with` because an allocation can happen while thread-locals are
        // being torn down, where `with` would panic.
        let _ = ALLOC_PROBE.try_with(|probe| {
            if let Some(largest) = probe.get() {
                probe.set(Some(largest.max(size)));
            }
        });
    }

    /// Records the largest single allocation made on the current thread for as
    /// long as it is alive, and stops on drop.
    ///
    /// Why a probe rather than a running total: `vec![0u8; n]` routes through
    /// `alloc_zeroed`, so a body buffer sized from a declared Content-Length
    /// shows up as one `n`-byte request even on a kernel that would lazily
    /// overcommit it instead of failing. `#[tokio::test]` polls the test future
    /// on the thread that armed the probe, so what it records is exactly the
    /// allocations of the code under test.
    struct AllocProbe;

    impl AllocProbe {
        fn arm() -> Self {
            ALLOC_PROBE.with(|probe| probe.set(Some(0)));
            Self
        }

        /// Largest single allocation seen since arming.
        fn largest(&self) -> usize {
            ALLOC_PROBE.with(|probe| probe.get().unwrap_or(0))
        }
    }

    impl Drop for AllocProbe {
        fn drop(&mut self) {
            ALLOC_PROBE.with(|probe| probe.set(None));
        }
    }

    /// Global allocator for the test binary: records the largest single request
    /// while a probe is armed on the requesting thread, and delegates everything
    /// to the system allocator.
    struct MaxTrackingAlloc;

    // SAFETY: every method forwards its unmodified `Layout` (and pointer, where
    // applicable) to `std::alloc::System` and returns System's result unchanged,
    // so this allocator inherits System's contract exactly. The only added work
    // is a `Cell` update behind a `const`-initialised thread-local, which
    // allocates nothing and cannot unwind, so no re-entrancy into the allocator
    // is possible.
    unsafe impl GlobalAlloc for MaxTrackingAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record_alloc(layout.size());
            // SAFETY: `layout` is the caller's, forwarded unchanged.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            record_alloc(layout.size());
            // SAFETY: `layout` is the caller's, forwarded unchanged.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: `ptr` came from one of our methods, i.e. from System with
            // this same `layout`; both are forwarded unchanged.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            record_alloc(new_size);
            // SAFETY: `ptr`/`layout` came from one of our methods (so from
            // System), and `new_size` is the caller's; all forwarded unchanged.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: MaxTrackingAlloc = MaxTrackingAlloc;

    /// Nothing on the framing path legitimately allocates a block this large, so
    /// a request above it inside an armed probe can only have come from trusting
    /// a declared length.
    const ALLOC_TRIPWIRE: usize = 256 * 1024 * 1024;

    /// Assert that the code under test allocated nothing near a declared length.
    ///
    /// `largest == 0` means the probe recorded nothing at all - the reads it was
    /// meant to watch happened somewhere it could not see - so it is a failure,
    /// not a pass. Without that check the bound would be vacuous.
    fn assert_no_declared_size_allocation(largest: usize, what: &str) {
        assert!(
            largest > 0,
            "the allocation probe recorded nothing, so it proves nothing about {what}"
        );
        assert!(
            largest < ALLOC_TRIPWIRE,
            "{what} must not be sized from a declared length; \
             largest single allocation was {largest} bytes"
        );
    }

    /// Unwrap the framing-error message, matching on the error *variant* rather
    /// than on its rendered text.
    fn framing_error(err: &crate::error::Error) -> String {
        match err {
            crate::error::Error::Transport(TransportError::InvalidMessage(msg)) => msg.clone(),
            other => panic!("expected TransportError::InvalidMessage, got {other:?}"),
        }
    }

    /// Read one message, failing the test rather than hanging if the read does
    /// not terminate (an unbounded read would otherwise wedge the harness).
    async fn read_bounded<R, W>(t: &mut FramedTransport<R, W>) -> Result<String>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        tokio::time::timeout(std::time::Duration::from_secs(2), t.read_message())
            .await
            .expect("read_message must terminate promptly")
    }

    /// Assert that the transport is poisoned: the next reads neither return a
    /// message nor merely run out of input, but say the stream was
    /// desynchronised. Several attempts, because one refusal could be a
    /// coincidence of where the input happened to end.
    async fn assert_poisoned<R, W>(t: &mut FramedTransport<R, W>)
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        for attempt in 0..3 {
            match read_bounded(t).await {
                Ok(msg) => panic!("read {attempt} after a refused frame returned a message: {msg}"),
                Err(err) => {
                    let reason = framing_error(&err);
                    assert!(
                        reason.contains("desynchronised"),
                        "read {attempt} must report the poisoned stream, got: {reason}"
                    );
                }
            }
        }
    }

    /// The allocation probe has to be believable before any bound can rest on
    /// it: armed, it sees a large single allocation made on its own thread.
    #[test]
    fn alloc_probe_records_the_largest_allocation_on_its_own_thread() {
        const BIG: usize = 16 * 1024 * 1024;
        let probe = AllocProbe::arm();
        let buf = std::hint::black_box(vec![0u8; BIG]);
        let largest = probe.largest();
        drop(buf);
        assert!(
            largest >= BIG,
            "probe must observe a {BIG}-byte allocation, saw {largest}"
        );
    }

    /// And unarmed it records nothing, so one test's allocations can never be
    /// read as another's.
    #[test]
    fn alloc_probe_records_nothing_once_dropped() {
        const BIG: usize = 16 * 1024 * 1024;
        drop(AllocProbe::arm());
        drop(std::hint::black_box(vec![0u8; BIG]));
        let probe = AllocProbe::arm();
        let largest = probe.largest();
        assert_eq!(largest, 0, "a dropped probe must stop recording");
    }

    /// Acceptance: an absurd declared length (~10 TB) is refused with an error
    /// naming the cap, and nothing near that size is ever allocated.
    #[tokio::test]
    async fn absurd_content_length_is_rejected_without_allocating() {
        let input = b"Content-Length: 9999999999999\r\n\r\n".to_vec();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 1024);
        let probe = AllocProbe::arm();
        let result = read_bounded(&mut t).await;
        let largest = probe.largest();
        drop(probe);
        // Asserted before the outcome: sizing a buffer from the declaration is
        // the failure this test exists to catch, so it must be what fails.
        assert_no_declared_size_allocation(largest, "a refused frame");
        let err = result.expect_err("an absurd declared length must not yield a message");
        let msg = framing_error(&err);
        assert!(msg.contains("9999999999999"), "must name the length: {msg}");
        assert!(msg.contains("exceeds maximum"), "must name the cap: {msg}");
    }

    /// Acceptance: a declared length too large for `usize` is a framing
    /// violation, not a header the parser quietly stops recognising — otherwise
    /// framing silently degrades to newline mode and the frame body is handed
    /// back to the caller as messages.
    #[tokio::test]
    async fn content_length_overflowing_usize_is_rejected() {
        let input = b"Content-Length: 99999999999999999999999999999999\r\n\r\n{}".to_vec();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 1024);
        let err = read_bounded(&mut t)
            .await
            .expect_err("an out-of-range declared length must not yield a message");
        let msg = framing_error(&err);
        assert!(msg.contains("Content-Length"), "{msg}");
    }

    /// Acceptance: one byte over the cap is refused.
    #[tokio::test]
    async fn content_length_just_over_cap_is_rejected() {
        const MAX: usize = 1024;
        let input = format!("Content-Length: {}\r\n\r\n", MAX + 1).into_bytes();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), MAX);
        let err = read_bounded(&mut t)
            .await
            .expect_err("a length one byte over the cap must be refused");
        let msg = framing_error(&err);
        assert!(
            msg.contains("1025") && msg.contains("1024"),
            "must name both the length and the cap: {msg}"
        );
    }

    /// Acceptance: one byte under the cap is accepted, body intact.
    #[tokio::test]
    async fn content_length_just_under_cap_is_accepted() {
        const MAX: usize = 1024;
        let body = "y".repeat(MAX - 1);
        let input = format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), MAX);
        assert_eq!(
            read_bounded(&mut t)
                .await
                .expect("a length under the cap must be accepted"),
            body
        );
    }

    /// Acceptance: exactly the cap is accepted — the bound is inclusive.
    #[tokio::test]
    async fn content_length_exactly_at_cap_is_accepted() {
        const MAX: usize = 1024;
        let body = "z".repeat(MAX);
        let input = format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), MAX);
        assert_eq!(
            read_bounded(&mut t)
                .await
                .expect("a length exactly at the cap must be accepted"),
            body
        );
    }

    /// Acceptance: a non-numeric length is a framing violation.
    #[tokio::test]
    async fn non_numeric_content_length_is_rejected() {
        let input = b"Content-Length: not-a-number\r\n\r\n{}".to_vec();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 1024);
        let err = read_bounded(&mut t)
            .await
            .expect_err("a non-numeric length must not yield a message");
        let msg = framing_error(&err);
        assert!(msg.contains("Content-Length"), "{msg}");
    }

    /// Acceptance: a negative length is a framing violation (and never wraps).
    #[tokio::test]
    async fn negative_content_length_is_rejected() {
        let input = b"Content-Length: -1\r\n\r\n{}".to_vec();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 1024);
        let err = read_bounded(&mut t)
            .await
            .expect_err("a negative length must not yield a message");
        let msg = framing_error(&err);
        assert!(msg.contains("Content-Length"), "{msg}");
    }

    /// Acceptance: an empty length is a framing violation.
    #[tokio::test]
    async fn empty_content_length_is_rejected() {
        let input = b"Content-Length:\r\n\r\n{}".to_vec();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 1024);
        let err = read_bounded(&mut t)
            .await
            .expect_err("an empty length must not yield a message");
        let msg = framing_error(&err);
        assert!(msg.contains("Content-Length"), "{msg}");
    }

    /// Acceptance: a zero-length body is legal and yields an empty message.
    #[tokio::test]
    async fn zero_content_length_yields_empty_message() {
        let input = b"Content-Length: 0\r\n\r\n".to_vec();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 1024);
        assert_eq!(
            read_bounded(&mut t)
                .await
                .expect("a zero-length body is well-framed"),
            ""
        );
    }

    /// Acceptance: the body buffer must track bytes actually delivered, not the
    /// declared length — a peer that declares a cap-sized frame and then sends
    /// ten bytes must not make the server reserve the whole cap.
    ///
    /// The cap here is deliberately above [`ALLOC_TRIPWIRE`] so that sizing the
    /// buffer from the declaration trips the probe armed around the read.
    #[tokio::test]
    async fn body_allocation_tracks_delivered_bytes_not_declared_length() {
        const MAX: usize = 512 * 1024 * 1024;
        let input = format!("Content-Length: {MAX}\r\n\r\nshort").into_bytes();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), MAX);
        let probe = AllocProbe::arm();
        let result = read_bounded(&mut t).await;
        let largest = probe.largest();
        drop(probe);
        // Asserted before the outcome: a buffer sized from the declaration is
        // the failure this test exists to catch, so it must be what fails.
        assert_no_declared_size_allocation(largest, "the body buffer");
        let err = result.expect_err("a body shorter than declared is not a message");
        let msg = framing_error(&err);
        assert!(
            msg.contains("body") && msg.contains("5"),
            "must say how much of the body arrived: {msg}"
        );
    }

    /// Acceptance: the refusal quotes at most a snippet of the declared value.
    /// A declared length is peer text and can be padded with leading zeros to
    /// any length the cap allows, and the refusal travels into logs and back to
    /// the peer — it must not carry kilobytes of someone else's bytes with it.
    #[tokio::test]
    async fn oversize_content_length_error_truncates_the_declared_value() {
        const MAX: usize = 8192;
        let padded = format!("{}{}", "0".repeat(4096), MAX + 1);
        let input = format!("Content-Length: {padded}\r\n\r\n").into_bytes();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), MAX);
        let err = read_bounded(&mut t)
            .await
            .expect_err("a padded oversize length must still be refused");
        let msg = framing_error(&err);
        assert!(msg.contains("exceeds maximum"), "{msg}");
        assert!(
            msg.len() < 256,
            "refusal must not echo the padding ({} bytes): {msg}",
            msg.len()
        );
    }

    /// Acceptance: a flood of header lines is refused. Each line is individually
    /// bounded, but the header loop itself must be bounded too, so one frame
    /// cannot cost unbounded work.
    #[tokio::test]
    async fn frame_with_header_flood_is_rejected() {
        let mut input = String::from("Content-Length: 2\r\n");
        for i in 0..1000 {
            input.push_str(&format!("X-Pad-{i}: pad\r\n"));
        }
        input.push_str("\r\n{}");
        let mut t = FramedTransport::new(BufReader::new(input.as_bytes()), Vec::new(), 1024);
        let err = read_bounded(&mut t)
            .await
            .expect_err("a header flood must not yield a message");
        let msg = framing_error(&err);
        assert!(msg.contains("header"), "must name the header limit: {msg}");
    }

    /// Acceptance: a frame hidden behind a refused one is never dispatched. The
    /// refused body was never consumed, so where the next frame starts is
    /// unknown and every later read has to fail rather than parse peer-placed
    /// bytes.
    #[tokio::test]
    async fn refused_oversize_content_length_is_not_followed_by_a_smuggled_message() {
        let smuggled = r#"{"jsonrpc":"2.0","id":1,"method":"smuggled"}"#;
        let input = format!(
            "Content-Length: 9999999999\r\n\r\nContent-Length: {}\r\n\r\n{smuggled}",
            smuggled.len()
        )
        .into_bytes();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 1024);
        let err = read_bounded(&mut t)
            .await
            .expect_err("the oversize frame must be refused");
        assert!(framing_error(&err).contains("exceeds maximum"));
        assert_poisoned(&mut t).await;
    }

    /// Acceptance: the same holds for newline framing. Only `max_len + 1` bytes
    /// of the oversize line were consumed, so the rest of that line - and
    /// anything a peer hid behind it - must not come back as a message.
    #[tokio::test]
    async fn refused_oversize_line_is_not_followed_by_a_smuggled_message() {
        const MAX: usize = 1024;
        let smuggled = r#"{"jsonrpc":"2.0","id":1,"method":"smuggled"}"#;
        // Exactly `MAX + 1` newline-free bytes: the read stops right at the cap,
        // leaving the newline and the following line in the stream.
        let mut input = vec![b'x'; MAX + 1];
        input.push(b'\n');
        input.extend_from_slice(smuggled.as_bytes());
        input.push(b'\n');
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), MAX);
        let err = read_bounded(&mut t)
            .await
            .expect_err("the oversize line must be refused");
        assert!(framing_error(&err).contains("exceeds maximum"));
        assert_poisoned(&mut t).await;
    }
}
