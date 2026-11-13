//! Bounded JSON lines from a provider CLI.

use futures_util::StreamExt as _;
use serde_json::Value;
use tokio::io::AsyncRead;
use tokio_util::codec::{AnyDelimiterCodec, AnyDelimiterCodecError, FramedRead};

pub const MAX_LINE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LineError {
    #[error("{0} sent an oversized event")]
    Oversized(&'static str),
    #[error("{0} sent malformed output: {1}")]
    Malformed(&'static str, String),
    #[error("reading {0}'s output failed: {1}")]
    Io(&'static str, String),
}

/// Reads one JSON value per line, at most [`MAX_LINE`] bytes each. Lines are
/// split as bytes, so a line that is not UTF-8 is malformed JSON rather than
/// the end of the stream. The frame buffer keeps partially read bytes, so a
/// read abandoned by a `select!` (a timer or cancellation) loses nothing.
pub struct JsonLines<R> {
    frames: FramedRead<R, AnyDelimiterCodec>,
    label: &'static str,
}

impl<R: AsyncRead + Unpin> JsonLines<R> {
    pub fn new(reader: R, label: &'static str) -> Self {
        let codec = AnyDelimiterCodec::new_with_max_length(b"\n".to_vec(), Vec::new(), MAX_LINE);
        Self {
            frames: FramedRead::new(reader, codec),
            label,
        }
    }

    /// The next value; `None` at the end of the output. A final line without
    /// a newline still counts.
    pub async fn next(&mut self) -> Result<Option<Value>, LineError> {
        loop {
            let line = match self.frames.next().await {
                None => return Ok(None),
                Some(Ok(line)) => line,
                Some(Err(AnyDelimiterCodecError::MaxChunkLengthExceeded)) => {
                    return Err(LineError::Oversized(self.label));
                }
                Some(Err(AnyDelimiterCodecError::Io(e))) => {
                    return Err(LineError::Io(self.label, e.to_string()));
                }
            };
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return serde_json::from_slice(&line)
                .map(Some)
                .map_err(|e| LineError::Malformed(self.label, e.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn an_abandoned_read_keeps_its_partial_line() {
        let (mut write, read) = tokio::io::duplex(64);
        let mut lines = JsonLines::new(read, "Test");
        write.write_all(b"{\"a\":").await.unwrap();
        let abandoned =
            tokio::time::timeout(std::time::Duration::from_millis(20), lines.next()).await;
        assert!(abandoned.is_err(), "no full line yet");
        write.write_all(b"1}\n\n{\"b\":2}").await.unwrap();
        drop(write);
        assert_eq!(lines.next().await, Ok(Some(serde_json::json!({"a": 1}))));
        assert_eq!(lines.next().await, Ok(Some(serde_json::json!({"b": 2}))));
        assert_eq!(lines.next().await, Ok(None));
    }

    #[tokio::test]
    async fn malformed_and_oversized_lines_are_errors() {
        let mut data = b"not json\n".to_vec();
        data.extend(std::iter::repeat_n(b'x', MAX_LINE + 1));
        let mut lines = JsonLines::new(&data[..], "Test");
        assert!(matches!(
            lines.next().await,
            Err(LineError::Malformed("Test", _))
        ));
        assert_eq!(lines.next().await, Err(LineError::Oversized("Test")));
    }

    #[tokio::test]
    async fn a_line_that_is_not_utf8_is_malformed_and_reading_goes_on() {
        let data = b"{\"a\":\"\xff\"}\r\n{\"b\":2}\r\n";
        let mut lines = JsonLines::new(&data[..], "Test");
        assert!(matches!(
            lines.next().await,
            Err(LineError::Malformed("Test", _))
        ));
        assert_eq!(lines.next().await, Ok(Some(serde_json::json!({"b": 2}))));
        assert_eq!(lines.next().await, Ok(None));
    }
}
