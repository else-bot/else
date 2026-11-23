//! Mentioning a bot by name in a message (`@Scout`): the query typed after
//! `@` and the bots it offers, and the mentions a message's text holds. The
//! desktop draws them as chips, the phone styles them; the text sent is the
//! name itself.

use std::ops::Range;

use else_wire::bots::{Avatar, Bot};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MentionQuery {
    /// The bytes of `@query` in the text, the mention's rest included.
    pub range: Range<usize>,
    /// What was typed after `@`, lowercased.
    pub text: String,
}

impl MentionQuery {
    pub fn at(text: &str, selection: Range<usize>) -> Option<Self> {
        if !selection.is_empty() {
            return None;
        }
        let caret = selection.start;
        let before = text.get(..caret)?;
        let start = before.rfind('@')?;
        if !mention_boundary(&before[..start]) {
            return None;
        }
        let query = &before[start + 1..];
        if query.is_empty() || !query.chars().all(mention_character) {
            return None;
        }
        let suffix = &text[caret..];
        let end = caret
            + suffix
                .find(|c| !mention_character(c))
                .unwrap_or(suffix.len());
        Some(Self {
            range: start..end,
            text: query.to_lowercase(),
        })
    }

    pub fn candidates(&self, bots: &[Bot]) -> Vec<Uuid> {
        bots.iter()
            .filter(|bot| bot.is_active() && bot.name.to_lowercase().contains(&self.text))
            .map(|bot| bot.id)
            .collect()
    }
}

pub fn mention_boundary(before: &str) -> bool {
    before
        .chars()
        .next_back()
        .is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | '{' | '"' | '\''))
}

pub fn mention_character(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// The bots a message can mention: active ones whose names fit on a line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: Uuid,
    pub name: String,
    pub avatar: Avatar,
}

#[derive(Default, PartialEq, Eq)]
pub struct Registry(Vec<Entry>);

impl Registry {
    pub fn name(&self, id: Uuid) -> Option<&str> {
        self.0
            .iter()
            .find(|bot| bot.id == id)
            .map(|bot| bot.name.as_str())
    }

    pub fn new(bots: &[Bot]) -> Self {
        let mut entries: Vec<_> = bots
            .iter()
            .filter(|bot| {
                bot.is_active()
                    && !bot
                        .name
                        .chars()
                        .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
            })
            .map(|bot| Entry {
                id: bot.id,
                name: bot.name.clone(),
                avatar: bot.avatar,
            })
            .collect();
        entries.sort_by_key(|bot| bot.id);
        Self(entries)
    }
}

/// One mention in a message's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mention {
    pub range: Range<usize>,
    pub id: Uuid,
    pub name: String,
    /// The mention as written, `@` included.
    pub text: String,
    pub avatar: Avatar,
}

/// The mentions in `source`: a whole name after `@` at a word boundary, the
/// longest name winning; a name two bots share stays plain.
pub fn find(source: &str, bots: &Registry) -> Vec<Mention> {
    let mut mentions = Vec::new();
    let mut end = 0;
    for (start, _) in source.match_indices('@') {
        if start < end || !mention_boundary(&source[..start]) {
            continue;
        }
        let suffix = &source[start + 1..];
        let matched = bots
            .0
            .iter()
            .filter_map(|bot| {
                let len = suffix
                    .char_indices()
                    .nth(bot.name.chars().count())
                    .map_or(suffix.len(), |(ix, _)| ix);
                (suffix[..len].to_lowercase() == bot.name.to_lowercase()
                    && suffix[len..].chars().next().is_none_or(|c| {
                        c.is_whitespace()
                            || matches!(
                                c,
                                '.' | ',' | '!' | '?' | ':' | ';' | ')' | ']' | '}' | '"' | '\''
                            )
                    }))
                .then_some((bot, start + 1 + len))
            })
            .max_by_key(|(_, end)| *end);
        if let Some((bot, matched_end)) = matched {
            if bots
                .0
                .iter()
                .filter(|candidate| candidate.name.to_lowercase() == bot.name.to_lowercase())
                .count()
                > 1
            {
                continue;
            }
            end = matched_end;
            mentions.push(Mention {
                range: start..end,
                id: bot.id,
                name: bot.name.clone(),
                text: source[start..end].to_owned(),
                avatar: bot.avatar,
            });
        }
    }
    mentions
}

#[cfg(test)]
mod tests {
    use super::MentionQuery;
    use uuid::Uuid;

    #[test]
    fn opens_after_the_first_character_at_a_mention_boundary() {
        for text in ["@s", "Hey @s", "Hello\n@日", "(@s", "@Scout @s"] {
            assert!(
                MentionQuery::at(text, text.len()..text.len()).is_some(),
                "{text}"
            );
        }
        for text in ["@", "Hey @", "a@scout", "hi@example.com", "@s ", "@@s"] {
            assert!(
                MentionQuery::at(text, text.len()..text.len()).is_none(),
                "{text}"
            );
        }
        assert!(MentionQuery::at("@scout", 1..3).is_none());
    }

    #[test]
    fn replaces_only_the_mention_at_the_caret_with_unicode_and_a_suffix() {
        let mut text = "Héllo @scout, help @todo".to_owned();
        let caret = "Héllo @sc".len();
        let query = MentionQuery::at(&text, caret..caret).unwrap();
        assert_eq!(query.text, "sc");
        text.replace_range(query.range, "@Scout Bot ");
        assert_eq!(text, "Héllo @Scout Bot , help @todo");
    }

    #[test]
    fn filters_active_bots_case_insensitively_including_multiword_names() {
        let bot = |id, name, cleanup| {
            serde_json::from_value(serde_json::json!({
                "id": Uuid::from_u128(id), "name": name, "cleanup": cleanup,
            }))
            .unwrap()
        };
        let bots = vec![
            bot(1, "Proliquid Scout", serde_json::Value::Null),
            bot(2, "Todo", serde_json::Value::Null),
            bot(
                3,
                "Scout removed",
                serde_json::json!({"state": "done", "error": null}),
            ),
            bot(4, "SCOUT", serde_json::Value::Null),
        ];
        let query = MentionQuery::at("Hey @Sc", 7..7).unwrap();
        assert_eq!(
            query.candidates(&bots),
            vec![Uuid::from_u128(1), Uuid::from_u128(4)]
        );
        let query = MentionQuery::at("@missing", 8..8).unwrap();
        assert!(query.candidates(&bots).is_empty());
    }
}
