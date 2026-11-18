//! Find text the way a reader sees it: case, accents, and emphasis marks
//! ignored, and any run of spaces, line breaks, or table bars read as one
//! space.

use std::ops::Range;

use unicode_normalization::char::{decompose_canonical, is_combining_mark};

/// Marks a reader does not see as letters, dropped from both the text and
/// the query.
const IGNORED: [char; 5] = ['*', '_', '~', '`', '\\'];

/// A folded query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needle(Vec<char>);

impl Needle {
    /// `None` when nothing searchable is left after folding.
    pub fn new(query: &str) -> Option<Self> {
        let folded = Folded::new(query).chars;
        (!folded.is_empty()).then_some(Self(folded))
    }

    /// Where the needle shows in `text`, as byte ranges in order that do
    /// not overlap.
    pub fn find(&self, text: &str) -> Vec<Range<usize>> {
        let folded = Folded::new(text);
        let mut found = Vec::new();
        let mut at = 0;
        while at + self.0.len() <= folded.chars.len() {
            if folded.chars[at..at + self.0.len()] == self.0[..] {
                let end = at + self.0.len() - 1;
                found.push(folded.from[at].start..folded.from[end].end);
                at += self.0.len();
            } else {
                at += 1;
            }
        }
        found
    }

    /// How often the needle shows in `markdown` once rendered, read from a
    /// cheap skim of its syntax rather than a full parse.
    pub fn count_in_markdown(&self, markdown: &str) -> usize {
        self.find(&skim(markdown)).len()
    }
}

/// `text` folded, with the source bytes of each folded character.
struct Folded {
    chars: Vec<char>,
    from: Vec<Range<usize>>,
}

impl Folded {
    fn new(text: &str) -> Self {
        let mut folded = Self {
            chars: Vec::with_capacity(text.len()),
            from: Vec::with_capacity(text.len()),
        };
        let mut space = None;
        for (start, c) in text.char_indices() {
            let source = start..start + c.len_utf8();
            if c.is_whitespace() || c == '|' {
                if !folded.chars.is_empty() {
                    space.get_or_insert(source);
                }
                continue;
            }
            if IGNORED.contains(&c) {
                continue;
            }
            for lower in c.to_lowercase() {
                decompose_canonical(lower, |base| {
                    if is_combining_mark(base) {
                        return;
                    }
                    if let Some(space) = space.take() {
                        folded.chars.push(' ');
                        folded.from.push(space);
                    }
                    folded.chars.push(base);
                    folded.from.push(source.clone());
                });
            }
        }
        folded
    }
}

/// Markdown with the syntax a reader never sees taken out: fence and table
/// rule lines, heading, quote, and list markers, link and image targets,
/// HTML tags, and common entities. Inline marks are left to the fold.
fn skim(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    for line in markdown.lines() {
        let line = line.trim_start();
        if line.starts_with("```") || line.starts_with("~~~") || is_table_rule(line) {
            continue;
        }
        skim_inline(strip_markers(line), &mut out);
        out.push('\n');
    }
    out
}

fn is_table_rule(line: &str) -> bool {
    line.contains('-') && line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

fn strip_markers(mut line: &str) -> &str {
    loop {
        let rest = line.trim_start();
        let hashes = rest.len() - rest.trim_start_matches('#').len();
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let next = if (1..=6).contains(&hashes) && rest[hashes..].starts_with(' ') {
            &rest[hashes + 1..]
        } else if let Some(after) = rest.strip_prefix('>') {
            after
        } else if let Some(after) = ["- ", "* ", "+ "]
            .iter()
            .find_map(|marker| rest.strip_prefix(marker))
        {
            ["[ ] ", "[x] ", "[X] "]
                .iter()
                .find_map(|task| after.strip_prefix(task))
                .unwrap_or(after)
        } else if digits > 0
            && (rest[digits..].starts_with(". ") || rest[digits..].starts_with(") "))
        {
            &rest[digits + 2..]
        } else {
            return rest;
        };
        line = next;
    }
}

fn skim_inline(line: &str, out: &mut String) {
    let mut opens = Vec::new();
    let mut rest = line;
    while let Some(c) = rest.chars().next() {
        let after = &rest[c.len_utf8()..];
        match c {
            '[' => {
                opens.push(out.len());
                out.push('[');
            }
            ']' if after.starts_with('(') && !opens.is_empty() => {
                let open = opens.pop().unwrap_or_default();
                out.remove(open);
                if out[..open].ends_with('!') {
                    out.remove(open - 1);
                }
                rest = skip_target(&after[1..]);
                continue;
            }
            '<' => {
                if let Some(close) = after.find('>') {
                    let inner = &after[..close];
                    if inner.contains("://") || inner.contains('@') {
                        out.push_str(inner);
                        rest = &after[close + 1..];
                        continue;
                    }
                    if inner
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!')
                    {
                        if inner
                            .trim_start_matches('/')
                            .to_ascii_lowercase()
                            .starts_with("br")
                        {
                            out.push(' ');
                        }
                        rest = &after[close + 1..];
                        continue;
                    }
                }
                out.push('<');
            }
            '&' => {
                let entity = [
                    ("amp;", '&'),
                    ("lt;", '<'),
                    ("gt;", '>'),
                    ("quot;", '"'),
                    ("#39;", '\''),
                    ("nbsp;", ' '),
                ]
                .into_iter()
                .find(|(name, _)| after.starts_with(name));
                match entity {
                    Some((name, shown)) => {
                        out.push(shown);
                        rest = &after[name.len()..];
                        continue;
                    }
                    None => out.push('&'),
                }
            }
            _ => out.push(c),
        }
        rest = after;
    }
}

/// The text after a link target's closing parenthesis, counting nested
/// parentheses in the URL.
fn skip_target(target: &str) -> &str {
    let mut depth = 0usize;
    for (at, c) in target.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return &target[at + 1..],
            ')' => depth -= 1,
            _ => {}
        }
    }
    ""
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found<'a>(query: &str, text: &'a str) -> Vec<&'a str> {
        Needle::new(query)
            .expect("query")
            .find(text)
            .into_iter()
            .map(|range| &text[range])
            .collect()
    }

    fn count(query: &str, markdown: &str) -> usize {
        Needle::new(query)
            .expect("query")
            .count_in_markdown(markdown)
    }

    #[test]
    fn matches_ignore_case_and_accents_and_return_source_ranges() {
        assert_eq!(
            found("cafe", "Café, CAFÉ and café."),
            ["Café", "CAFÉ", "café"]
        );
        assert_eq!(found("crème", "Creme brulee"), ["Creme"]);
    }

    #[test]
    fn spaces_and_line_breaks_read_as_one_space() {
        assert_eq!(
            found("deploy  plan", "the deploy\n  plan and deploy plan"),
            ["deploy\n  plan", "deploy plan"]
        );
        assert_eq!(found(" plan ", "plan"), ["plan"]);
    }

    #[test]
    fn matches_do_not_overlap() {
        assert_eq!(found("aa", "aaaaa"), ["aa", "aa"]);
    }

    #[test]
    fn an_empty_or_marks_only_query_finds_nothing() {
        assert_eq!(Needle::new("  "), None);
        assert_eq!(Needle::new("**"), None);
    }

    #[test]
    fn markdown_counts_what_a_reader_sees() {
        assert_eq!(count("found it", "I **found** it"), 1);
        assert_eq!(count("foobar", "**foo**bar"), 1);
        assert_eq!(count("example", "See [the docs](https://example.com/x)"), 0);
        assert_eq!(
            count("the docs", "See [the docs](https://example.com/(x))"),
            1
        );
        assert_eq!(count("chart", "![chart](c.png) and [chart][1]"), 2);
        assert_eq!(count("example", "Opened <https://example.com/a>"), 1);
        assert_eq!(count("div", "<div>Done</div>"), 0);
        assert_eq!(count("a & b", "a &amp; b"), 1);
        assert_eq!(count("rust", "```rust\nfn main() {}\n```"), 0);
        assert_eq!(count("fn main", "```rust\nfn main() {}\n```"), 1);
        assert_eq!(count("a b 1 2", "| a | b |\n|---|---|\n| 1 | 2 |"), 1);
        assert_eq!(count("next shipped", "# Next\n- [x] shipped"), 1);
        assert_eq!(count("one two", "1. one\n> 2) two"), 1);
        assert_eq!(count("#563", "Fixes #563"), 1);
    }
}
