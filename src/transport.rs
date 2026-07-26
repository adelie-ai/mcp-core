//! Framed JSON-RPC transport over any byte stream (stdio, unix socket).
//!
//! Supports both newline-delimited JSON and LSP-style
//! `Content-Length: N\r\n\r\n<bytes>` framing, auto-detected from the first
//! line. A configurable size cap (`max_len`) bounds memory in both modes:
//! Content-Length is checked against the cap *before* the body buffer is
//! allocated, and newline lines are read incrementally and rejected after at
//! most `max_len + 1` bytes — so a peer can't trigger an OOM with a huge
//! `Content-Length` *or* an endless newline-free line.

use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, Stdin,
    Stdout,
};

use crate::error::{Result, TransportError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    Auto,
    Newline,
    ContentLength,
}

fn trim_crlf(s: &str) -> &str {
    s.trim_end_matches(['\r', '\n'])
}

fn parse_content_length_header(line: &str) -> Option<usize> {
    let (name, value) = trim_crlf(line).trim().split_once(':')?;
    if !name.trim().eq_ignore_ascii_case("content-length") {
        return None;
    }
    value.trim().parse::<usize>().ok()
}

/// A framed transport over a buffered reader and a writer.
pub struct FramedTransport<R, W> {
    reader: R,
    writer: W,
    framing: Framing,
    max_len: usize,
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
        }
    }

    fn check_len(&self, len: usize, what: &str) -> Result<()> {
        if len > self.max_len {
            return Err(TransportError::InvalidMessage(format!(
                "{what} {len} exceeds maximum of {} bytes",
                self.max_len
            ))
            .into());
        }
        Ok(())
    }

    /// Read one JSON-RPC message. Returns
    /// `Err(TransportError::ConnectionClosed)` on clean EOF.
    pub async fn read_message(&mut self) -> Result<String> {
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
            if parse_content_length_header(trimmed).is_some() {
                self.framing = Framing::ContentLength;
                return self.read_content_length(trimmed).await;
            }
            self.framing = Framing::Newline;
            return Ok(trimmed.to_string());
        }
    }

    async fn read_content_length(&mut self, first: &str) -> Result<String> {
        let content_length = parse_content_length_header(first).ok_or_else(|| {
            TransportError::InvalidMessage(format!("expected Content-Length header, got: {first}"))
        })?;
        // Cap before allocating the body buffer.
        self.check_len(content_length, "Content-Length")?;

        // Consume any remaining headers up to the blank line.
        loop {
            let line = self.read_line().await?;
            if trim_crlf(&line).is_empty() {
                break;
            }
        }

        let mut buf = vec![0u8; content_length];
        self.reader.read_exact(&mut buf).await?;
        String::from_utf8(buf)
            .map_err(|e| TransportError::InvalidMessage(format!("invalid UTF-8: {e}")).into())
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn parses_content_length() {
        assert_eq!(
            parse_content_length_header("Content-Length: 10\r\n"),
            Some(10)
        );
        assert_eq!(parse_content_length_header("content-length:  0"), Some(0));
        assert_eq!(parse_content_length_header("Content-Type: x"), None);
        assert_eq!(parse_content_length_header("garbage"), None);
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

    /// Largest single allocation the test binary has requested. `vec![0u8; n]`
    /// routes through `alloc_zeroed`, so a body buffer sized from a declared
    /// Content-Length shows up here as an `n`-byte request even on a kernel that
    /// would lazily overcommit it instead of failing.
    static LARGEST_ALLOC: AtomicUsize = AtomicUsize::new(0);

    fn record_alloc(size: usize) {
        LARGEST_ALLOC.fetch_max(size, Ordering::Relaxed);
    }

    /// Global allocator for the test binary: records the largest single request
    /// and delegates everything to the system allocator.
    struct MaxTrackingAlloc;

    // SAFETY: every method forwards its unmodified `Layout` (and pointer, where
    // applicable) to `std::alloc::System` and returns System's result unchanged,
    // so this allocator inherits System's contract exactly. The only added work
    // is a relaxed atomic max, which allocates nothing and cannot unwind, so no
    // re-entrancy into the allocator is possible.
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

    /// No test in this binary legitimately allocates a block this large, so any
    /// request above it can only have come from trusting a declared length.
    const ALLOC_TRIPWIRE: usize = 256 * 1024 * 1024;

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

    /// Acceptance: an absurd declared length (~10 TB) is refused with an error
    /// naming the cap, and nothing near that size is ever allocated.
    #[tokio::test]
    async fn absurd_content_length_is_rejected_without_allocating() {
        let input = b"Content-Length: 9999999999999\r\n\r\n".to_vec();
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 1024);
        let err = read_bounded(&mut t)
            .await
            .expect_err("an absurd declared length must not yield a message");
        let msg = framing_error(&err);
        assert!(msg.contains("9999999999999"), "must name the length: {msg}");
        assert!(msg.contains("exceeds maximum"), "must name the cap: {msg}");
        let largest = LARGEST_ALLOC.load(Ordering::Relaxed);
        assert!(
            largest < ALLOC_TRIPWIRE,
            "a declared length must never size an allocation; largest was {largest} bytes"
        );
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

    /// Acceptance: refusing an oversize frame must not desynchronise the stream.
    /// The refused body was never consumed, so its bytes are not messages — a
    /// peer must not be able to smuggle a frame in behind a rejected header.
    #[tokio::test]
    async fn rejected_oversize_content_length_does_not_desynchronise_stream() {
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
        for attempt in 0..3 {
            if let Ok(msg) = read_bounded(&mut t).await {
                panic!("read {attempt} after a refused frame returned a message: {msg}");
            }
        }
    }

    /// Acceptance: the same holds for newline framing. Only `max_len + 1` bytes
    /// of the oversize line were consumed, so the rest of that line - and
    /// anything a peer hid behind it - must not come back as a message.
    #[tokio::test]
    async fn rejected_oversize_line_does_not_desynchronise_stream() {
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
        for attempt in 0..3 {
            if let Ok(msg) = read_bounded(&mut t).await {
                panic!("read {attempt} after a refused line returned a message: {msg}");
            }
        }
    }
}
