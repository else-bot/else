use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::bots::{Activity, Bot};
use crate::email::MailTrigger;
use crate::items::Item;
use crate::outgoing::DraftKind;
use crate::routines::{Routine, Watcher, Webhook};
use crate::socials::Trigger;
use crate::turns::Turn;

/// The per-bot lists a snapshot and each `bot.updated` carry. A
/// `bot.updated` without `routines` or `rules_version` keeps the current
/// value; the other lists reset when absent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BotExtras {
    /// Absent from servers without routines.
    #[serde(default)]
    pub routines: Option<Vec<Routine>>,
    #[serde(default)]
    pub social_triggers: Vec<Trigger>,
    /// Absent from servers without email wake-ups.
    #[serde(default)]
    pub mail_triggers: Vec<MailTrigger>,
    /// The kinds of drafts the owner approves always.
    #[serde(default)]
    pub draft_grants: Vec<DraftKind>,
    /// Absent from servers that don't list webhooks.
    #[serde(default)]
    pub webhooks: Vec<Webhook>,
    #[serde(default)]
    pub watchers: Vec<Watcher>,
    /// Absent from servers without rules.
    #[serde(default)]
    pub rules_version: Option<i64>,
}

impl BotExtras {
    /// Apply a `bot.updated`: absent routines and rules version keep their
    /// current value.
    pub fn update(&mut self, next: Self) {
        let routines = next.routines.or_else(|| self.routines.take());
        let rules_version = next.rules_version.or(self.rules_version);
        *self = Self {
            routines,
            rules_version,
            ..next
        };
    }
}

/// `GET /v1/bots/{id}/conversation`: the newest page or an older one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The desktop does not read it.
    #[serde(default)]
    pub conversation_id: Option<Uuid>,
    /// The desktop does not read it.
    #[serde(default)]
    pub bot_id: Option<Uuid>,
    pub selected_provider_id: Option<String>,
    pub event_cursor: String,
    pub items: Vec<Item>,
    pub turns: Vec<Turn>,
    pub activity: Activity,
    pub older_before_seq: Option<i64>,
    #[serde(flatten)]
    pub extras: BotExtras,
}

/// Shown items loaded around one message, between two the owner already
/// has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub items: Vec<Item>,
    pub turns: Vec<Turn>,
    /// Shown items still missing below the first item.
    pub older_gap: i64,
    /// Shown items still missing above the last item.
    pub newer_gap: i64,
}

/// The messages a search found, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHits {
    /// Matches in all of them.
    pub total: usize,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub item_id: Uuid,
    pub seq: i64,
    /// Matches in this message.
    pub count: usize,
}

/// The server's canonical owner item and turn for an accepted message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sent {
    pub item: Item,
    pub turn: Turn,
}

/// One event of a `bot:<id>` channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum Event {
    #[serde(rename = "item.upsert")]
    ItemUpsert(Box<ItemUpsert>),
    #[serde(rename = "item.delta")]
    ItemDelta(ItemDelta),
    #[serde(rename = "turn.updated")]
    TurnUpdated(Box<TurnUpdated>),
    #[serde(rename = "bot.updated")]
    BotUpdated(Box<BotUpdated>),
}

impl Event {
    pub fn kind(&self) -> EventKind {
        match self {
            Self::ItemUpsert(_) => EventKind::ItemUpsert,
            Self::ItemDelta(_) => EventKind::ItemDelta,
            Self::TurnUpdated(_) => EventKind::TurnUpdated,
            Self::BotUpdated(_) => EventKind::BotUpdated,
        }
    }
}

wire_state! {
    /// The `event` name of an [`Event`], so a reader can tell an event it
    /// does not know from a known one whose data does not parse.
    pub enum EventKind {
        ItemUpsert = "item.upsert",
        ItemDelta = "item.delta",
        TurnUpdated = "turn.updated",
        BotUpdated = "bot.updated",
    }
    _ => Unknown
}

/// The desktop reads neither ID of an event; tests and older servers may
/// omit them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemUpsert {
    #[serde(default)]
    pub bot_id: Option<Uuid>,
    #[serde(default)]
    pub conversation_id: Option<Uuid>,
    pub item: Item,
}

/// Text appended to a streaming reply at `offset_utf8` bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemDelta {
    #[serde(default)]
    pub bot_id: Option<Uuid>,
    #[serde(default)]
    pub conversation_id: Option<Uuid>,
    pub item_id: Uuid,
    pub offset_utf8: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnUpdated {
    #[serde(default)]
    pub bot_id: Option<Uuid>,
    #[serde(default)]
    pub conversation_id: Option<Uuid>,
    pub turn: Turn,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BotUpdated {
    #[serde(default)]
    pub bot_id: Option<Uuid>,
    pub bot: Bot,
    #[serde(flatten)]
    pub extras: BotExtras,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn each_event_name_reads_as_its_kind() {
        let turn = json!({
            "id": Uuid::nil(), "request_id": Uuid::nil(), "turn_no": 1, "provider_id": "codex",
            "status": "queued", "error": null, "effect_uncertain": false,
        });
        let item = json!({
            "id": Uuid::nil(), "seq": 1, "turn_id": Uuid::nil(), "request_id": null,
            "kind": "user", "text": "hi", "provider_id": null,
        });
        let events = [
            json!({ "event": "item.upsert", "data": { "item": item } }),
            json!({ "event": "item.delta", "data": { "item_id": Uuid::nil(), "offset_utf8": 0, "text": "" } }),
            json!({ "event": "turn.updated", "data": { "turn": turn } }),
            json!({ "event": "bot.updated", "data": { "bot": { "id": Uuid::nil(), "name": "Bot" } } }),
        ];
        let kinds: Vec<_> = events
            .iter()
            .map(|message| {
                let event = Event::deserialize(message).unwrap();
                assert_eq!(message["event"], event.kind().as_str());
                event.kind()
            })
            .collect();
        assert_eq!(
            kinds,
            [
                EventKind::ItemUpsert,
                EventKind::ItemDelta,
                EventKind::TurnUpdated,
                EventKind::BotUpdated,
            ]
        );
        assert_eq!(
            EventKind::deserialize(&json!("bot.renamed")).unwrap(),
            EventKind::Unknown
        );
    }

    #[test]
    fn a_bot_update_keeps_absent_routines_and_rules_version_and_resets_the_rest() {
        let mut extras: BotExtras = serde_json::from_value(json!({
            "routines": [], "rules_version": 3,
            "draft_grants": [{ "platform": "email", "account_id": null, "account": null,
                "destination": null }],
        }))
        .unwrap();
        extras.update(serde_json::from_value(json!({})).unwrap());
        assert_eq!(extras.routines, Some(vec![]));
        assert_eq!(extras.rules_version, Some(3));
        assert!(extras.draft_grants.is_empty());
        extras.update(serde_json::from_value(json!({ "rules_version": 4 })).unwrap());
        assert_eq!(extras.rules_version, Some(4));
    }
}
