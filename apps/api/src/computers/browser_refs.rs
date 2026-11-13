//! The controls a bot's latest browser snapshot listed, by ref, so an
//! approval card can name the control a ref selector points at.

use std::collections::HashMap;

use uuid::Uuid;

use super::ComputerManager;

/// What a snapshot line said about one ref. Page content, so untrusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefLabel {
    pub role: String,
    pub name: String,
    pub url: Option<String>,
}

/// The refs in a snapshot's lines, such as `button "Delete" @kqvzb5 [disabled]`
/// or `link "Docs" @kqvzb6 [url=/docs]`. Headings carry no ref and are skipped.
pub fn parse(snapshot: &str) -> HashMap<String, RefLabel> {
    snapshot.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<(String, RefLabel)> {
    let (role, rest) = line.split_once(' ')?;
    let mut names = serde_json::Deserializer::from_str(rest).into_iter::<String>();
    let name = names.next()?.ok()?;
    let rest = rest[names.byte_offset()..].strip_prefix(" @")?;
    let (id, states) = rest.split_once(' ').unwrap_or((rest, ""));
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let url = states
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .and_then(|s| s.split(", ").next())
        .and_then(|s| s.strip_prefix("url="))
        .map(str::to_owned);
    let label = RefLabel {
        role: role.to_owned(),
        name,
        url,
    };
    Some((id.to_owned(), label))
}

impl ComputerManager {
    /// Keep a successful snapshot's refs as the bot's latest; the guest
    /// refuses refs from any earlier snapshot.
    pub async fn remember_browser_refs(&self, bot: Uuid, refs: HashMap<String, RefLabel>) {
        self.browser_refs
            .call(move |all| {
                all.insert(bot, refs);
            })
            .await;
    }

    /// The label the bot's latest snapshot gave a ref (`@kqvzb5`), if any.
    pub async fn browser_ref(&self, bot: Uuid, selector: &str) -> Option<RefLabel> {
        let id = selector.strip_prefix('@')?.to_owned();
        self.browser_refs
            .call(move |all| all.get(&bot)?.get(&id).cloned())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_lines_give_each_ref_its_role_name_and_link_url() {
        let refs = parse(
            "heading \"Projects\"\n\
             button \"Delete project\" @kqvzb5 [disabled]\n\
             textbox \"Email\" @kqvzb6 [value=\"a@b.c\", required]\n\
             link \"Docs\" @kqvzb7 [url=/docs, expanded]\n\
             link \"\" @kqvzb8 [url=https://x.com/a]\n\
             button \"say \\\"hi\\\" @fake1\" @kqvzb9\n\
             not a snapshot line",
        );
        assert_eq!(refs.len(), 5, "{refs:?}");
        let label = |r: &str| refs[r].clone();
        assert_eq!(
            label("kqvzb5"),
            RefLabel {
                role: "button".into(),
                name: "Delete project".into(),
                url: None
            }
        );
        assert_eq!(label("kqvzb6").name, "Email");
        assert_eq!(label("kqvzb6").url, None);
        assert_eq!(label("kqvzb7").url.as_deref(), Some("/docs"));
        assert_eq!(label("kqvzb8").url.as_deref(), Some("https://x.com/a"));
        assert_eq!(label("kqvzb9").name, "say \"hi\" @fake1");
        assert!(!refs.contains_key("fake1"));
    }
}
