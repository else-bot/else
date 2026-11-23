//! Bot mentions in the chat's text, found as the desktop finds them
//! (else_client::mention) and shown in bold: an owner's plain message as
//! text with bold pieces, exactly as typed; a bot's Markdown reply keeps
//! its code untouched.

use else_client::mention::{Registry, find, find_in_markdown};

/// An owner's plain message cut at its mentions: each piece with whether
/// it is a mention, to show in bold. Nothing else in the text changes.
pub fn plain(text: &str, bots: &Registry) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut at = 0;
    for mention in find(text, bots) {
        if at < mention.range.start {
            out.push((text[at..mention.range.start].to_owned(), false));
        }
        out.push((text[mention.range.clone()].to_owned(), true));
        at = mention.range.end;
    }
    if at < text.len() {
        out.push((text[at..].to_owned(), false));
    }
    out
}

/// A bot's Markdown reply with its mentions in bold; code, links and HTML
/// left as they are.
pub fn markdown(source: &str, bots: &Registry) -> String {
    let mut out = String::new();
    let mut at = 0;
    for mention in find_in_markdown(source, bots) {
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
    fn an_owner_message_bolds_mentions_and_keeps_the_rest_as_typed() {
        assert_eq!(
            plain("Ask @Scout *now*\n  https://a.b?x&amp;y", &bots(&["Scout"])),
            [
                ("Ask ".to_owned(), false),
                ("@Scout".to_owned(), true),
                (" *now*\n  https://a.b?x&amp;y".to_owned(), false),
            ]
        );
    }

    #[test]
    fn a_reply_bolds_mentions_in_prose_only() {
        assert_eq!(
            markdown(
                "Ask @Scout, run ``@Scout`` or [him](@Scout)\n~~~\n@Scout\n~~~",
                &bots(&["Scout"])
            ),
            "Ask **@Scout**, run ``@Scout`` or [him](@Scout)\n~~~\n@Scout\n~~~"
        );
    }
}
