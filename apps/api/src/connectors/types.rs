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

/// How an inbox card in chat signs the owner in to a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardSignIn {
    /// A browser sign-in with this company. It may authorize another
    /// mailbox than the bot named.
    Browser(&'static str),
    /// An app-specific password typed into the card, for exactly the named
    /// mailbox. `help` says where the owner makes one.
    AppPassword {
        issuer: &'static str,
        help: &'static str,
    },
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
    pub health: Health,
}

/// What the runtime last saw of an account's mail server. It is stored and
/// sent as its code: `unknown`, `connected`, or a `MailError` code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Health {
    /// Not checked since the account or its credential changed.
    Unknown,
    Connected,
    /// The server rejected the saved credential. Only a later sign-in or a
    /// new credential clears it.
    CredentialRejected,
    /// The OAuth provider no longer accepts the saved grant, as when its
    /// refresh token expires. Only a new sign-in clears it.
    ReauthenticationRequired,
    /// Any other failure, by its error code.
    Failing(String),
}

impl Health {
    pub fn code(&self) -> &str {
        match self {
            Self::Unknown => "unknown",
            Self::Connected => "connected",
            Self::CredentialRejected => AUTHENTICATION_FAILED,
            Self::ReauthenticationRequired => REAUTHENTICATION_REQUIRED,
            Self::Failing(code) => code,
        }
    }

    /// Whether only a new credential or sign-in can restore the inbox.
    pub fn needs_new_credential(&self) -> bool {
        matches!(
            self,
            Self::CredentialRejected | Self::ReauthenticationRequired
        )
    }
}

impl From<String> for Health {
    fn from(code: String) -> Self {
        match code.as_str() {
            "unknown" => Self::Unknown,
            "connected" => Self::Connected,
            AUTHENTICATION_FAILED => Self::CredentialRejected,
            REAUTHENTICATION_REQUIRED => Self::ReauthenticationRequired,
            _ => Self::Failing(code),
        }
    }
}

impl From<Health> for String {
    fn from(health: Health) -> Self {
        health.code().to_owned()
    }
}

/// The error code of a server rejecting the saved credential, stored as
/// `Health::CredentialRejected`.
pub const AUTHENTICATION_FAILED: &str = "mail_authentication_failed";

/// The error code of an OAuth provider refusing the saved grant, stored as
/// `Health::ReauthenticationRequired`.
pub const REAUTHENTICATION_REQUIRED: &str = "oauth_reauthentication_required";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    pub read: bool,
    pub write: bool,
    pub send: bool,
    pub wake: bool,
}

/// A bot's trigger on an inbox it can read: whether new mail wakes it, and
/// what to do then.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MailTrigger {
    pub account_id: Uuid,
    pub address: String,
    pub state: String,
    pub health: String,
    pub wake: bool,
    pub instructions: String,
    pub last_woke_at: Option<String>,
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
    /// The Message-ID header, without angle brackets. An approved draft
    /// keeps one across retries, so its Sent copy can be found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
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
    /// The bytes of these attachments, by their part index from `read`.
    /// Only the server calls it, after the owner approves the files.
    OpenAttachments {
        message: MessageRef,
        parts: Vec<usize>,
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
    Send {
        draft: Draft,
    },
    Reply {
        message: MessageRef,
        body: String,
        #[serde(default)]
        all: bool,
        send: SendNow,
    },
    Forward {
        message: MessageRef,
        to: Vec<String>,
        body: String,
        send: SendNow,
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
    /// Whether the Sent folder holds a message with this Message-ID. Only
    /// the server calls it, before it sends an uncertain draft again.
    FindSent {
        message_id: String,
    },
    /// The whole message for the owner's reader: headers, full text, the
    /// HTML part with the images it shows, and the attachment list. Only
    /// the owner's route calls it.
    View {
        message: MessageRef,
    },
}
/// `send: true`, the only value reply and forward accept: they send at once,
/// and a draft for the owner goes to a `draft_email` card, not the mailbox.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(try_from = "bool", into = "bool")]
pub struct SendNow;

impl TryFrom<bool> for SendNow {
    type Error = &'static str;
    fn try_from(send: bool) -> Result<Self, Self::Error> {
        send.then_some(Self).ok_or("send must be true")
    }
}

impl From<SendNow> for bool {
    fn from(_: SendNow) -> bool {
        true
    }
}

/// Attachments one `open_attachments` call or approval card may cover.
pub const MAX_OPEN_ATTACHMENTS: usize = 10;
/// Decoded bytes one sent email's attachments may total. Base64 makes the
/// message about a third larger, which stays under Gmail's 25 MB.
pub const MAX_ATTACHMENT_TOTAL: usize = 15 * 1024 * 1024;
/// Base64 text of the largest attachment `MAX_ATTACHMENT_TOTAL` allows.
pub const MAX_ATTACHMENT_BASE64: usize = MAX_ATTACHMENT_TOTAL.div_ceil(3) * 4;

fn default_limit() -> usize {
    50
}
impl MailCommand {
    pub fn mutates(&self) -> bool {
        !matches!(
            self,
            Self::Folders
                | Self::Search { .. }
                | Self::Read { .. }
                | Self::OpenAttachments { .. }
                | Self::FindSent { .. }
                | Self::View { .. }
        )
    }
    pub fn sends(&self) -> bool {
        matches!(
            self,
            Self::Send { .. } | Self::Reply { .. } | Self::Forward { .. }
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

/// How far a send got. Only a send that wrote the whole message and then
/// lost the server's reply may have gone out; every earlier failure, and a
/// refusal of the written message, sent nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SendPhase {
    /// Composing the message, or reading the one it answers.
    Prepare,
    /// Connecting, TLS, and signing in.
    Connect,
    /// MAIL FROM and RCPT TO.
    Envelope,
    /// The DATA command and writing the message.
    Data,
    /// The whole message was written; waiting for the server's reply.
    Reply,
    /// The server refused the written message.
    Refused,
}

impl SendPhase {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn may_have_sent(self) -> bool {
        self == Self::Reply
    }

    fn during(self) -> &'static str {
        match self {
            Self::Prepare => "while preparing the message",
            Self::Connect => "while connecting and signing in",
            Self::Envelope => "while naming the sender and recipients",
            Self::Data => "while sending the message",
            Self::Reply => "while waiting for the server's reply after the whole message was sent",
            Self::Refused => "after the whole message was sent",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{code}{}", .detail.map(|d| format!(": {d}")).unwrap_or_default())]
pub struct MailError {
    pub code: &'static str,
    /// Which field failed and its allowed values, for the bot to correct.
    pub detail: Option<&'static str>,
    /// Set on a failed send: how far it got.
    pub phase: Option<SendPhase>,
    /// What went wrong, in words safe to log and show the owner: never
    /// message content or credentials.
    pub cause: Option<String>,
}
impl MailError {
    pub fn new(code: &'static str) -> Self {
        Self {
            code,
            detail: None,
            phase: None,
            cause: None,
        }
    }
    pub fn detail(self, detail: &'static str) -> Self {
        Self {
            detail: Some(detail),
            ..self
        }
    }
    pub fn cause(self, cause: impl Into<String>) -> Self {
        Self {
            cause: Some(cause.into()),
            ..self
        }
    }
    pub fn at(self, phase: SendPhase) -> Self {
        Self {
            phase: Some(phase),
            ..self
        }
    }
    /// A send that failed this way may still have reached its recipients.
    pub fn may_have_sent(&self) -> bool {
        self.phase.is_some_and(SendPhase::may_have_sent)
    }
    /// Why it failed, for the owner: "the server closed the connection
    /// while waiting for the server's reply after the whole message was sent".
    pub fn reason(&self) -> String {
        let cause = self.cause.as_deref().unwrap_or(match self.code {
            "mail_timeout" => "the mail server timed out",
            AUTHENTICATION_FAILED => "the mail server rejected the sign-in",
            REAUTHENTICATION_REQUIRED => "the account needs a new sign-in",
            "mail_tls_failed" => "the secure connection failed",
            "mail_transport_failed" => "the connection to the mail server failed",
            code => code,
        });
        match self.phase {
            Some(phase) => format!("{cause} {}", phase.during()),
            None => cause.to_owned(),
        }
    }
}

pub struct Auth {
    pub username: String,
    pub secret: zeroize::Zeroizing<String>,
    pub oauth: bool,
}

#[async_trait::async_trait]
pub trait MailTransport: Send + Sync {
    /// Signs in to IMAP and SMTP once, so a rejected password fails before
    /// it is saved.
    async fn verify(&self, config: &MailConfig, auth: &Auth) -> Result<(), MailError>;
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
