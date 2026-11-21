use serde::{Deserialize, Serialize};
use uuid::Uuid;

wire_state! {
    pub enum SetupStatus {
        Pending = "pending",
        Connected = "connected",
        Cancelled = "cancelled",
    }
    _ => Unknown
}

wire_state! {
    /// A connection's state as the server sends it; a newer server's state
    /// keeps its text so the panel can still show it.
    pub enum ConnectionState {
        Active = "active",
        Paused = "paused",
        Removed = "removed",
    }
    _ => Other(String)
}

wire_state! {
    /// How a connection's listener is doing; a newer server's health keeps
    /// its text so the panel can still show it.
    pub enum Health {
        Connecting = "connecting",
        Connected = "connected",
        Reconnecting = "reconnecting",
        /// Another receiver took the platform's single listener slot.
        ReceiverConflict = "receiver_conflict",
        /// The account signed this device out; the owner must pair it again.
        SignedOut = "signed_out",
        Paused = "paused",
        Removed = "removed",
        Restored = "restored",
    }
    _ => Other(String)
}

wire_state! {
    pub enum Provider {
        Telegram = "telegram",
        Discord = "discord",
        WhatsApp = "whatsapp",
    }
    _ => Unknown
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Telegram => "Telegram",
            Self::Discord => "Discord",
            Self::WhatsApp => "WhatsApp",
            Self::Unknown => "Unknown provider",
        }
    }

    pub fn portal(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Telegram => Some(("Open BotFather", "https://t.me/BotFather")),
            Self::Discord => Some((
                "Open Discord Developer Portal",
                "https://discord.com/developers/applications",
            )),
            Self::WhatsApp | Self::Unknown => None,
        }
    }

    /// Whether the owner links this provider by scanning a code instead of
    /// pasting a bot token.
    pub fn pairs(self) -> bool {
        match self {
            Self::Telegram | Self::Discord | Self::Unknown => false,
            Self::WhatsApp => true,
        }
    }

    pub fn token_help(self) -> &'static str {
        match self {
            Self::Telegram => {
                "Create a bot with @BotFather using /newbot. This token lets the API act as that bot in chats it joins. It does not connect your personal Telegram inbox."
            }
            Self::Discord => {
                "Create an application in the Discord Developer Portal and copy its bot token. This token lets the API act as that bot, within its server and channel permissions. It does not connect your personal account."
            }
            Self::WhatsApp => {
                "This links your personal WhatsApp account as a device, like WhatsApp Web. Bots read your chats and groups, and anything they send goes out as you, only after you approve it. WhatsApp does not allow unofficial clients and may ban accounts that act like bots."
            }
            Self::Unknown => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocialSetup {
    pub id: Uuid,
    pub bot_id: Option<Uuid>,
    pub turn_id: Option<Uuid>,
    pub provider: Provider,
    pub reason: String,
    pub status: SetupStatus,
    pub connection_id: Option<Uuid>,
    pub note: Option<String>,
}

/// What the pairing card shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Pairing {
    /// Waiting for a code, or linking the phone that scanned one.
    Starting,
    /// Scan `code` with WhatsApp → Linked devices before `expires_in` seconds.
    Code {
        code: String,
        expires_in: u64,
    },
    /// The connection holds the linked device.
    Paired {
        connection: Box<Connection>,
    },
    /// No code was scanned in time. Start again for a new one.
    Expired,
    Failed {
        message: String,
    },
    Cancelled,
    #[serde(other)]
    Unknown,
}

impl Pairing {
    pub fn finished(&self) -> bool {
        !matches!(self, Self::Starting | Self::Code { .. })
    }
}

/// A pairing as the card sees it. `revision` advances on every change, so
/// a card can wait for the next one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingView {
    pub revision: u64,
    #[serde(flatten)]
    pub pairing: Pairing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingEnvelope {
    pub pairing: PairingView,
}

impl SocialSetup {
    pub fn is_pending(&self) -> bool {
        self.status == SetupStatus::Pending
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub id: Uuid,
    pub provider: Provider,
    pub remote_id: String,
    pub display_name: String,
    pub username: Option<String>,
    pub state: ConnectionState,
    pub health: Health,
    pub generation: i64,
    pub instructions: String,
    #[serde(default)]
    pub message_content: bool,
}

impl Connection {
    /// Another webhook, app, or server takes this bot's messages, so none
    /// reach the connection.
    pub fn receiver_conflict(&self) -> bool {
        self.health == Health::ReceiverConflict
    }

    pub fn health_label(&self) -> &str {
        if self.receiver_conflict() {
            "messages go elsewhere"
        } else {
            self.health.as_str()
        }
    }
}

wire_state! {
    pub enum WakePolicy {
        Mentions = "mentions",
        All = "all",
    }
    _ => Unknown
}

impl WakePolicy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mentions => "on mention",
            Self::All => "every message",
            Self::Unknown => "unknown wake policy",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trigger {
    pub id: Uuid,
    pub connection_id: Uuid,
    pub bot_id: Uuid,
    pub provider: Provider,
    pub connection_name: String,
    pub username: Option<String>,
    pub destination: Option<String>,
    pub destination_name: Option<String>,
    pub wake: WakePolicy,
    pub instructions: String,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub last_woke_at: Option<String>,
}

impl Trigger {
    pub fn title(&self) -> &str {
        self.destination_name
            .as_deref()
            .or(self.destination.as_deref())
            .unwrap_or("Any chat")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionEnvelope {
    pub connection: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connections {
    pub connections: Vec<Connection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerEnvelope {
    pub trigger: Trigger,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupEnvelope {
    pub setup: SocialSetup,
}
