use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

wire_state! {
    pub enum OwnerNoticeKind {
        Blocked = "blocked",
        Decision = "decision",
    }
    _ => Other(String)
}

wire_state! {
    pub enum OwnerNoticeStatus {
        Pending = "pending",
        Replied = "replied",
        Dismissed = "dismissed",
        /// A card on the same turn asks the owner instead.
        Superseded = "superseded",
    }
    _ => Other(String)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerNotice {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub turn_id: Uuid,
    pub kind: OwnerNoticeKind,
    pub summary: String,
    pub tried: Option<String>,
    pub status: OwnerNoticeStatus,
    pub created_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
}

/// An owner notice shown on an exchange, with the name of the bot asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExchangeAsk {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub bot_name: String,
    pub turn_id: Uuid,
    pub kind: OwnerNoticeKind,
    pub summary: String,
    #[serde(default)]
    pub tried: Option<String>,
    pub status: OwnerNoticeStatus,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub settled_at: Option<DateTime<Utc>>,
}

impl From<&ExchangeAsk> for OwnerNotice {
    fn from(ask: &ExchangeAsk) -> Self {
        Self {
            id: ask.id,
            bot_id: ask.bot_id,
            turn_id: ask.turn_id,
            kind: ask.kind.clone(),
            summary: ask.summary.clone(),
            tried: ask.tried.clone(),
            status: ask.status.clone(),
            created_at: ask.created_at,
            settled_at: ask.settled_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeResponse {
    pub notice: OwnerNotice,
}
