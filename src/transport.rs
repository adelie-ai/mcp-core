//! Framed JSON-RPC transport over any byte stream (stdio, unix socket).
//!
//! Supports both newline-delimited JSON and LSP-style
//! `Content-Length: N\r\n\r\n<bytes>` framing, auto-detected from the first
//! line. A configurable size cap is enforced *before* allocating, so a peer
//! can't trigger an OOM with a huge `Content-Length` (or an endless line).

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

    async fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(TransportError::ConnectionClosed.into());
        }
        // Bound newline-mode lines so an endless line can't exhaust memory.
        self.check_len(line.len(), "line length")?;
        Ok(line)
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
            if self.chunks % 8 == 0 {
                // Yield so the runtime can poll other tasks (e.g. the timeout).
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
            let n = buf.remaining().min(4096);
            buf.put_slice(&vec![b'a'; n]);
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
        let input = vec![0xff, 0xfe, b'\n'];
        let mut t = FramedTransport::new(BufReader::new(&input[..]), Vec::new(), 64);
        let err = t.read_message().await.unwrap_err();
        assert!(err.to_string().contains("invalid UTF-8"), "{err}");
    }
}
