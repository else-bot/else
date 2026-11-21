use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

wire_state! {
    /// A bot's sidebar activity, projected from its turns.
    pub enum Phase {
        Idle = "idle",
        Queued = "queued",
        Working = "working",
        Cancelling = "cancelling",
    }
    _ => Unknown
}

/// An active bot, or a deletion receipt (`cleanup` set). A receipt sets
/// only `id`, `name`, `avatar`, `deleted_at` and `cleanup`; its other
/// fields are null, false or zero, or omitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bot {
    pub id: Uuid,
    pub name: String,
    /// Absent from older servers, which draw every bot the same.
    #[serde(default)]
    pub avatar: Avatar,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<Uuid>,
    /// Absent from receipts and older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    /// The newest owner or bot message; `None` before the first one and
    /// from older servers.
    #[serde(default)]
    pub last_message_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_revision: Option<i64>,
    #[serde(default)]
    pub deleted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub selected_provider_id: Option<String>,
    /// Its sidebar category; absent (older servers, receipts) or null means
    /// unassigned.
    #[serde(default)]
    pub category_id: Option<Uuid>,
    /// The owner silenced it: unread and announced only while it waits on
    /// the owner. Absent from older servers.
    #[serde(default)]
    pub muted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<Activity>,
    #[serde(default)]
    pub sidebar_status: Option<SidebarStatus>,
    /// Absent from older servers and on receipts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_state: Option<ReadState>,
    #[serde(default)]
    pub watching: usize,
    #[serde(default)]
    pub cleanup: Option<Cleanup>,
}

impl Bot {
    pub fn is_active(&self) -> bool {
        self.cleanup.is_none()
    }

    /// The open request this bot waits on the owner to settle, if any.
    pub fn waiting(&self) -> Option<Waiting> {
        self.sidebar_status.as_ref()?.waiting()
    }

    /// Muted and not waiting on the owner: never unread and never announced.
    pub fn silenced(&self) -> bool {
        self.muted && self.waiting().is_none()
    }
}

/// How far a deleted bot's cleanup got.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cleanup {
    pub state: CleanupState,
    pub error: Option<serde_json::Value>,
}

wire_state! {
    pub enum CleanupState {
        Pending = "pending",
        Done = "done",
    }
    _ => Unknown
}

/// `{"bot": ...}`, the body of each single-bot response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotEnvelope {
    pub bot: Bot,
}

/// `GET /v1/bots` and the `bots.snapshot` stream event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotList {
    pub default_bot_id: Option<Uuid>,
    /// Absent from servers without categories.
    #[serde(default)]
    pub categories: Vec<Category>,
    pub bots: Vec<Bot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub phase: Phase,
    pub active_turn_id: Option<Uuid>,
    pub queued_count: u32,
    /// Set while the active turn is parked on tasks it handed to other
    /// bots or on its own jobs; absent from older servers.
    #[serde(default)]
    pub waiting_on: Option<WaitingOn>,
    /// Always 0; kept for older desktops.
    #[serde(default)]
    pub running_job_count: u32,
    /// Always null; kept for older desktops.
    #[serde(default)]
    pub error: Option<serde_json::Value>,
}

/// What a parked turn waits on: the bots doing its tasks, oldest task
/// first, and its own pending jobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitingOn {
    /// When its oldest unfinished task or job wait began.
    pub since: DateTime<Utc>,
    pub bots: Vec<WaitingBot>,
    /// Its pending MCP and computer job waits; absent from older servers,
    /// which report only tasks.
    #[serde(default)]
    pub jobs: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitingBot {
    pub id: Uuid,
    pub name: String,
    /// Its unfinished tasks from this turn.
    pub tasks: u32,
}

/// What the sidebar shows under a bot. `since` is when the open request
/// was made; older servers omit it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SidebarStatus {
    WaitingForAnswer {
        preview: String,
        #[serde(default)]
        since: Option<DateTime<Utc>>,
    },
    NeedsApproval {
        preview: String,
        #[serde(default)]
        since: Option<DateTime<Utc>>,
    },
    NeedsTakeover {
        preview: String,
        #[serde(default)]
        since: Option<DateTime<Utc>>,
    },
    NeedsCredentials {
        preview: String,
        #[serde(default)]
        since: Option<DateTime<Utc>>,
    },
    /// A task delegated to the bot ended needing the owner.
    NeedsYou {
        preview: String,
        #[serde(default)]
        since: Option<DateTime<Utc>>,
        /// The ask shown, so a desktop announces each ask once. Absent from
        /// servers before #508.
        #[serde(default)]
        notice_id: Option<Uuid>,
    },
    /// The owner took over the bot's computer and has not handed it back.
    HasComputer {
        #[serde(default)]
        since: Option<DateTime<Utc>>,
    },
    Failed,
    Stopped,
    Uncertain,
    #[serde(other)]
    Unknown,
}

/// A request the bot waits on the owner to settle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Waiting {
    pub since: Option<DateTime<Utc>>,
}

impl SidebarStatus {
    pub fn waiting(&self) -> Option<Waiting> {
        match self {
            Self::WaitingForAnswer { since, .. }
            | Self::NeedsApproval { since, .. }
            | Self::NeedsTakeover { since, .. }
            | Self::NeedsCredentials { since, .. }
            | Self::NeedsYou { since, .. } => Some(Waiting { since: *since }),
            Self::HasComputer { .. }
            | Self::Failed
            | Self::Stopped
            | Self::Uncertain
            | Self::Unknown => None,
        }
    }
}

/// A point in a bot's replies: the assistant item, then the UTF-8 bytes of
/// its text. Field order makes the derived ordering lexicographic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReplyPosition {
    pub item_seq: i64,
    pub text_bytes: i64,
}

/// The newest reply of a turn in the owner conversation that ended or
/// waits on the owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatestReply {
    pub turn_id: Uuid,
    pub item_id: Uuid,
    pub position: ReplyPosition,
    #[serde(default)]
    pub preview: String,
}

impl LatestReply {
    /// The preview as one line, for surfaces that cannot render markdown.
    /// The server already strips the markdown, so parsing again would turn
    /// literal `*stars*` into emphasis.
    pub fn plain_preview(&self) -> String {
        self.preview
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The owner's saved read state for a bot's owner conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadState {
    pub latest: Option<LatestReply>,
    pub read_through: ReplyPosition,
    pub marked_unread: bool,
    /// Absent from older servers.
    #[serde(default)]
    pub unread: bool,
    /// Replies past `read_through`; absent from older servers.
    #[serde(default)]
    pub unread_replies: Option<u32>,
}

/// A sidebar category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Category {
    pub id: Uuid,
    pub name: String,
    /// The owner's sidebar place; absent from older servers, null when
    /// never placed.
    #[serde(default)]
    pub position: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryList {
    pub categories: Vec<Category>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryEnvelope {
    pub category: Category,
}

/// A preset avatar: one of the shapes the desktop draws, in one of its
/// named colors. A shape or color this desktop does not know reads as the
/// default one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "LenientAvatar")]
pub struct Avatar {
    pub shape: AvatarShape,
    pub color: AvatarColor,
}

impl Default for Avatar {
    fn default() -> Self {
        Self {
            shape: AvatarShape::Star,
            color: AvatarColor::Blue,
        }
    }
}

#[derive(Deserialize)]
struct LenientAvatar {
    #[serde(default, deserialize_with = "lenient")]
    shape: Option<AvatarShape>,
    #[serde(default, deserialize_with = "lenient")]
    color: Option<AvatarColor>,
}

fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

impl From<LenientAvatar> for Avatar {
    fn from(raw: LenientAvatar) -> Self {
        let fallback = Self::default();
        Self {
            shape: raw.shape.unwrap_or(fallback.shape),
            color: raw.color.unwrap_or(fallback.color),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvatarShape {
    Star,
    Round,
    Cloud,
    Tile,
    Sparkle,
    Flower,
    Egg,
    Bean,
    Cone,
    House,
    Gem,
    Ghost,
}

impl AvatarShape {
    pub const ALL: [Self; 12] = [
        Self::Star,
        Self::Round,
        Self::Cloud,
        Self::Tile,
        Self::Sparkle,
        Self::Flower,
        Self::Egg,
        Self::Bean,
        Self::Cone,
        Self::House,
        Self::Gem,
        Self::Ghost,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvatarColor {
    White,
    Brown,
    Red,
    Orange,
    Amber,
    Yellow,
    Green,
    Teal,
    Blue,
    Purple,
    Pink,
    Gray,
}

impl AvatarColor {
    pub const ALL: [Self; 12] = [
        Self::White,
        Self::Brown,
        Self::Red,
        Self::Orange,
        Self::Amber,
        Self::Yellow,
        Self::Green,
        Self::Teal,
        Self::Blue,
        Self::Purple,
        Self::Pink,
        Self::Gray,
    ];
}
