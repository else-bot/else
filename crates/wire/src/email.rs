use serde::{Deserialize, Serialize};
use uuid::Uuid;

wire_state! {
    pub enum AccountState {
        Active = "active",
        Paused = "paused",
        Removed = "removed",
    }
    _ => Unknown
}

wire_state! {
    /// What the server last saw of an account's mail server.
    pub enum AccountHealth {
        /// Not checked since the account or its credential changed.
        Unchecked = "unknown",
        Connected = "connected",
    }
    /// Any failure, sent as its error code.
    _ => Failing(String)
}

wire_state! {
    pub enum EmailProvider {
        Gmail = "gmail",
        Microsoft = "microsoft",
        Yahoo = "yahoo",
        Icloud = "icloud",
        Imap = "imap",
    }
    _ => Unknown
}

impl EmailProvider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Gmail => "Gmail",
            Self::Microsoft => "Microsoft 365",
            Self::Yahoo => "Yahoo",
            Self::Icloud => "iCloud",
            Self::Imap | Self::Unknown => "Other email",
        }
    }
}

wire_state! {
    pub enum Authentication {
        AppPassword = "app_password",
        OAuth2 = "oauth2",
    }
    _ => Unknown
}

wire_state! {
    pub enum HtmlOmission {
        TooLarge = "too_large",
    }
    _ => Unknown
}

/// Where a message lives in a mailbox.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct MessageRef {
    pub folder: String,
    pub uid_validity: u32,
    pub uid: u32,
}

/// An email as the owner's reader shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailMessage {
    #[serde(default)]
    pub message: Option<MessageRef>,
    /// The sender's Date header, RFC 3339 when it parsed.
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub received_at: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub from: Vec<EmailPerson>,
    #[serde(default)]
    pub to: Vec<EmailPerson>,
    #[serde(default)]
    pub cc: Vec<EmailPerson>,
    #[serde(default)]
    pub reply_to: Vec<EmailPerson>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub text_truncated: bool,
    /// The message's own HTML part, when it has one small enough to show.
    #[serde(default)]
    pub html: Option<String>,
    /// Why an HTML part was left out: `too_large`.
    #[serde(default)]
    pub html_omitted: Option<HtmlOmission>,
    /// The images the HTML part shows from the message itself.
    #[serde(default)]
    pub inline: Vec<Inline>,
    #[serde(default)]
    pub attachments: Vec<EmailAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailPerson {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
}

impl EmailPerson {
    /// `Name <address>`, or whichever of the two the header has.
    pub fn label(&self) -> String {
        match (self.name.as_deref(), self.address.as_deref()) {
            (Some(name), Some(address)) if !name.is_empty() => format!("{name} <{address}>"),
            (_, Some(address)) => address.to_owned(),
            (Some(name), None) => name.to_owned(),
            (None, None) => String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailAttachment {
    #[serde(default)]
    pub part: usize,
    #[serde(default)]
    pub name: Option<String>,
    pub media_type: String,
    pub size: u64,
}

/// A bot's trigger on an inbox: whether new mail wakes it, and what to do
/// then. A trigger the inbox sign-in card turned on has no instructions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailTrigger {
    pub account_id: Uuid,
    pub address: String,
    pub state: AccountState,
    #[serde(default)]
    pub health: Option<AccountHealth>,
    pub wake: bool,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub last_woke_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailEndpoint {
    pub host: String,
    pub port: u16,
    pub start_tls: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailConfig {
    pub provider: EmailProvider,
    pub address: String,
    pub username: String,
    pub imap: EmailEndpoint,
    pub smtp: EmailEndpoint,
    pub archive_folder: String,
    pub trash_folder: String,
    pub drafts_folder: String,
    pub sent_folder: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailAccount {
    pub id: Uuid,
    pub config: EmailConfig,
    #[serde(default)]
    pub generation: i64,
    pub state: AccountState,
    pub health: AccountHealth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailPreset {
    pub provider: EmailProvider,
    pub preset: Option<EmailConfig>,
    #[serde(default)]
    pub authentication: Vec<Authentication>,
    #[serde(default)]
    pub oauth_wizard: bool,
}

/// The server's settings for `provider`, signing in as `address`.
pub fn preset_config(
    presets: &[EmailPreset],
    provider: EmailProvider,
    address: &str,
) -> Option<EmailConfig> {
    let mut config = presets
        .iter()
        .find(|p| p.provider == provider)?
        .preset
        .clone()?;
    config.address = address.to_owned();
    config.username = address.to_owned();
    Some(config)
}

/// An image part the message carries and its HTML names as `cid:`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inline {
    /// The part's Content-ID, without angle brackets.
    pub content_id: String,
    pub media_type: String,
    pub data_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Definition {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub version: u32,
    pub providers: Vec<EmailPreset>,
    #[serde(default)]
    pub credentials: String,
    #[serde(default)]
    pub notifications: String,
    #[serde(default)]
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Definitions {
    pub connectors: Vec<Definition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accounts {
    pub accounts: Vec<EmailAccount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connected {
    pub account: EmailAccount,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountDetail {
    pub account: EmailAccount,
    pub grants: Vec<Grant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub bot_id: Uuid,
    pub permissions: Permissions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    pub read: bool,
    pub write: bool,
    pub send: bool,
    pub wake: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailAuthorization {
    pub request_id: Uuid,
    pub authorization_url: String,
    #[serde(default)]
    pub expires_in: u64,
}

/// An email that started a turn, which the owner can open in the reader.
/// `message` is where it arrived; the server follows it from there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMail {
    pub account_id: Uuid,
    pub message: MessageRef,
    /// Absent from older servers, and for mail that arrived before them.
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub sender: Option<String>,
    #[serde(default)]
    pub location: MailLocation,
}

/// Where a turn's email is now, and which bot's turn put it there.
/// Older servers send none, which reads as where it arrived.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MailLocation {
    #[default]
    Arrived,
    Archived(MailMove),
    Trashed(MailMove),
    Moved(MailMove),
    Deleted(MailDeletion),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailMove {
    pub folder: String,
    pub by_bot_id: Option<Uuid>,
    pub by_turn_id: Option<Uuid>,
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailDeletion {
    pub by_bot_id: Option<Uuid>,
    pub by_turn_id: Option<Uuid>,
    pub at: String,
}

impl MailLocation {
    /// The turn that moved or deleted the email, when a bot did.
    pub fn by_turn(&self) -> Option<Uuid> {
        match self {
            Self::Arrived | Self::Unknown => None,
            Self::Archived(m) | Self::Trashed(m) | Self::Moved(m) => m.by_turn_id,
            Self::Deleted(d) => d.by_turn_id,
        }
    }
}
