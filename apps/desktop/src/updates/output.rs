//! The installer's output as the app shows it: decoded across read
//! boundaries, without terminal escapes, with carriage returns redrawing the
//! line, and capped to its last lines.

const MAX_ESCAPE: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escape {
    None,
    Start,
    Csi,
    Osc,
    OscEnd,
}

#[derive(Debug)]
pub struct Log {
    text: String,
    line_start: usize,
    partial: Vec<u8>,
    escape: Escape,
    escape_len: usize,
    carriage_return: bool,
    cap: usize,
}

impl Log {
    pub fn new(cap: usize) -> Self {
        Self {
            text: String::new(),
            line_start: 0,
            partial: Vec::new(),
            escape: Escape::None,
            escape_len: 0,
            carriage_return: false,
            cap,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn push(&mut self, bytes: &[u8]) {
        let mut input = std::mem::take(&mut self.partial);
        input.extend_from_slice(bytes);
        let mut rest = input.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    self.chars(valid);
                    break;
                }
                Err(err) => {
                    let (valid, after) = rest.split_at(err.valid_up_to());
                    // SAFETY: `valid_up_to` bytes were just validated.
                    self.chars(unsafe { std::str::from_utf8_unchecked(valid) });
                    match err.error_len() {
                        Some(len) => {
                            self.chars("\u{FFFD}");
                            rest = &after[len..];
                        }
                        None => {
                            self.partial = after.to_vec();
                            break;
                        }
                    }
                }
            }
        }
        self.trim();
    }

    /// The stream ended: an incomplete character is shown as replaced.
    pub fn finish(&mut self) {
        if !std::mem::take(&mut self.partial).is_empty() {
            self.chars("\u{FFFD}");
            self.trim();
        }
    }

    fn chars(&mut self, text: &str) {
        for c in text.chars() {
            self.char(c);
        }
    }

    fn char(&mut self, c: char) {
        if self.escape != Escape::None {
            self.escape_len += 1;
            self.escape = match (self.escape, c) {
                _ if self.escape_len > MAX_ESCAPE => Escape::None,
                (Escape::Start, '[') => Escape::Csi,
                (Escape::Start, ']') => Escape::Osc,
                (Escape::Csi, '@'..='~') => Escape::None,
                (Escape::Osc, '\u{7}') => Escape::None,
                (Escape::Osc | Escape::OscEnd, '\u{1b}') => Escape::OscEnd,
                (Escape::OscEnd, '\\') => Escape::None,
                (Escape::Csi, _) => Escape::Csi,
                (Escape::Osc | Escape::OscEnd, _) => Escape::Osc,
                (Escape::Start | Escape::None, _) => Escape::None,
            };
            return;
        }
        match c {
            '\u{1b}' => {
                self.escape = Escape::Start;
                self.escape_len = 0;
            }
            '\r' => self.carriage_return = true,
            '\n' => {
                self.carriage_return = false;
                self.text.push('\n');
                self.line_start = self.text.len();
            }
            c if c.is_control() && c != '\t' => {}
            c => {
                if std::mem::take(&mut self.carriage_return) {
                    self.text.truncate(self.line_start);
                }
                self.text.push(c);
            }
        }
    }

    /// Keep the last `cap` bytes, starting at a whole line when there is one.
    fn trim(&mut self) {
        if self.text.len() <= self.cap {
            return;
        }
        let mut cut = self.text.len() - self.cap;
        while !self.text.is_char_boundary(cut) {
            cut += 1;
        }
        if self.text.as_bytes()[cut - 1] != b'\n'
            && let Some(newline) = self.text[cut..].find('\n')
            && cut + newline + 1 < self.text.len()
        {
            cut += newline + 1;
        }
        self.text.drain(..cut);
        self.line_start = self.line_start.saturating_sub(cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(chunks: &[&[u8]], cap: usize) -> String {
        let mut log = Log::new(cap);
        for chunk in chunks {
            log.push(chunk);
        }
        log.finish();
        log.text().to_owned()
    }

    #[test]
    fn characters_split_across_reads_are_kept() {
        let text = "✓ built\n".as_bytes();
        assert_eq!(
            render(&[&text[..1], &text[1..2], &text[2..]], 1024),
            "✓ built\n"
        );
        assert_eq!(render(&[b"ok \xe2\x9c"], 1024), "ok \u{FFFD}");
        assert_eq!(render(&[b"a\xffb"], 1024), "a\u{FFFD}b");
    }

    #[test]
    fn terminal_escapes_are_removed_even_when_split() {
        let colored =
            b"    \x1b[32m\xe2\x9c\x93\x1b[0m done\n\x1b]0;title\x07next\x1b]8;;u\x1b\\link\n";
        assert_eq!(render(&[colored], 1024), "    ✓ done\nnextlink\n");
        assert_eq!(render(&[b"a\x1b", b"[1", b"mb\n"], 1024), "ab\n");
    }

    #[test]
    fn carriage_returns_redraw_the_line() {
        assert_eq!(render(&[b"10%\r50%\r", b"done\n"], 1024), "done\n");
        assert_eq!(render(&[b"line\r", b"\nnext\r\n"], 1024), "line\nnext\n");
        assert_eq!(render(&[b"keep\nbar 1\rbar 2"], 1024), "keep\nbar 2");
    }

    #[test]
    fn the_log_keeps_its_last_whole_lines() {
        let mut log = Log::new(16);
        for n in 0..100 {
            log.push(format!("line {n}\n").as_bytes());
        }
        assert!(log.text().len() <= 16, "{:?}", log.text());
        assert_eq!(log.text(), "line 98\nline 99\n");
        let mut wide = Log::new(5);
        wide.push("ééééé\r".as_bytes());
        wide.push(b"x");
        assert!(
            wide.text().len() <= 5 && wide.text().ends_with('x'),
            "{:?}",
            wide.text()
        );
    }
}
