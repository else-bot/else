//! Bounded JSON lines from a provider CLI.

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

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

/// Reads one JSON value per line, at most [`MAX_LINE`] bytes each. Partially
/// read bytes stay in `buf`, so a read abandoned by a `select!` (a timer or
/// cancellation) loses nothing: the next call continues the same line.
pub struct JsonLines<R> {
    reader: R,
    buf: Vec<u8>,
    label: &'static str,
}

impl<R: AsyncBufRead + Unpin> JsonLines<R> {
    pub fn new(reader: R, label: &'static str) -> Self {
        Self {
            reader,
            buf: Vec::new(),
            label,
        }
    }

    /// The next value; `None` at the end of the output. A final line without
    /// a newline still counts.
    pub async fn next(&mut self) -> Result<Option<Value>, LineError> {
        loop {
            let room = MAX_LINE.saturating_sub(self.buf.len()) as u64;
            if room == 0 {
                self.buf.clear();
                return Err(LineError::Oversized(self.label));
            }
            let read = (&mut self.reader)
                .take(room)
                .read_until(b'\n', &mut self.buf)
                .await
                .map_err(|e| LineError::Io(self.label, e.to_string()))?;
            if read == 0 && self.buf.is_empty() {
                return Ok(None);
            }
            if read != 0 && !self.buf.ends_with(b"\n") {
                continue;
            }
            let line = std::mem::take(&mut self.buf);
            if line.iter().all(u8::is_ascii_whitespace) {
                if read == 0 {
                    return Ok(None);
                }
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
    use tokio::io::{AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn an_abandoned_read_keeps_its_partial_line() {
        let (mut write, read) = tokio::io::duplex(64);
        let mut lines = JsonLines::new(BufReader::new(read), "Test");
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
        let mut lines = JsonLines::new(BufReader::new(&data[..]), "Test");
        assert!(matches!(
            lines.next().await,
            Err(LineError::Malformed("Test", _))
        ));
        assert_eq!(lines.next().await, Err(LineError::Oversized("Test")));
    }
}
