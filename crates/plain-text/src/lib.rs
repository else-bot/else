//! Markdown flattened to plain text for one-line previews.

use markdown::mdast::Node;

pub mod find;

/// The lines `markdown` shows, as plain text: link text without its URL,
/// markup and list markers dropped, whitespace collapsed, empty lines
/// skipped. Parses the whole text, so cut it afterwards, never before.
pub fn lines(markdown: &str) -> Vec<String> {
    let mut out = String::new();
    match markdown::to_mdast(markdown, &markdown::ParseOptions::gfm()) {
        Ok(root) => visit(&root, &mut out),
        Err(_) => out.push_str(markdown),
    }
    out.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect()
}

/// All of `markdown` as one line of plain text.
pub fn one_line(markdown: &str) -> String {
    lines(markdown).join(" ")
}

/// `text` cut to at most `max_chars` characters, at a word when one ends in
/// the second half, then followed by an ellipsis.
pub fn cut(text: &str, max_chars: usize) -> String {
    let Some((end, _)) = text.char_indices().nth(max_chars) else {
        return text.to_owned();
    };
    let head = &text[..end];
    let head = match head.rfind(' ') {
        Some(space) if head[..space].chars().count() > max_chars / 2 => &head[..space],
        _ => head,
    };
    format!("{}…", head.trim_end())
}

fn visit(node: &Node, out: &mut String) {
    match node {
        Node::Text(t) => out.push_str(&t.value),
        Node::InlineCode(c) => out.push_str(&c.value.replace('\n', " ")),
        Node::InlineMath(m) => out.push_str(&m.value),
        Node::Code(c) => out.push_str(&c.value),
        Node::Math(m) => out.push_str(&m.value),
        Node::Image(image) => out.push_str(&image.alt),
        Node::ImageReference(image) => out.push_str(&image.alt),
        Node::Html(html) => push_html(&html.value, out),
        Node::Break(_) => out.push('\n'),
        _ => {}
    }
    for child in node.children().into_iter().flatten() {
        visit(child, out);
    }
    match node {
        Node::TableCell(_) => out.push(' '),
        Node::Text(_)
        | Node::Emphasis(_)
        | Node::Strong(_)
        | Node::Delete(_)
        | Node::InlineCode(_)
        | Node::InlineMath(_)
        | Node::Link(_)
        | Node::LinkReference(_)
        | Node::Image(_)
        | Node::ImageReference(_)
        | Node::Break(_)
        | Node::Html(_)
        | Node::FootnoteReference(_)
        | Node::MdxTextExpression(_)
        | Node::MdxJsxTextElement(_) => {}
        _ => out.push('\n'),
    }
}

/// The text an HTML fragment shows: tags dropped, `<br>` as a line break.
fn push_html(html: &str, out: &mut String) {
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else {
            rest = &rest[open..];
            break;
        };
        let tag = rest[open + 1..open + close].trim_start_matches('/');
        if tag
            .get(..2)
            .is_some_and(|name| name.eq_ignore_ascii_case("br"))
        {
            out.push('\n');
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG_URL: &str = "https://app.t3.codes/threads/032fb8c7-85fb-4c1e/mcp%3Alaunch%3A\
        abababababababababababababababababababababababababababababababab";

    #[test]
    fn a_link_cut_mid_url_keeps_only_its_text() {
        let text = format!("I started [Implement #468: stacked waiting rows]({LONG_URL}) today");
        assert_eq!(
            cut(&one_line(&text), 40),
            "I started Implement #468: stacked…"
        );
    }

    #[test]
    fn a_link_cut_mid_text_drops_the_url() {
        let text = format!("See [the full deployment checklist for staging]({LONG_URL})");
        assert_eq!(cut(&one_line(&text), 24), "See the full deployment…");
    }

    #[test]
    fn markup_is_dropped() {
        for (source, expected) in [
            (
                "**Ready _now_** with `cargo test`",
                "Ready now with cargo test",
            ),
            ("## [Report](https://example.com) ~~old~~", "Report old"),
            ("- [x] shipped\n- 2. nested", "shipped nested"),
            ("> quoted **bold** line", "quoted bold line"),
            ("![chart of usage](u.png) below", "chart of usage below"),
            ("\\*literal\\* &amp; café", "*literal* & café"),
            ("2 * 3 = 6 and foo_bar", "2 * 3 = 6 and foo_bar"),
            ("| a | b |\n|---|---|\n| 1 | 2 |", "a b 1 2"),
            ("---", ""),
            ("<div>Done</div>", "Done"),
            ("A<br>B and <b>bold</b>", "A B and bold"),
            ("`<path>` exists", "<path> exists"),
        ] {
            assert_eq!(one_line(source), expected, "{source}");
        }
    }

    #[test]
    fn autolinks_stay_as_their_url() {
        let text = format!("Opened {LONG_URL}");
        assert_eq!(one_line(&text), format!("Opened {LONG_URL}"));
        assert_eq!(
            one_line("Opened <https://example.com/a>"),
            "Opened https://example.com/a"
        );
        assert_eq!(cut(&one_line(&text), 30), "Opened https://app.t3.codes/th…");
    }

    #[test]
    fn lines_split_where_the_text_breaks() {
        assert_eq!(
            lines("\n**Opened** Search Console\nmore  detail\n\n# Next\n\n```\nfn a() {}\n```"),
            ["Opened Search Console", "more detail", "Next", "fn a() {}"]
        );
        assert!(lines("  \n\n").is_empty());
    }

    #[test]
    fn cut_counts_characters_not_bytes() {
        assert_eq!(cut("café crème brûlée", 50), "café crème brûlée");
        assert_eq!(cut("café crème brûlée", 12), "café crème…");
        assert_eq!(cut(&"✓".repeat(20), 10), format!("{}…", "✓".repeat(10)));
        assert_eq!(cut("éé ééééééé", 9), "éé éééééé…");
    }
}
