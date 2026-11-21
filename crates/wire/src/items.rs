use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
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
    #[serde(default)]
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
    #[serde(default)]
    pub parts: Vec<MessagePart>,
    /// The provider attempt that wrote an assistant item. Absent from older servers.
    #[serde(default)]
    pub attempt: Option<i64>,
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
