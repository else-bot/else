use chrono::{DateTime, Utc};
use serde::de::IntoDeserializer as _;
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::notices::ExchangeAsk;
use crate::rules::RulesUpdate;

wire_state! {
    pub enum ItemKind {
        User = "user",
        Assistant = "assistant",
        Status = "status",
        Routine = "routine",
        /// A connector event, shown only to a bot reading its history.
        Connector = "connector",
    }
    _ => Unknown
}

/// The largest file a message may carry, in bytes.
pub const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
/// The most files one message may carry.
pub const MAX_FILES: usize = 8;

/// One conversation message: the owner's, the bot's, or a receipt.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: Uuid,
    pub seq: i64,
    pub turn_id: Uuid,
    pub request_id: Option<Uuid>,
    pub kind: ItemKind,
    pub text: String,
    pub provider_id: Option<String>,
    /// Absent from older servers.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// Set on the owner's feedback message sent as a reaction.
    #[serde(default, deserialize_with = "known_reaction")]
    pub reaction: Option<Reaction>,
    #[serde(default)]
    pub bot_task: Option<BotTask>,
    #[serde(default)]
    pub bot_reply: Option<BotReply>,
    #[serde(default)]
    pub routine_update: Option<RoutineUpdate>,
    #[serde(default)]
    pub bot_created: Option<BotCreated>,
    #[serde(default)]
    pub rules_update: Option<RulesUpdate>,
    #[serde(default, deserialize_with = "known_parts")]
    pub parts: Vec<MessagePart>,
    /// The provider attempt that wrote an assistant item. Absent from older servers.
    #[serde(default)]
    pub attempt: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemEnvelope {
    pub item: Item,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Items {
    pub items: Vec<Item>,
}

/// A stored file scoped to one bot. Messages reference its ID, never a client path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: Uuid,
    pub name: String,
    pub media_type: String,
    pub size: u64,
}

impl Attachment {
    /// How the file appears in a message's plain text.
    pub fn fallback(&self) -> String {
        format!("[File: {}]", self.name)
    }
}

/// Ordered text or a stored file in a conversation message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePart {
    Text { text: String },
    File { file: Attachment },
}

impl MessagePart {
    /// A message's plain text: its text parts, with each file spelled as
    /// [`Attachment::fallback`]. The server requires `text` to equal it.
    pub fn flatten(parts: &[MessagePart]) -> String {
        parts
            .iter()
            .map(|part| match part {
                MessagePart::Text { text } => text.clone(),
                MessagePart::File { file } => file.fallback(),
            })
            .collect()
    }
}

/// Reads a reaction a newer server added as none; the reacting message's
/// text still says it. [`ReactionKind`] stays strict, so a desktop sends
/// only the reactions it knows.
pub(crate) fn known_reaction<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Reaction>, D::Error> {
    #[derive(Deserialize)]
    struct Received {
        item_id: Uuid,
        kind: String,
    }
    let Some(Received { item_id, kind }) = Option::deserialize(deserializer)? else {
        return Ok(None);
    };
    let kind: Result<ReactionKind, serde::de::value::Error> =
        ReactionKind::deserialize(kind.into_deserializer());
    Ok(kind.ok().map(|kind| Reaction { item_id, kind }))
}

/// Reads a message holding a part a newer server added as plain text: the
/// item's `text` carries every part, and the desktop's offsets into it hold
/// only when it knows each part's length. [`MessagePart`] stays strict, so
/// a desktop sends only the parts it knows.
fn known_parts<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<MessagePart>, D::Error> {
    #[derive(Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum Tag {
        Text,
        File,
        #[serde(other)]
        Other,
    }
    let mut unknown = false;
    let mut known = Vec::new();
    for part in Vec::<serde_json::Value>::deserialize(deserializer)? {
        if matches!(Tag::deserialize(&part), Ok(Tag::Other)) {
            unknown = true;
        } else {
            known.push(MessagePart::deserialize(part).map_err(serde::de::Error::custom)?);
        }
    }
    Ok(if unknown { Vec::new() } else { known })
}

/// The owner's reaction to one of the bot's messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub item_id: Uuid,
    pub kind: ReactionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactionKind {
    ThumbsUp,
    ThumbsDown,
}

/// The main bot's notice that `creator_bot_id` created `bot_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotCreated {
    pub bot_id: Uuid,
    pub name: String,
    pub creator_bot_id: Uuid,
    pub creator_name: String,
    pub purpose: String,
}

/// The receipt of a routine a bot renamed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineUpdate {
    pub routine_id: Uuid,
    pub name: String,
}

/// The receipt of a task another bot sent, in the receiving bot's chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotTask {
    pub sender_bot_id: Uuid,
    pub sender_name: String,
    pub prompt: String,
    /// The delegating bot's short name for the task. Absent from older
    /// servers and older tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_need: Option<NeedKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_line: Option<String>,
}

impl BotTask {
    pub fn declared(&self) -> Option<OwnerNeed<'_>> {
        OwnerNeed::declared(self.owner_need.as_ref(), self.owner_line.as_deref())
    }
}

/// A task's outcome, in the delegating bot's chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotReply {
    pub sender_bot_id: Uuid,
    pub sender_name: String,
    pub prompt: String,
    pub status: BotReplyStatus,
    /// Why a failed task failed, apart from the progress it wrote. Absent
    /// from older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_need: Option<NeedKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_line: Option<String>,
    /// The owner notices of the task and of the tasks it delegated, oldest
    /// first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asks: Vec<ExchangeAsk>,
}

impl BotReply {
    pub fn declared(&self) -> Option<OwnerNeed<'_>> {
        OwnerNeed::declared(self.owner_need.as_ref(), self.owner_line.as_deref())
    }
}

wire_state! {
    /// The delegation status behind a task's outcome.
    pub enum BotReplyStatus {
        Completed = "completed",
        Failed = "failed",
        Cancelled = "cancelled",
    }
    _ => Other(String)
}

/// What a delegated task declared it needs from the owner. The bot's
/// judgment, shown as its words, never an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerNeed<'a> {
    Nothing,
    /// Done, with the bot's one line for the owner.
    Fyi(&'a str),
    Decision,
    Blocked,
}

wire_state! {
    /// The need a task declared, as written.
    pub enum NeedKind {
        Nothing = "nothing",
        Fyi = "fyi",
        Decision = "decision",
        Blocked = "blocked",
    }
    _ => Other(String)
}

impl<'a> OwnerNeed<'a> {
    /// `None` when the task declared nothing: an older server or task, or a
    /// bot that forgot. Never read as "nothing".
    fn declared(need: Option<&NeedKind>, line: Option<&'a str>) -> Option<Self> {
        match (need?, line) {
            (NeedKind::Nothing, _) => Some(Self::Nothing),
            (NeedKind::Fyi, Some(line)) => Some(Self::Fyi(line)),
            (NeedKind::Decision, _) => Some(Self::Decision),
            (NeedKind::Blocked, _) => Some(Self::Blocked),
            (NeedKind::Fyi, None) | (NeedKind::Other(_), _) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_known_part_that_does_not_parse_fails_the_item_even_beside_an_unknown_one() {
        let item = json!({
            "id": Uuid::nil(), "seq": 1, "turn_id": Uuid::nil(), "request_id": null,
            "kind": "user", "text": "hi", "provider_id": null,
            "parts": [{ "type": "poll" }, { "type": "file" }],
        });
        assert!(Item::deserialize(&item).is_err());
    }
}
