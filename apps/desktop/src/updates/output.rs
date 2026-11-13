/// Characters a terminal escape may run before the log gives up on it and
/// shows what follows, so a broken sequence never hides the rest.
const MAX_ESCAPE: usize = 256;

pub struct Log {
    parser: vte::Parser,
    screen: Screen,
    partial: Vec<u8>,
    cap: usize,
}

#[derive(Default)]
struct Screen {
    text: String,
    line_start: usize,
    carriage_return: bool,
    pending: usize,
}

impl vte::Perform for Screen {
    fn print(&mut self, c: char) {
        self.pending = 0;
        if !c.is_control() {
            self.put(c);
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.carriage_return = true,
            b'\n' => {
                self.carriage_return = false;
                self.text.push('\n');
                self.line_start = self.text.len();
            }
            b'\t' => self.put('\t'),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, _: &vte::Params, _: &[u8], _: bool, _: char) {
        self.pending = 0;
    }

    fn esc_dispatch(&mut self, _: &[u8], _: bool, _: u8) {
        self.pending = 0;
    }

    fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {
        self.pending = 0;
    }

    fn unhook(&mut self) {
        self.pending = 0;
    }
}

impl Screen {
    fn put(&mut self, c: char) {
        if std::mem::take(&mut self.carriage_return) {
            self.text.truncate(self.line_start);
        }
        self.text.push(c);
    }
}

impl Log {
    pub fn new(cap: usize) -> Self {
        Self {
            parser: vte::Parser::new(),
            screen: Screen::default(),
            partial: Vec::new(),
            cap,
        }
    }

    pub fn text(&self) -> &str {
        &self.screen.text
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
            self.screen.pending += 1;
            self.parser
                .advance(&mut self.screen, c.encode_utf8(&mut [0; 4]).as_bytes());
            if self.screen.pending > MAX_ESCAPE {
                self.parser = vte::Parser::new();
                self.screen.pending = 0;
            }
        }
    }

    fn trim(&mut self) {
        let screen = &mut self.screen;
        if screen.text.len() <= self.cap {
            return;
        }
        let mut cut = screen.text.len() - self.cap;
        while !screen.text.is_char_boundary(cut) {
            cut += 1;
        }
        if screen.text.as_bytes()[cut - 1] != b'\n'
            && let Some(newline) = screen.text[cut..].find('\n')
            && cut + newline + 1 < screen.text.len()
        {
            cut += newline + 1;
        }
        screen.text.drain(..cut);
        screen.line_start = screen.line_start.saturating_sub(cut);
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
        assert_eq!(
            render(&[b"\x1b[1mbold\x1b(B\x1b[m plain\n"], 1024),
            "bold plain\n"
        );
        assert_eq!(render(&[b"a\x1bP1$r\x1b\\b\n"], 1024), "ab\n");
    }

    #[test]
    fn an_unterminated_escape_stops_hiding_text_after_its_limit() {
        let mut stream = b"start\n\x1b]0;".to_vec();
        stream.extend(std::iter::repeat_n(b'x', MAX_ESCAPE));
        stream.extend(b"\nvisible\n");
        let shown = render(&[&stream], 4096);
        assert!(shown.starts_with("start\n"), "{shown:?}");
        assert!(shown.ends_with("visible\n"), "{shown:?}");
    }

    #[test]
    fn line_breaks_inside_an_unfinished_escape_count_toward_its_limit() {
        let mut stream = b"\x1b[".to_vec();
        for _ in 0..100 {
            stream.extend(b"1234\n");
        }
        stream.extend(b"VISIBLE\n");
        let shown = render(&[&stream], 4096);
        assert!(shown.ends_with("VISIBLE\n"), "{shown:?}");
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
