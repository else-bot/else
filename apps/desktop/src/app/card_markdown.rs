//! The light markdown a card body may carry. A card shows text the owner
//! must check before answering, so only unambiguous markup formats: fenced
//! code blocks, inline code, list markers at the start of a line, and
//! `**bold**`. Every other markdown character is escaped, a line break stays
//! a line break, and code keeps its exact characters.

/// What a card's text is, which decides when it formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CardText {
    /// Prose for the owner, such as a question's context.
    Prose,
    /// The exact action an approval covers. It formats only when it holds a
    /// fenced block, so an unfenced command keeps every backtick and star.
    Action,
}

impl CardText {
    /// The markdown source that shows `text`, or `None` when `text` shows as
    /// its exact characters.
    pub(super) fn markdown(self, text: &str) -> Option<String> {
        let (source, found) = convert(text);
        let formats = match self {
            Self::Prose => found.fence || found.markup,
            Self::Action => found.fence,
        };
        (formats && allowed(&source)).then_some(source)
    }
}

/// The markdown source that shows `text` with only the allowed markup, or
/// `None` when the conversion would let anything else through.
pub(super) fn restricted(text: &str) -> Option<String> {
    let source = convert(text).0;
    allowed(&source).then_some(source)
}

/// Whether `source` parses, as the text view parses it, into only the
/// allowed nodes.
fn allowed(source: &str) -> bool {
    use markdown::mdast::Node;
    fn only_allowed(node: &Node) -> bool {
        matches!(
            node,
            Node::Root(_)
                | Node::Paragraph(_)
                | Node::Text(_)
                | Node::Code(_)
                | Node::InlineCode(_)
                | Node::List(_)
                | Node::ListItem(_)
                | Node::Strong(_)
                | Node::Break(_)
        ) && node.children().into_iter().flatten().all(only_allowed)
    }
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    markdown::to_mdast(source, &options).is_ok_and(|root| only_allowed(&root))
}

/// Each line with the ending that closed it, split on every line ending the
/// markdown parser knows: `\r\n`, `\n` and a lone `\r`.
fn lines(text: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest.find(['\r', '\n']).unwrap_or(rest.len());
        let ending = if rest[end..].starts_with("\r\n") {
            2
        } else {
            usize::from(end < rest.len())
        };
        let line = (&rest[..end], &rest[end..end + ending]);
        rest = &rest[end + ending..];
        Some(line)
    })
}

#[derive(Default)]
struct Found {
    fence: bool,
    markup: bool,
}

#[derive(Clone, Copy)]
enum Line<'a> {
    Blank,
    Item {
        indent: usize,
        marker: &'a str,
        rest: &'a str,
    },
    Prose {
        indented: bool,
        text: &'a str,
    },
}

/// A fence opener: its marker run and info string.
fn fence(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let mark = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let run = trimmed.len() - trimmed.trim_start_matches(mark).len();
    let info = trimmed[run..].trim();
    (run >= 3 && !(mark == '`' && info.contains('`'))).then(|| (&trimmed[..run], info))
}

fn closes(line: &str, run: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    let mark = run.as_bytes()[0] as char;
    let rest = trimmed.trim_start_matches(mark);
    line.len() - trimmed.len() <= 3
        && trimmed.len() - rest.len() >= run.len()
        && rest.trim_matches([' ', '\t']).is_empty()
}

fn classify(line: &str) -> Line<'_> {
    let text = line.trim_start();
    if text.is_empty() {
        return Line::Blank;
    }
    let indent = line[..line.len() - text.len()]
        .chars()
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum();
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let marker = match text.as_bytes()[0] {
        b'-' | b'*' | b'+' => Some(1),
        _ if (1..=9).contains(&digits)
            && matches!(text.as_bytes().get(digits), Some(b'.' | b')')) =>
        {
            Some(digits + 1)
        }
        _ => None,
    };
    if let Some(len) = marker
        && text[len..].starts_with([' ', '\t'])
        && !text[len..].trim().is_empty()
    {
        return Line::Item {
            indent,
            marker: &text[..len],
            rest: text[len..].trim(),
        };
    }
    Line::Prose {
        indented: indent > 0,
        text: text.trim_end(),
    }
}

fn convert(text: &str) -> (String, Found) {
    let mut found = Found::default();
    let mut out = String::new();
    let mut lines = lines(text);
    let mut prose: Vec<Line> = Vec::new();
    while let Some((line, _)) = lines.next() {
        let Some((run, info)) = fence(line) else {
            prose.push(classify(line));
            continue;
        };
        found.fence = true;
        flush(&prose, &mut out, &mut found);
        prose.clear();
        out.push_str(run);
        if !info.is_empty() {
            out.push(' ');
            out.push_str(info);
        }
        out.push('\n');
        for (body, ending) in lines.by_ref() {
            if closes(body, run) {
                break;
            }
            out.push_str(body);
            out.push_str(if ending.is_empty() { "\n" } else { ending });
        }
        out.push_str(run);
        out.push_str("\n\n");
    }
    flush(&prose, &mut out, &mut found);
    (out.trim_end().to_owned(), found)
}

/// Writes a run of lines outside fences. Inside a paragraph each line break
/// becomes a hard break; a list starts and ends on a blank line so it never
/// swallows the prose around it.
fn flush(lines: &[Line], out: &mut String, found: &mut Found) {
    // The content column of the list item last written, while in a list.
    let mut list: Option<usize> = None;
    let mut previous = Line::Blank;
    for &line in lines {
        match line {
            Line::Blank => {
                if !matches!(previous, Line::Blank) {
                    out.push_str("\n\n");
                }
            }
            Line::Item {
                indent,
                marker,
                rest,
            } => {
                found.markup = true;
                match previous {
                    Line::Prose { .. } if list.is_none() => out.push_str("\n\n"),
                    Line::Blank => {}
                    _ => out.push('\n'),
                }
                let indent = list.map_or(0, |column| indent.min(column));
                out.push_str(&" ".repeat(indent));
                out.push_str(marker);
                out.push(' ');
                list = Some(indent + marker.len() + 1);
                inline(rest, true, out, found);
            }
            Line::Prose { indented, text } => {
                match previous {
                    Line::Blank => list = None,
                    Line::Item { .. } | Line::Prose { .. } if list.is_some() && !indented => {
                        out.push_str("\n\n");
                        list = None;
                    }
                    _ => out.push_str("\\\n"),
                }
                inline(text, true, out, found);
            }
        }
        previous = line;
    }
    if !matches!(previous, Line::Blank) {
        out.push_str("\n\n");
    }
}

/// Writes one line's text: matched backtick spans verbatim, matched `**`
/// pairs as bold when `bold` allows, and every other ASCII punctuation
/// character escaped.
fn inline(text: &str, bold: bool, out: &mut String, found: &mut Found) {
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if c == '`' {
            let run = rest.len() - rest.trim_start_matches('`').len();
            if let Some(end) = closing_ticks(&rest[run..], run) {
                found.markup = true;
                out.push_str(&rest[..run + end + run]);
                rest = &rest[run + end + run..];
            } else {
                out.push_str(&"\\`".repeat(run));
                rest = &rest[run..];
            }
            continue;
        }
        if bold
            && let Some(after) = rest.strip_prefix("**")
            && let Some(end) = closing_stars(after)
            && end > 0
            && !after.starts_with(char::is_whitespace)
            && !after[..end].ends_with(char::is_whitespace)
        {
            found.markup = true;
            out.push_str("**");
            inline(&after[..end], false, out, found);
            out.push_str("**");
            rest = &after[end + 2..];
            continue;
        }
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
}

/// Where the `**` that closes a bold run starts in `text`, skipping matched
/// code spans so a delimiter inside code stays code.
fn closing_stars(text: &str) -> Option<usize> {
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        if rest.starts_with("**") {
            return Some(at);
        }
        if rest.starts_with('`') {
            let run = rest.len() - rest.trim_start_matches('`').len();
            at += closing_ticks(&rest[run..], run).map_or(run, |end| run + end + run);
            continue;
        }
        at += rest.chars().next().map_or(1, char::len_utf8);
    }
    None
}

/// Where a run of exactly `run` backticks starts in `text`.
fn closing_ticks(text: &str, run: usize) -> Option<usize> {
    let mut at = 0;
    while let Some(start) = text[at..].find('`').map(|ix| ix + at) {
        let len = text[start..].len() - text[start..].trim_start_matches('`').len();
        if len == run {
            return Some(start);
        }
        at = start + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{CardText, restricted};

    fn source(text: &str) -> String {
        restricted(text).expect("only allowed markup")
    }
    use markdown::mdast::Node;

    fn tree(text: &str) -> Node {
        markdown::to_mdast(&source(text), &markdown::ParseOptions::gfm()).unwrap()
    }

    fn kinds(node: &Node, out: &mut Vec<&'static str>) {
        let kind = match node {
            Node::Root(_) | Node::Paragraph(_) | Node::Text(_) => None,
            Node::Code(_) => Some("code"),
            Node::InlineCode(_) => Some("inline code"),
            Node::List(_) => Some("list"),
            Node::ListItem(_) => Some("item"),
            Node::Strong(_) => Some("strong"),
            Node::Break(_) => Some("break"),
            _ => Some("other"),
        };
        out.extend(kind);
        for child in node.children().into_iter().flatten() {
            kinds(child, out);
        }
    }

    fn code_blocks(node: &Node, out: &mut Vec<(Option<String>, String)>) {
        if let Node::Code(code) = node {
            out.push((code.lang.clone(), code.value.clone()));
        }
        for child in node.children().into_iter().flatten() {
            code_blocks(child, out);
        }
    }

    #[test]
    fn the_usb_fix_card_shows_three_verbatim_code_blocks() {
        let script = "#!/bin/sh\nset -eu\nfor dev in /sys/class/net/*; do\n    echo \"$dev\" | grep -q '^en' && ip link set \"${dev##*/}\" up\ndone";
        let unit = "[Unit]\nDescription=USB-C Ethernet boot fix\n\n[Service]\nType=oneshot\nExecStart=/usr/local/bin/usb-eth-fix";
        let command = "sudo systemctl enable --now usb-eth-fix.service";
        let text = format!(
            "Writes two files on **omarchy-m1**, then enables the unit:\n\n```sh\n{script}\n```\n\n`/etc/systemd/system/usb-eth-fix.service`:\n```ini\n{unit}\n```\n\n```\n{command}\n```"
        );
        assert!(CardText::Action.markdown(&text).is_some());
        let mut blocks = Vec::new();
        code_blocks(&tree(&text), &mut blocks);
        assert_eq!(
            blocks,
            vec![
                (Some("sh".into()), script.into()),
                (Some("ini".into()), unit.into()),
                (None, command.into()),
            ]
        );
    }

    #[test]
    fn text_without_markup_stays_plain() {
        for text in [
            "Delete 3 old drafts?",
            "Rename file_name_v2 to file_name_v3 and 2*3 = 6.",
            "a * b * c",
            "Line one\nLine two",
            "Price: $5 & up <today>",
        ] {
            assert!(CardText::Prose.markdown(text).is_none(), "{text}");
            assert!(CardText::Action.markdown(text).is_none(), "{text}");
        }
    }

    #[test]
    fn an_unfenced_action_keeps_its_backticks_and_stars() {
        for action in [
            "cp `which foo` /tmp",
            "rm -rf **/node_modules **/dist",
            "- run this",
        ] {
            assert!(CardText::Action.markdown(action).is_none(), "{action}");
        }
        assert!(CardText::Prose.markdown("cp `which foo` /tmp").is_some());
        assert!(
            CardText::Prose
                .markdown("rm -rf **/node_modules **/dist")
                .is_none()
        );
    }

    #[test]
    fn prose_formats_only_the_allowed_markup() {
        let text = "# Not a heading\nRun `make` then **check**:\n- one [link](https://example.com)\n- two <b>html</b>\n\n> no quote\n| a | b |\n|---|---|\nhttps://example.com ~~gone~~ $x$ ![img](a.png) _em_ *em*";
        let mut found = Vec::new();
        kinds(&tree(text), &mut found);
        assert!(!found.contains(&"other"), "{found:?}\n{}", source(text));
        for kind in ["inline code", "strong", "list", "item", "break"] {
            assert!(found.contains(&kind), "{kind} in {found:?}");
        }
    }

    #[test]
    fn every_character_outside_markup_survives() {
        fn text(node: &Node) -> String {
            match node {
                Node::Text(t) => t.value.clone(),
                Node::InlineCode(c) => format!("`{}`", c.value),
                Node::Break(_) => "\n".into(),
                _ => node.children().into_iter().flatten().map(text).collect(),
            }
        }
        let line = r#"Path C:\Users\me\*.txt, a_b_c, 1 < 2 > 0, "quoted" & 'single' {x} [y] (z) ~ ^ | # ! = %"#;
        let input = format!("{line}\nsee `x`");
        assert_eq!(text(&tree(&input)), input);
    }

    #[test]
    fn lists_keep_their_prose_apart_and_nest_one_level() {
        let text = "Steps:\n1. stop it\n2. start it\n    - nested\nAfter the list.";
        let mut found = Vec::new();
        kinds(&tree(text), &mut found);
        assert_eq!(
            found.iter().filter(|k| **k == "list").count(),
            2,
            "{found:?}"
        );
        assert_eq!(
            found.iter().filter(|k| **k == "item").count(),
            3,
            "{found:?}"
        );
        let Node::Root(root) = tree(text) else {
            unreachable!()
        };
        assert!(matches!(root.children.last(), Some(Node::Paragraph(_))));
    }

    #[test]
    fn a_cut_preview_closes_its_open_fence() {
        let text = "Writes the script:\n```sh\nset -eu\n    echo hi";
        let mut blocks = Vec::new();
        code_blocks(&tree(text), &mut blocks);
        assert_eq!(
            blocks,
            vec![(Some("sh".into()), "set -eu\n    echo hi".into())]
        );
    }

    #[test]
    fn a_fence_holding_a_shorter_fence_keeps_it() {
        let text = "````md\n```sh\nls\n```\n````";
        let mut blocks = Vec::new();
        code_blocks(&tree(text), &mut blocks);
        assert_eq!(blocks, vec![(Some("md".into()), "```sh\nls\n```".into())]);
    }

    #[test]
    fn line_endings_split_as_the_parser_splits_them() {
        for text in [
            "~~~sh\r~~~\r# heading\nbody\n~~~",
            "```sh\rls\r```\r[link](https://example.com) ![i](a.png) <b>x</b>\r| a |\r|---|",
        ] {
            let source = source(text);
            let mut found = Vec::new();
            kinds(
                &markdown::to_mdast(&source, &markdown::ParseOptions::gfm()).unwrap(),
                &mut found,
            );
            assert!(!found.contains(&"other"), "{found:?}\n{source}");
        }
    }

    #[test]
    fn code_keeps_its_line_endings_and_fence_rules() {
        let mut blocks = Vec::new();
        code_blocks(&tree("```sh\r\necho hi\r\n    next\r\n```"), &mut blocks);
        assert_eq!(
            blocks,
            vec![(Some("sh".into()), "echo hi\r\n    next".into())]
        );
        let mut blocks = Vec::new();
        code_blocks(&tree("~~~ ~foo\nhi\n~~~\nafter"), &mut blocks);
        assert_eq!(blocks, vec![(Some("~foo".into()), "hi".into())]);
        let mut blocks = Vec::new();
        code_blocks(&tree("```sh\nfirst\n```\u{a0}\nlast\n```"), &mut blocks);
        assert_eq!(
            blocks,
            vec![(Some("sh".into()), "first\n```\u{a0}\nlast".into())]
        );
    }

    #[test]
    fn bold_never_ends_inside_inline_code() {
        let Node::Root(root) = tree("**Run `echo \"**\"` now**") else {
            unreachable!()
        };
        let Some(Node::Paragraph(paragraph)) = root.children.first() else {
            panic!("{root:?}")
        };
        let [Node::Strong(strong)] = paragraph.children.as_slice() else {
            panic!("{paragraph:?}")
        };
        assert!(
            strong
                .children
                .iter()
                .any(|node| matches!(node, Node::InlineCode(code) if code.value == "echo \"**\""))
        );
    }
}
