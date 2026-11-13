use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub struct SendMessage {
    pub destination: String,
    pub body: String,
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Provider {
    Telegram,
    Discord,
}
impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::Discord => "discord",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Telegram => "Telegram",
            Self::Discord => "Discord",
        }
    }
    pub fn supports(self, operation: CommandKind) -> bool {
        match operation {
            CommandKind::CreateThread => self == Self::Discord,
            CommandKind::Poll | CommandKind::Members => self == Self::Telegram,
            CommandKind::Conversations
            | CommandKind::History
            | CommandKind::Search
            | CommandKind::Message
            | CommandKind::Chat
            | CommandKind::Edit
            | CommandKind::Delete
            | CommandKind::React
            | CommandKind::Pin
            | CommandKind::Unpin => true,
        }
    }
    pub fn limitations(self) -> &'static [&'static str] {
        match self {
            Self::Telegram => &[
                "A BotFather token connects a bot identity, not your personal Telegram inbox.",
                "History and search retain at most 5000 observed messages per connection, not arbitrary older chats.",
                "Add the bot to a group or message it first. Group privacy mode controls which messages it receives.",
                "A public channel or group also works by its @username or t.me link. Posting in a channel needs the bot as an administrator.",
            ],
            Self::Discord => &[
                "A bot token acts as your Discord application, not your personal user account.",
                "The bot can access only channels permitted by its server roles.",
                "Gateway notifications use mentions and DMs. Wake All requests Message Content Intent, which must be enabled in the Developer Portal.",
                "Search covers at most 5000 observed messages per connection. History uses accessible channel history.",
            ],
        }
    }
}

pub struct Token(pub zeroize::Zeroizing<String>);
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token([redacted])")
    }
}
impl Token {
    pub fn parse(value: String) -> Result<Self, Error> {
        let value = zeroize::Zeroizing::new(value);
        if value.len() < 10
            || value.len() > 1024
            || value.chars().any(char::is_whitespace)
            || value.chars().any(char::is_control)
        {
            return Err(Error::Invalid("Enter a bot token without whitespace."));
        }
        Ok(Self(value))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum ConnectionState {
    Active,
    Paused,
    Removed,
}
impl ConnectionState {
    /// The state to write when the owner asks for `to`, or `None` when the
    /// connection is already there. A removed connection never comes back.
    pub fn transition(self, to: Self) -> Result<Option<Self>, Error> {
        match self {
            Self::Removed => Err(Error::NotFound),
            from if from == to => Ok(None),
            _ => Ok(Some(to)),
        }
    }
    /// The health a connection reports on entering this state.
    pub fn health(self) -> &'static str {
        match self {
            Self::Active => "connecting",
            Self::Paused => "paused",
            Self::Removed => "removed",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum ActionStatus {
    Dispatching,
    Succeeded,
    Failed,
    /// The platform may or may not have applied the action.
    Uncertain,
}
impl ActionStatus {
    pub fn settled(result: &Result<Value, Error>) -> Self {
        match result {
            Ok(_) => Self::Succeeded,
            Err(Error::Invalid(_) | Error::Unsupported | Error::Forbidden) => Self::Failed,
            Err(_) => Self::Uncertain,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Connection {
    pub id: Uuid,
    pub provider: Provider,
    pub remote_id: String,
    pub display_name: String,
    pub username: Option<String>,
    pub state: ConnectionState,
    pub health: String,
    pub generation: i64,
    pub instructions: String,
    #[sqlx(default)]
    #[serde(default)]
    pub message_content: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub id: String,
    pub display_name: String,
    pub username: Option<String>,
}
/// How an owner names a public Telegram chat: `@name` or its share link.
/// Returns `@name` for a public username, `None` for anything else, and
/// an error for a private invite link, which a bot cannot look up.
pub fn public_username(destination: &str) -> Result<Option<String>, Error> {
    let text = destination.trim();
    let name = if let Some(name) = text.strip_prefix('@') {
        name
    } else {
        let host = text
            .strip_prefix("https://")
            .or_else(|| text.strip_prefix("http://"))
            .unwrap_or(text);
        let Some(path) = ["t.me/", "telegram.me/", "www.t.me/", "www.telegram.me/"]
            .iter()
            .find_map(|prefix| host.strip_prefix(prefix))
        else {
            return Ok(None);
        };
        if path.starts_with('+') || path.starts_with("joinchat/") {
            return Err(Error::Invalid(
                "A private Telegram invite link has no public username. Post in the chat so it appears in social_connections, then use its chat ID.",
            ));
        }
        let path = path.trim_prefix("s/");
        path.split(['/', '?', '#']).next().unwrap_or_default()
    };
    let valid = (4..=32).contains(&name.len())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid {
        Ok(Some(format!("@{name}")))
    } else {
        Err(Error::Invalid(
            "Use a Telegram chat ID, a public @username, or a t.me link.",
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Destination {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub can_send: bool,
}
/// What one turn may do on a connection. Work for the owner may read any
/// chat and request mutations, each still approved per call. A turn an
/// incoming message started may only read the chats that woke it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Owner,
    Woken(Vec<String>),
}
impl Access {
    pub fn allows(&self, destination: &str) -> bool {
        match self {
            Self::Owner => true,
            Self::Woken(chats) => chats.iter().any(|chat| chat == destination),
        }
    }
    pub fn mutates(&self) -> bool {
        matches!(self, Self::Owner)
    }
}

/// When a trigger wakes its bot.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(rename_all = "lowercase")]
pub enum Wake {
    /// A mention of the bot, or any direct message.
    Mentions,
    All,
}

/// A bot's own rule to wake on incoming messages, set for an owner request.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Trigger {
    pub id: Uuid,
    pub connection_id: Uuid,
    pub bot_id: Uuid,
    pub provider: Provider,
    pub connection_name: String,
    pub username: Option<String>,
    /// `None` wakes on any chat the platform delivers.
    pub destination: Option<String>,
    pub destination_name: Option<String>,
    pub wake: Wake,
    pub instructions: String,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
    /// When a message last woke the bot through this trigger.
    pub last_woke_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerInput {
    pub connection_id: Uuid,
    #[serde(default)]
    pub destination: Option<String>,
    pub wake: Wake,
    pub instructions: String,
}
impl TriggerInput {
    pub fn validate(&self) -> Result<(), Error> {
        if self
            .destination
            .as_deref()
            .is_some_and(|d| d.is_empty() || d.len() > 100 || d.chars().any(char::is_control))
        {
            return Err(Error::Invalid(
                "Use a chat ID from social_connections, or omit destination for any chat.",
            ));
        }
        let instructions = self.instructions.trim();
        if instructions.is_empty() || instructions.len() > 4000 || instructions.contains('\0') {
            return Err(Error::Invalid(
                "Describe what to do on a match in 1 to 4000 bytes.",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Conversations,
    History {
        destination: String,
        #[serde(default)]
        before: Option<String>,
        #[serde(default = "default_limit")]
        limit: u16,
    },
    Search {
        destination: String,
        query: String,
        #[serde(default = "default_limit")]
        limit: u16,
    },
    Message {
        destination: String,
        message_id: String,
    },
    Chat {
        destination: String,
    },
    Members {
        destination: String,
        #[serde(default = "default_limit")]
        limit: u16,
    },
    Edit {
        destination: String,
        message_id: String,
        body: String,
    },
    Delete {
        destination: String,
        message_id: String,
    },
    React {
        destination: String,
        message_id: String,
        emoji: String,
    },
    Pin {
        destination: String,
        message_id: String,
    },
    Unpin {
        destination: String,
        message_id: String,
    },
    CreateThread {
        destination: String,
        message_id: String,
        name: String,
    },
    Poll {
        destination: String,
        question: String,
        options: Vec<String>,
    },
}
fn default_limit() -> u16 {
    50
}
impl Command {
    pub fn operation(&self) -> CommandKind {
        match self {
            Self::Conversations => CommandKind::Conversations,
            Self::History { .. } => CommandKind::History,
            Self::Search { .. } => CommandKind::Search,
            Self::Message { .. } => CommandKind::Message,
            Self::Chat { .. } => CommandKind::Chat,
            Self::Members { .. } => CommandKind::Members,
            Self::Edit { .. } => CommandKind::Edit,
            Self::Delete { .. } => CommandKind::Delete,
            Self::React { .. } => CommandKind::React,
            Self::Pin { .. } => CommandKind::Pin,
            Self::Unpin { .. } => CommandKind::Unpin,
            Self::CreateThread { .. } => CommandKind::CreateThread,
            Self::Poll { .. } => CommandKind::Poll,
        }
    }
    pub fn destination(&self) -> Option<&str> {
        match self {
            Self::Conversations => None,
            Self::History { destination, .. }
            | Self::Search { destination, .. }
            | Self::Message { destination, .. }
            | Self::Chat { destination }
            | Self::Members { destination, .. }
            | Self::Edit { destination, .. }
            | Self::Delete { destination, .. }
            | Self::React { destination, .. }
            | Self::Pin { destination, .. }
            | Self::Unpin { destination, .. }
            | Self::CreateThread { destination, .. }
            | Self::Poll { destination, .. } => Some(destination),
        }
    }
    pub fn destination_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Conversations => None,
            Self::History { destination, .. }
            | Self::Search { destination, .. }
            | Self::Message { destination, .. }
            | Self::Chat { destination }
            | Self::Members { destination, .. }
            | Self::Edit { destination, .. }
            | Self::Delete { destination, .. }
            | Self::React { destination, .. }
            | Self::Pin { destination, .. }
            | Self::Unpin { destination, .. }
            | Self::CreateThread { destination, .. }
            | Self::Poll { destination, .. } => Some(destination),
        }
    }
    pub fn sends(&self) -> bool {
        self.operation().sends()
    }
    pub fn mutates(&self) -> bool {
        self.operation().mutates()
    }
    pub fn validate(&self) -> Result<(), Error> {
        if self
            .destination()
            .is_some_and(|s| s.is_empty() || s.len() > 100)
        {
            return Err(Error::Invalid("Invalid destination."));
        }
        let valid = match self {
            Self::History { limit, .. }
            | Self::Search { limit, .. }
            | Self::Members { limit, .. } => (1..=100).contains(limit),
            Self::Edit { body, .. } => !body.trim().is_empty() && body.len() <= 4000,
            Self::React { emoji, .. } => !emoji.is_empty() && emoji.len() <= 100,
            Self::CreateThread { name, .. } => !name.trim().is_empty() && name.len() <= 100,
            Self::Poll {
                question, options, ..
            } => {
                !question.trim().is_empty()
                    && question.len() <= 300
                    && (2..=10).contains(&options.len())
                    && options
                        .iter()
                        .all(|s| !s.trim().is_empty() && s.len() <= 100)
            }
            _ => true,
        };
        if valid {
            Ok(())
        } else {
            Err(Error::Invalid("Invalid social operation parameters."))
        }
    }
}

#[derive(Debug, Clone)]
pub struct Incoming {
    pub event_id: String,
    pub destination: Destination,
    pub message_id: String,
    pub author_id: String,
    pub body: String,
    pub mentioned: bool,
    pub direct: bool,
    pub from_bot: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("The connection or setup was not found.")]
    NotFound,
    #[error("This task has no permission for this connection or destination.")]
    Forbidden,
    #[error("The connection changed. Reload it before retrying.")]
    Inactive,
    #[error("An existing connection already uses this bot identity.")]
    Duplicate,
    #[error("This action ID was already used for a different operation.")]
    Conflict,
    #[error("This operation is not supported for this provider.")]
    Unsupported,
    #[error("The platform could not complete the operation. Check credentials and permissions.")]
    Platform,
    #[error(
        "The platform response was interrupted. The action may have happened; do not retry with a new action ID."
    )]
    Uncertain,
    #[error("The connection is busy. Retry later.")]
    Busy,
    #[error(
        "Another webhook, app, or server receives this bot's messages. Telegram delivers them to only one receiver."
    )]
    ReceiverConflict,
    #[error("Encrypted credentials are unavailable.")]
    Crypto,
    #[error("The connector database operation failed.")]
    Db(#[from] sqlx::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    Conversations,
    History,
    Search,
    Message,
    Chat,
    Members,
    Edit,
    Delete,
    React,
    Pin,
    Unpin,
    CreateThread,
    Poll,
}
impl CommandKind {
    pub fn mutates(self) -> bool {
        match self {
            Self::Conversations
            | Self::History
            | Self::Search
            | Self::Message
            | Self::Chat
            | Self::Members => false,
            Self::Edit
            | Self::Delete
            | Self::React
            | Self::Pin
            | Self::Unpin
            | Self::CreateThread
            | Self::Poll => true,
        }
    }
    pub fn sends(self) -> bool {
        self == Self::Poll
    }
}
pub const OPERATIONS: &[CommandKind] = &[
    CommandKind::Conversations,
    CommandKind::History,
    CommandKind::Search,
    CommandKind::Message,
    CommandKind::Chat,
    CommandKind::Members,
    CommandKind::Edit,
    CommandKind::Delete,
    CommandKind::React,
    CommandKind::Pin,
    CommandKind::Unpin,
    CommandKind::CreateThread,
    CommandKind::Poll,
];
pub fn definitions() -> Value {
    serde_json::json!({"providers":([Provider::Telegram,Provider::Discord].map(|p|serde_json::json!({"provider":p,"label":p.label(),"token_help":format!("Connect the {} bot identity. The API stores the token encrypted and uses it for native tools; the assistant never receives the token.",p.label()),"capabilities":OPERATIONS.iter().filter(|op|p.supports(**op)).collect::<Vec<_>>(),"limitations":p.limitations()})))})
}

pub struct Envelope {
    pub incoming: Incoming,
    pub next_offset: i64,
    pub ack: tokio::sync::oneshot::Sender<()>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_public_telegram_chat_is_named_by_username_or_share_link() {
        for named in [
            "@proliquid_test_alerts",
            "t.me/proliquid_test_alerts",
            "https://t.me/proliquid_test_alerts",
            "http://telegram.me/proliquid_test_alerts/",
            "https://t.me/s/proliquid_test_alerts",
            "https://t.me/proliquid_test_alerts/42?single",
        ] {
            assert_eq!(
                public_username(named).unwrap().as_deref(),
                Some("@proliquid_test_alerts"),
                "{named}"
            );
        }
        assert_eq!(
            public_username("t.me/joinchat_news").unwrap().as_deref(),
            Some("@joinchat_news"),
            "a public username may start with joinchat"
        );
        for id in ["-1001234567890", "100", "123456789012345678"] {
            assert_eq!(public_username(id).unwrap(), None, "{id}");
        }
        for refused in [
            "https://t.me/+AbCdEf123",
            "t.me/joinchat/AbCd",
            "@x",
            "@bad name",
        ] {
            assert!(public_username(refused).is_err(), "{refused}");
        }
    }
}
