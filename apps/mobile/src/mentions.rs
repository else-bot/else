//! Bot mentions in the chat's text, found as the desktop finds them
//! (else_client::mention) and shown in bold, as Markdown: an owner's plain
//! message is escaped first so nothing else in it turns into formatting; a
//! bot's Markdown reply keeps its code untouched.

use else_client::mention::{Registry, find};

/// `text` as Markdown that shows exactly those characters.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '`'
                | '*'
                | '_'
                | '['
                | ']'
                | '('
                | ')'
                | '#'
                | '+'
                | '-'
                | '.'
                | '!'
                | '>'
                | '|'
                | '~'
                | '<'
                | '{'
                | '}'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// An owner's plain message as Markdown, its mentions in bold. Line breaks
/// stay line breaks.
pub fn plain(text: &str, bots: &Registry) -> String {
    let mut out = String::new();
    let mut at = 0;
    for mention in find(text, bots) {
        out.push_str(&escape(&text[at..mention.range.start]));
        out.push_str("**");
        out.push_str(&escape(&mention.text));
        out.push_str("**");
        at = mention.range.end;
    }
    out.push_str(&escape(&text[at..]));
    // A single newline is a soft break in Markdown; the owner meant a new line.
    out.replace('\n', "  \n")
}

/// Whether byte `at` of `markdown` sits in code: a fenced block or an
/// inline span, by the fences and backticks before it.
fn in_code(markdown: &str, at: usize) -> bool {
    let before = &markdown[..at];
    let fences = before
        .lines()
        .filter(|l| l.trim_start().starts_with("```"))
        .count();
    if fences % 2 == 1 {
        return true;
    }
    let line = before.rsplit('\n').next().unwrap_or(before);
    line.matches('`').count() % 2 == 1
}

/// A bot's Markdown reply with its mentions in bold, code left as it is.
pub fn markdown(source: &str, bots: &Registry) -> String {
    let mut out = String::new();
    let mut at = 0;
    for mention in find(source, bots) {
        if in_code(source, mention.range.start) {
            continue;
        }
        out.push_str(&source[at..mention.range.start]);
        out.push_str("**");
        out.push_str(&mention.text);
        out.push_str("**");
        at = mention.range.end;
    }
    out.push_str(&source[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use else_wire::bots::Bot;
    use uuid::Uuid;

    fn bots(names: &[&str]) -> Registry {
        let list: Vec<Bot> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                serde_json::from_value(serde_json::json!({
                    "id": Uuid::from_u128(i as u128 + 1), "name": name,
                }))
                .unwrap()
            })
            .collect();
        Registry::new(&list)
    }

    #[test]
    fn an_owner_message_bolds_mentions_and_nothing_else() {
        assert_eq!(
            plain("Ask @Scout *now*\nthanks", &bots(&["Scout"])),
            "Ask **@Scout** \\*now\\*  \nthanks"
        );
    }

    #[test]
    fn a_reply_bolds_mentions_outside_code() {
        assert_eq!(
            markdown("Ask @Scout, run `@Scout`", &bots(&["Scout"])),
            "Ask **@Scout**, run `@Scout`"
        );
        assert_eq!(
            markdown("```\n@Scout\n```\n@Scout", &bots(&["Scout"])),
            "```\n@Scout\n```\n**@Scout**"
        );
    }
}
