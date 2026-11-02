//! Incremental Server-Sent Events parser over raw bytes.
//!
//! Network chunks can split a frame or a multi-byte character anywhere, so
//! bytes are buffered and only decoded once a complete frame (terminated by a
//! blank line) is present. Comment lines (heartbeats) are skipped.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub id: Option<String>,
    pub event: String,
    pub data: String,
}

#[derive(Default)]
pub struct Parser {
    buffer: Vec<u8>,
}

/// A frame that is not valid UTF-8 means the stream is corrupt.
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidFrame;

const MAX_BUFFER: usize = 4 * 1024 * 1024;

impl Parser {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, InvalidFrame> {
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() > MAX_BUFFER {
            return Err(InvalidFrame);
        }
        let mut frames = Vec::new();
        loop {
            // Accept LF and CRLF line endings.
            let Some((end, sep)) = find_frame_end(&self.buffer) else {
                break;
            };
            let raw: Vec<u8> = self.buffer.drain(..end + sep).collect();
            let text = std::str::from_utf8(&raw[..end]).map_err(|_| InvalidFrame)?;
            let (mut id, mut event, mut data) = (None, String::new(), Vec::new());
            for line in text.split('\n') {
                let line = line.strip_suffix('\r').unwrap_or(line);
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }
                let (field, value) = line.split_once(':').unwrap_or((line, ""));
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "id" => id = Some(value.to_owned()),
                    "event" => event = value.to_owned(),
                    "data" => data.push(value.to_owned()),
                    _ => {}
                }
            }
            if !data.is_empty() || !event.is_empty() {
                frames.push(Frame {
                    id,
                    event: if event.is_empty() {
                        "message".into()
                    } else {
                        event
                    },
                    data: data.join("\n"),
                });
            }
        }
        Ok(frames)
    }
}

fn find_frame_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2));
    let crlf = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: &str = "id: i:b:7\nevent: item.delta\ndata: {\"text\":\"✓é日本\"}\n\n";

    #[test]
    fn every_split_point_yields_the_same_frame() {
        let bytes = FRAME.as_bytes();
        for split in 0..=bytes.len() {
            let mut parser = Parser::default();
            let mut frames = parser.push(&bytes[..split]).unwrap();
            frames.extend(parser.push(&bytes[split..]).unwrap());
            assert_eq!(frames.len(), 1, "split at {split}");
            assert_eq!(frames[0].id.as_deref(), Some("i:b:7"));
            assert_eq!(frames[0].event, "item.delta");
            assert_eq!(frames[0].data, "{\"text\":\"✓é日本\"}");
        }
    }

    #[test]
    fn byte_at_a_time_and_heartbeats() {
        let stream = format!(": keep-alive\n\n{FRAME}: ping\n\n{FRAME}");
        let mut parser = Parser::default();
        let mut frames = Vec::new();
        for byte in stream.as_bytes() {
            frames.extend(parser.push(std::slice::from_ref(byte)).unwrap());
        }
        assert_eq!(frames.len(), 2);
    }

    #[test]
    fn incomplete_frames_wait_and_crlf_works() {
        let mut parser = Parser::default();
        assert!(
            parser
                .push(b"id: 1\r\nevent: x\r\ndata: a")
                .unwrap()
                .is_empty()
        );
        let frames = parser.push(b"\r\n\r\n").unwrap();
        assert_eq!(frames[0].data, "a");
        assert_eq!(parser.push(b"data: \xff\n\n"), Err(InvalidFrame));
    }
}
