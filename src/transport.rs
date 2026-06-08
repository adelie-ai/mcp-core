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
}
