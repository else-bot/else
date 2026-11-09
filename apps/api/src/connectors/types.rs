use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Gmail,
    Microsoft,
    Yahoo,
    Icloud,
    Imap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub start_tls: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MailConfig {
    pub provider: Provider,
    pub address: String,
    pub username: String,
    pub imap: Endpoint,
    pub smtp: Endpoint,
    pub archive_folder: String,
    pub trash_folder: String,
    pub drafts_folder: String,
    pub sent_folder: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Credential {
    Password {
        password: String,
    },
    #[serde(rename = "oauth2")]
    OAuth2 {
        client_id: String,
        client_secret: Option<String>,
        refresh_token: String,
        access_token: Option<String>,
        expires_at: Option<i64>,
    },
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credential([redacted])")
    }
}

impl Drop for Credential {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        match self {
            Self::Password { password } => password.zeroize(),
            Self::OAuth2 {
                client_secret,
                refresh_token,
                access_token,
                ..
            } => {
                client_secret.zeroize();
                refresh_token.zeroize();
                access_token.zeroize();
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: Uuid,
    pub config: MailConfig,
    pub generation: i64,
    pub state: String,
    pub health: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    pub read: bool,
    pub write: bool,
    pub send: bool,
    pub wake: bool,
}

/// An inbox a bot can read, and whether its new mail wakes the bot.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MailTrigger {
    pub account_id: Uuid,
    pub address: String,
    pub state: String,
    pub health: String,
    pub wake: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MessageRef {
    pub folder: String,
    pub uid_validity: u32,
    pub uid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    #[serde(default)]
    pub attachments: Vec<OutgoingAttachment>,
    /// The message this answers: only its thread headers are used, never
    /// its recipients or subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutgoingAttachment {
    pub name: String,
    pub media_type: String,
    pub data_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum MailCommand {
    Folders,
    Search {
        folder: String,
        query: Option<String>,
        #[serde(default = "default_limit")]
        limit: usize,
    },
    Read {
        message: MessageRef,
    },
    Attachment {
        message: MessageRef,
        part: usize,
    },
    SetRead {
        message: MessageRef,
        read: bool,
    },
    SetFlagged {
        message: MessageRef,
        flagged: bool,
    },
    Move {
        message: MessageRef,
        folder: String,
    },
    Archive {
        message: MessageRef,
    },
    Trash {
        message: MessageRef,
    },
    Delete {
        message: MessageRef,
    },
    Draft {
        draft: Draft,
    },
    Send {
        draft: Draft,
    },
    Reply {
        message: MessageRef,
        body: String,
        #[serde(default)]
        all: bool,
        #[serde(default)]
        send: bool,
    },
    Forward {
        message: MessageRef,
        to: Vec<String>,
        body: String,
        #[serde(default)]
        send: bool,
    },
    CreateFolder {
        folder: String,
    },
    RenameFolder {
        folder: String,
        name: String,
    },
    DeleteFolder {
        folder: String,
    },
}
fn default_limit() -> usize {
    50
}
impl MailCommand {
    pub fn mutates(&self) -> bool {
        !matches!(
            self,
            Self::Folders | Self::Search { .. } | Self::Read { .. } | Self::Attachment { .. }
        )
    }
    pub fn sends(&self) -> bool {
        matches!(
            self,
            Self::Send { .. } | Self::Reply { send: true, .. } | Self::Forward { send: true, .. }
        )
    }
    pub fn allowed(&self, p: &Permissions) -> bool {
        p.read && (!self.mutates() || p.write) && (!self.sends() || p.send)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cursor {
    pub uid_validity: u32,
    pub last_uid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Arrival {
    pub message: MessageRef,
}

pub struct Scan {
    pub cursor: Cursor,
    pub arrivals: Vec<Arrival>,
    pub more: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("{code}")]
pub struct MailError {
    pub code: &'static str,
}
impl MailError {
    pub fn new(code: &'static str) -> Self {
        Self { code }
    }
}

pub struct Auth {
    pub username: String,
    pub secret: zeroize::Zeroizing<String>,
    pub oauth: bool,
}

#[async_trait::async_trait]
pub trait MailTransport: Send + Sync {
    async fn execute(
        &self,
        config: &MailConfig,
        auth: &Auth,
        command: &MailCommand,
    ) -> Result<Value, MailError>;
    async fn scan(
        &self,
        config: &MailConfig,
        auth: &Auth,
        cursor: Option<&Cursor>,
    ) -> Result<Scan, MailError>;
    async fn wait(
        &self,
        config: &MailConfig,
        auth: &Auth,
        cursor: &Cursor,
        stop: tokio_util::sync::CancellationToken,
    ) -> Result<(), MailError>;
}
