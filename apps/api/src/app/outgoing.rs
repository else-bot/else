use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::connectors::types::{Draft, MailCommand, MessageRef, OutgoingAttachment};

pub const MAX_ADDRESS: usize = 320;
pub const MAX_RECIPIENTS: usize = 50;
pub const MAX_SUBJECT: usize = 998;
pub const MAX_BODY: usize = 100_000;
pub const MAX_DETAIL: usize = 2000;
/// An account or destination name on a messaging or social platform.
pub const MAX_HANDLE: usize = 200;
/// Files one draft may name, on any channel.
pub const MAX_FILES: usize = 10;
const MIB: u64 = 1024 * 1024;

/// A file attached to a draft. Drafting copies it into the bot's file store
/// (`attachments`), so the owner approves these exact bytes and a send
/// refuses bytes whose hash differs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftFile {
    pub id: Uuid,
    pub name: String,
    pub media_type: String,
    pub size: u64,
    /// Lowercase hex SHA-256 of the approved bytes.
    pub sha256: String,
}

impl DraftFile {
    /// A picture platforms show inline: a photo on Telegram, an image with
    /// a caption on WhatsApp.
    pub fn is_image(&self) -> bool {
        matches!(
            self.media_type.as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        )
    }
}

/// Where `attachments` takes a file from, as the bot names it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum FileSource {
    /// A file in the bot's project: project-relative or under `~/work/`.
    Path(String),
    /// One attachment of an email the bot can read in a connected account.
    Email(EmailPart),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailPart {
    pub account_id: Uuid,
    pub message: MessageRef,
    pub part: usize,
}

/// The `attachments` argument of a draft tool; absent means none.
pub fn file_sources(args: &Value) -> Result<Vec<FileSource>, &'static str> {
    const USAGE: &str = "attachments is a list of up to 10 items, each {path} for a file in your \
                         project or {email: {account_id, message, part}} for an email attachment";
    match args.get("attachments") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(list)) if list.len() <= MAX_FILES => list
            .iter()
            .map(|item| serde_json::from_value(item.clone()).map_err(|_| USAGE))
            .collect(),
        Some(_) => Err(USAGE),
    }
}

/// What a channel accepts as attachments, checked when drafting so a send
/// never meets a file its platform refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRules {
    pub medium: &'static str,
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub images_only: bool,
}

impl FileRules {
    pub fn check(&self, files: &[DraftFile]) -> Result<(), String> {
        let FileRules {
            medium,
            max_files,
            max_file_bytes,
            max_total_bytes,
            images_only,
        } = *self;
        if files.len() > max_files {
            return Err(format!("{medium} takes at most {max_files} attachments"));
        }
        if let Some((_, file)) = files
            .iter()
            .enumerate()
            .find(|(index, file)| files[..*index].iter().any(|f| f.id == file.id))
        {
            return Err(format!(
                "{} is attached twice; attach each file once",
                file.name
            ));
        }
        if let Some(file) = files.iter().find(|f| f.size > max_file_bytes) {
            return Err(format!(
                "{} is {}; {medium} takes files of at most {}",
                file.name,
                size_text(file.size),
                size_text(max_file_bytes)
            ));
        }
        let total: u64 = files.iter().map(|f| f.size).sum();
        if total > max_total_bytes {
            return Err(format!(
                "the attachments total {}; {medium} takes at most {} in all",
                size_text(total),
                size_text(max_total_bytes)
            ));
        }
        if images_only && let Some(file) = files.iter().find(|f| !f.is_image()) {
            return Err(format!(
                "{} is {}; {medium} takes only PNG, JPEG, GIF, or WebP images",
                file.name, file.media_type
            ));
        }
        Ok(())
    }
}

/// A size as the owner reads it: "820 KB", "4.2 MB".
pub fn size_text(bytes: u64) -> String {
    if bytes < MIB {
        format!("{} KB", bytes.div_ceil(1024))
    } else {
        format!("{:.1} MB", bytes as f64 / MIB as f64)
    }
}

/// Where a draft is in its life. `pending` is the only state the owner can
/// edit. `approved` allows exactly one send by its send request; the claim
/// moves it to `sending`, and only the bot's report for that claim settles
/// it as `sent`, `failed`, or `uncertain`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, strum::IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Status {
    Pending,
    Approved,
    Sending,
    Sent,
    Failed,
    Uncertain,
    Discarded,
    Superseded,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// The owner may still decide to send it: as drafted, or again after a
    /// failed or uncertain attempt (a chat approval only).
    pub fn approvable_in_chat(self) -> bool {
        matches!(self, Self::Pending | Self::Failed | Self::Uncertain)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum ApprovalSource {
    /// The Approve button on the card.
    Button,
    /// An owner message sent while the draft was open, which the bot read
    /// as approval of that exact version.
    Message,
    /// The owner approved its kind always ([`DraftKind`]); it was approved
    /// as proposed.
    Standing,
}

/// What "Approve always" covers: messages on one platform from one sending
/// account to one destination, whatever they say, or, for a message the bot
/// files under a recurring task, every message of that task from that
/// account, to any destination. Emails have no kind (each goes to its own
/// recipients), nor do X replies without a task (each answers its own
/// post). WhatsApp sends as the owner's own account, so its kind is always
/// one chat, never a task. `kind!` in `db::outgoing_drafts` derives it from
/// stored content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftKind {
    pub platform: Platform,
    pub account_id: Option<Uuid>,
    /// The account as the platform shows it, lowercased.
    pub account: Option<String>,
    pub destination: Option<String>,
    /// The recurring task, lowercased.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
}

/// How the bot reports a claimed send ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Sent,
    Failed,
    Uncertain,
}

impl Outcome {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "sent" => Some(Self::Sent),
            "failed" => Some(Self::Failed),
            "uncertain" => Some(Self::Uncertain),
            _ => None,
        }
    }

    pub fn status(self) -> Status {
        match self {
            Self::Sent => Status::Sent,
            Self::Failed => Status::Failed,
            Self::Uncertain => Status::Uncertain,
        }
    }
}

/// The one call that sent (or is sending) an approved draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "via", rename_all = "lowercase")]
pub enum Delivery {
    /// A computer tool call declared external, with its loggable arguments.
    Tool {
        tool: String,
        args: Value,
    },
    /// The bot's host tools, which the API cannot see.
    Host,
    /// The email's connected account, as one durable mail action: its
    /// receipt settles the draft (see the migration's triggers).
    Mail {
        account_id: Uuid,
        action_id: Uuid,
    },
    Social {
        account_id: Uuid,
        action_id: Uuid,
    },
}

/// A platform a bot can draft a message or post for. The bot sends through
/// its own tools; the API only names the medium on the card and receipt.
/// Adding a platform is a new variant, not a new lifecycle.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Platform {
    /// A post on X (Twitter).
    X,
    Telegram,
    Discord,
    WhatsApp,
}

impl Platform {
    pub const ALL: [Self; 4] = [Self::X, Self::Telegram, Self::Discord, Self::WhatsApp];

    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// What one draft on this platform is, for receipts and logs.
    pub fn noun(self) -> &'static str {
        match self {
            Self::X => "X post",
            Self::Telegram => "Telegram message",
            Self::Discord => "Discord message",
            Self::WhatsApp => "WhatsApp message",
        }
    }

    fn needs_destination(self) -> bool {
        match self {
            Self::X => false,
            Self::Telegram | Self::Discord | Self::WhatsApp => true,
        }
    }

    /// The attachments one message accepts. X takes up to four pictures;
    /// Telegram photos and Discord uploads cap at 10 MB.
    pub fn file_rules(self) -> FileRules {
        match self {
            Self::X => FileRules {
                medium: "an X post",
                max_files: 4,
                max_file_bytes: 5 * MIB,
                max_total_bytes: 20 * MIB,
                images_only: true,
            },
            Self::Telegram => FileRules {
                medium: "a Telegram message",
                max_files: MAX_FILES,
                max_file_bytes: 10 * MIB,
                max_total_bytes: 100 * MIB,
                images_only: false,
            },
            Self::Discord => FileRules {
                medium: "a Discord message",
                max_files: MAX_FILES,
                max_file_bytes: 10 * MIB,
                max_total_bytes: 10 * MIB,
                images_only: false,
            },
            Self::WhatsApp => FileRules {
                medium: "a WhatsApp message",
                max_files: MAX_FILES,
                max_file_bytes: 10 * MIB,
                max_total_bytes: 100 * MIB,
                images_only: false,
            },
        }
    }

    /// Whether messages go out as the owner's own account, so Approve always
    /// covers one chat and never a task. The SQL `kind!` in
    /// `db::outgoing_drafts` mirrors this.
    pub fn sends_as_owner(self) -> bool {
        match self {
            Self::WhatsApp => true,
            Self::X | Self::Telegram | Self::Discord => false,
        }
    }
}

/// An email: headers, subject, and plain-text body. With `account_id` it
/// goes from that connected account (`from` is its address) through the
/// API, optionally in the thread of `reply_to`; without, the bot sends it
/// with its own tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Email {
    pub from: String,
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<DraftFile>,
}

/// What one email accepts, the connector's MIME limit included.
pub const EMAIL_FILES: FileRules = FileRules {
    medium: "an email",
    max_files: MAX_FILES,
    max_file_bytes: 10 * MIB,
    max_total_bytes: crate::connectors::types::MAX_ATTACHMENT_TOTAL as u64,
    images_only: false,
};

/// A message or post on a messaging or social platform. It has no subject
/// and no addresses: `destination` names the chat, channel, user, or post
/// replied to as the platform knows it, and `account` the sending account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub platform: Platform,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub account: Option<String>,
    pub destination: Option<String>,
    /// The recurring task the bot files this message under, so "Approve
    /// always" can cover the task's messages to any destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<DraftFile>,
}

/// What the bot drafts, by channel. The owner can change only the body and,
/// for an email, the subject. Other fields are fixed within a revision;
/// replacements keep the same channel and platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "channel", rename_all = "lowercase")]
pub enum Content {
    Email(Email),
    Message(Message),
}

/// The owner's text for a draft, as last shown to them; an omitted field
/// keeps the saved text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Edits<'a> {
    pub subject: Option<&'a str>,
    pub body: Option<&'a str>,
    /// The attachments the owner keeps, by id: removal only.
    pub attachments: Option<&'a [Uuid]>,
}

impl Content {
    pub fn body(&self) -> &str {
        match self {
            Self::Email(email) => &email.body,
            Self::Message(message) => &message.body,
        }
    }

    pub fn files(&self) -> &[DraftFile] {
        match self {
            Self::Email(email) => &email.attachments,
            Self::Message(message) => &message.attachments,
        }
    }

    /// What this draft's channel accepts as attachments.
    pub fn file_rules(&self) -> FileRules {
        match self {
            Self::Email(_) => EMAIL_FILES,
            Self::Message(message) => message.platform.file_rules(),
        }
    }

    /// This content carrying `files`, if its channel accepts them.
    pub fn with_files(mut self, files: Vec<DraftFile>) -> Result<Self, String> {
        self.file_rules().check(&files)?;
        match &mut self {
            Self::Email(email) => email.attachments = files,
            Self::Message(message) => message.attachments = files,
        }
        Ok(self)
    }

    /// What this draft is, for receipts and logs: "email", "X post", ...
    pub fn noun(&self) -> &'static str {
        match self {
            Self::Email(_) => "email",
            Self::Message(message) => message.platform.noun(),
        }
    }

    /// Who or where it goes, for logs.
    pub fn audience(&self) -> String {
        match self {
            Self::Email(email) => email.to.join(", "),
            Self::Message(message) => message
                .destination
                .clone()
                .unwrap_or_else(|| "the public timeline".to_owned()),
        }
    }

    /// The connected account an email goes from, if any.
    pub fn account(&self) -> Option<Uuid> {
        match self {
            Self::Email(email) => email.account_id,
            Self::Message(message) => message.account_id,
        }
    }

    /// What a revision must keep, for its refusal.
    pub fn channel(&self) -> &'static str {
        match self {
            Self::Email(Email {
                account_id: Some(_),
                ..
            }) => "an email from its connected account: pass the same account_id and reply_to",
            other => other.noun(),
        }
    }

    /// The same channel: both emails from the same connected account (or
    /// none) in the same thread, or messages on the same platform. A
    /// revision may change its recipients (it is approved afresh) but never
    /// its medium or sending account.
    pub fn same_channel(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Email(a), Self::Email(b)) => {
                a.account_id == b.account_id && a.reply_to == b.reply_to
            }
            (Self::Message(a), Self::Message(b)) => {
                a.platform == b.platform && a.account_id == b.account_id && a.reply_to == b.reply_to
            }
            _ => false,
        }
    }

    /// This content with the owner's edits applied.
    pub fn edited(&self, edits: Edits<'_>) -> Result<Self, &'static str> {
        let body = edits.body.map(body).transpose()?;
        let attachments = match edits.attachments {
            None => self.files().to_vec(),
            Some(kept)
                if kept
                    .iter()
                    .all(|id| self.files().iter().any(|f| f.id == *id)) =>
            {
                self.files()
                    .iter()
                    .filter(|file| kept.contains(&file.id))
                    .cloned()
                    .collect()
            }
            Some(_) => return Err("the owner can only remove attachments the draft has"),
        };
        match self {
            Self::Email(email) => Email {
                subject: match edits.subject {
                    Some(text) => subject(text)?,
                    None => email.subject.clone(),
                },
                body: body.unwrap_or_else(|| email.body.clone()),
                attachments,
                ..email.clone()
            }
            .connectable()
            .map(Self::Email),
            Self::Message(_) if edits.subject.is_some() => {
                Err("this draft is a message with no subject; edit only its body")
            }
            Self::Message(message) => Ok(Self::Message(Message {
                body: body.unwrap_or_else(|| message.body.clone()),
                attachments,
                ..message.clone()
            })),
        }
    }
}

fn address(value: &Value) -> Result<String, &'static str> {
    let text = value
        .as_str()
        .map(str::trim)
        .ok_or("each address is text")?;
    if text.len() < 3
        || text.len() > MAX_ADDRESS
        || !text.contains('@')
        || text.chars().any(char::is_control)
    {
        return Err("each address is 3 to 320 bytes with an @ and no line breaks");
    }
    Ok(text.to_owned())
}

fn addresses(args: &Value, key: &str, required: bool) -> Result<Vec<String>, &'static str> {
    let list = match args.get(key) {
        None | Some(Value::Null) if !required => return Ok(Vec::new()),
        Some(Value::Array(list)) => list,
        _ => return Err("to is a list of 1 to 50 addresses; cc and bcc are optional lists"),
    };
    if (required && list.is_empty()) || list.len() > MAX_RECIPIENTS {
        return Err("to is a list of 1 to 50 addresses; cc and bcc are optional lists");
    }
    list.iter().map(address).collect()
}

fn handle(args: &Value, key: &str) -> Result<Option<String>, &'static str> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text))
            if !text.trim().is_empty()
                && text.trim().len() <= MAX_HANDLE
                && !text.chars().any(char::is_control) =>
        {
            Ok(Some(text.trim().to_owned()))
        }
        Some(_) => Err("account, destination, and task are one line of 1 to 200 bytes"),
    }
}

/// A subject: one line, possibly empty.
pub fn subject(text: &str) -> Result<String, &'static str> {
    let text = text.trim();
    if text.len() > MAX_SUBJECT || text.chars().any(char::is_control) {
        return Err("subject is one line of at most 998 bytes");
    }
    Ok(text.to_owned())
}

/// A body: some text, at most 100 KB, no NUL.
pub fn body(text: &str) -> Result<String, &'static str> {
    if text.trim().is_empty() || text.len() > MAX_BODY || text.contains('\0') {
        return Err("body is 1 to 100000 bytes of text");
    }
    Ok(text.to_owned())
}

/// A short receipt or error the bot reports for a send.
pub fn detail(value: Option<&Value>) -> Result<Option<String>, &'static str> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.len() <= MAX_DETAIL && !text.contains('\0') => {
            Ok(Some(text.trim().to_owned()).filter(|t| !t.is_empty()))
        }
        Some(_) => Err("detail is at most 2000 bytes of text"),
    }
}

fn only(args: &Value, allowed: &[&str], usage: &'static str) -> Result<(), &'static str> {
    match args.as_object() {
        Some(object) if object.keys().all(|k| allowed.contains(&k.as_str())) => Ok(()),
        _ => Err(usage),
    }
}

impl Email {
    /// Parse `draft_email` arguments (`replaces` is read by the caller).
    /// With `account_id`, `from` is the connected account's `address`,
    /// which the caller looked up for the bot.
    pub fn parse(args: &Value, account: Option<&str>) -> Result<Self, &'static str> {
        only(
            args,
            &[
                "from",
                "to",
                "cc",
                "bcc",
                "subject",
                "body",
                "replaces",
                "account_id",
                "reply_to",
                "attachments",
            ],
            "use from (or account_id), to, subject, body, and optionally cc, bcc, reply_to, \
             attachments, replaces",
        )?;
        let account_id = match args.get("account_id") {
            None | Some(Value::Null) => None,
            Some(id) => Some(
                id.as_str()
                    .and_then(|id| id.parse().ok())
                    .ok_or("account_id is a connected account id from mail_accounts")?,
            ),
        };
        let reply_to = match args.get("reply_to") {
            None | Some(Value::Null) => None,
            Some(_) if account_id.is_none() => {
                return Err("reply_to answers a message in a connected account; pass account_id");
            }
            Some(message) => Some(
                serde_json::from_value(message.clone())
                    .map_err(|_| "reply_to is a message {folder, uid_validity, uid} from mail")?,
            ),
        };
        let from = match (account_id, account, args.get("from")) {
            (Some(_), Some(own), None | Some(Value::Null)) => own.to_owned(),
            (Some(_), Some(own), Some(given))
                if given
                    .as_str()
                    .is_some_and(|g| g.trim().eq_ignore_ascii_case(own)) =>
            {
                own.to_owned()
            }
            (Some(_), _, _) => {
                return Err("a connected account sends from its own address; omit from");
            }
            (None, _, from) => address(from.unwrap_or(&Value::Null))
                .map_err(|_| "from is the sending address, 3 to 320 bytes with an @")?,
        };
        Self {
            from,
            to: addresses(args, "to", true)?,
            cc: addresses(args, "cc", false)?,
            bcc: addresses(args, "bcc", false)?,
            subject: subject(args["subject"].as_str().ok_or("subject is text")?)?,
            body: body(args["body"].as_str().ok_or("body is text")?)?,
            account_id,
            reply_to,
            attachments: Vec::new(),
        }
        .connectable()
    }

    /// The mail command that sends exactly this email from its account,
    /// with the bytes of its attachments in order and its Message-ID.
    pub fn send_command(
        &self,
        files: Vec<OutgoingAttachment>,
        message_id: Option<String>,
    ) -> MailCommand {
        MailCommand::Send {
            draft: Draft {
                to: self.to.clone(),
                cc: self.cc.clone(),
                bcc: self.bcc.clone(),
                subject: self.subject.clone(),
                body: self.body.clone(),
                attachments: files,
                reply_to: self.reply_to.clone(),
                message_id,
            },
        }
    }

    /// The Message-ID every send of draft `draft_id` carries, at the
    /// sender's domain, so a retry can find an earlier copy in Sent.
    pub fn message_id(&self, draft_id: Uuid) -> String {
        let domain = self
            .from
            .rsplit_once('@')
            .map_or("else.bot", |(_, domain)| domain);
        format!("draft-{}@{domain}", draft_id.simple())
    }

    fn connectable(self) -> Result<Self, &'static str> {
        if self.account_id.is_some() && self.send_command(Vec::new(), None).validate().is_err() {
            return Err(
                "an email from a connected account has 1 to 50 bare addresses \
                        (name@example.com) and a body of at most 32000 bytes",
            );
        }
        Ok(self)
    }
}

impl Message {
    /// Parse `draft_message` arguments (`replaces` is read by the caller).
    pub fn parse(args: &Value) -> Result<Self, &'static str> {
        only(
            args,
            &[
                "platform",
                "account",
                "destination",
                "body",
                "replaces",
                "account_id",
                "reply_to",
                "task",
                "attachments",
            ],
            "use platform, body, and optionally account, destination, task, attachments, replaces \
             (a message has no subject or addresses; use draft_email for email)",
        )?;
        let platform = args["platform"]
            .as_str()
            .and_then(|p| p.parse::<Platform>().ok())
            .ok_or("platform is x, telegram, discord or whatsapp")?;
        let destination = handle(args, "destination")?;
        if platform.needs_destination() && destination.is_none() {
            return Err("a social message needs its destination chat or channel");
        }
        let account_id = args
            .get("account_id")
            .filter(|v| !v.is_null())
            .map(|v| {
                v.as_str()
                    .and_then(|s| s.parse().ok())
                    .ok_or("account_id must be a native social connection UUID")
            })
            .transpose()?;
        if account_id.is_some() && platform == Platform::X {
            return Err("X has no native social connector");
        }
        let account = handle(args, "account")?;
        let reply_to = handle(args, "reply_to")?;
        // A public post is approved always by its account alone, so a task
        // would add nothing there.
        let task = handle(args, "task")?
            .filter(|_| destination.is_some() || reply_to.is_some())
            .map(|task| task.split_whitespace().collect::<Vec<_>>().join(" "));
        if task.is_some() && platform.sends_as_owner() {
            return Err(
                "a WhatsApp message goes out as the owner, so Approve always covers one chat, never a task; omit task",
            );
        }
        if task.is_some() && account.is_none() && account_id.is_none() {
            return Err("a task needs the account it sends from: account or account_id");
        }
        Ok(Self {
            platform,
            account_id,
            reply_to,
            account,
            destination,
            task,
            body: body(args["body"].as_str().ok_or("body is text")?)?,
            attachments: Vec::new(),
        })
    }
}

/// A draft as a turn's attempt is told about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: Uuid,
    pub lineage_id: Uuid,
    pub revision: i64,
    pub content: Content,
    pub status: Status,
    pub version: i64,
    /// This request's owner message was sent with the draft open at its
    /// current version: the owner's words may approve exactly it.
    pub in_context: bool,
    /// The owner approved or discarded it on its card, and this is the
    /// first attempt to hear it.
    pub decided: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file(name: &str, media_type: &str, size: u64) -> DraftFile {
        DraftFile {
            id: Uuid::new_v5(&Uuid::nil(), name.as_bytes()),
            name: name.into(),
            media_type: media_type.into(),
            size,
            sha256: "0".repeat(64),
        }
    }

    #[test]
    fn each_platform_refuses_files_it_cannot_send_when_drafting() {
        let photo = file("flyer.jpg", "image/jpeg", 800 * 1024);
        let pdf = file("menu.pdf", "application/pdf", 2 * MIB);
        assert!(
            Platform::WhatsApp
                .file_rules()
                .check(&[photo.clone(), pdf.clone()])
                .is_ok()
        );
        let refused = Platform::X
            .file_rules()
            .check(std::slice::from_ref(&pdf))
            .unwrap_err();
        assert!(
            refused.contains("only PNG, JPEG, GIF, or WebP"),
            "{refused}"
        );
        let five = vec![photo.clone(); 5];
        assert!(
            Platform::X.file_rules().check(&five).is_err(),
            "X takes four"
        );
        let big = file("video.mp4", "video/mp4", 11 * MIB);
        let refused = Platform::Telegram.file_rules().check(&[big]).unwrap_err();
        assert_eq!(
            refused,
            "video.mp4 is 11.0 MB; a Telegram message takes files of at most 10.0 MB"
        );
        let two = [
            file("a.pdf", "application/pdf", 6 * MIB),
            file("b.pdf", "application/pdf", 6 * MIB),
        ];
        assert!(
            Platform::Discord.file_rules().check(&two).is_err(),
            "10 MB per message"
        );
        assert!(EMAIL_FILES.check(&two).is_ok());
        let twice = EMAIL_FILES.check(&[photo.clone(), photo]).unwrap_err();
        assert_eq!(twice, "flyer.jpg is attached twice; attach each file once");
    }

    #[test]
    fn attachments_name_a_project_path_or_an_email_part() {
        let account = Uuid::new_v4();
        let sources = file_sources(&json!({ "attachments": [
            { "path": "flyer.jpg" },
            { "email": { "account_id": account, "part": 1,
                         "message": { "folder": "INBOX", "uid_validity": 7, "uid": 42 } } } ] }))
        .unwrap();
        assert_eq!(sources[0], FileSource::Path("flyer.jpg".into()));
        assert!(matches!(&sources[1], FileSource::Email(part) if part.part == 1));
        assert!(file_sources(&json!({})).unwrap().is_empty());
        for bad in [
            json!({ "attachments": [{ "path": "a", "email": {} }] }),
            json!({ "attachments": [{ "data_base64": "AAAA" }] }),
            json!({ "attachments": vec![json!({ "path": "a" }); 11] }),
        ] {
            assert!(file_sources(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_owner_removes_attachments_but_never_adds_one() {
        let kept = file("a.png", "image/png", 10);
        let dropped = file("b.png", "image/png", 10);
        let message = Content::Message(Message {
            platform: Platform::WhatsApp,
            account_id: None,
            reply_to: None,
            account: None,
            destination: Some("chat".into()),
            task: None,
            body: "Flyer".into(),
            attachments: Vec::new(),
        })
        .with_files(vec![kept.clone(), dropped])
        .unwrap();
        let edited = message
            .edited(Edits {
                subject: None,
                body: None,
                attachments: Some(&[kept.id]),
            })
            .unwrap();
        assert_eq!(edited.files(), [kept]);
        assert!(
            message
                .edited(Edits {
                    subject: None,
                    body: None,
                    attachments: Some(&[Uuid::new_v4()])
                })
                .is_err()
        );
        let plain = serde_json::to_value(
            edited
                .edited(Edits {
                    subject: None,
                    body: None,
                    attachments: Some(&[]),
                })
                .unwrap(),
        )
        .unwrap();
        assert!(
            plain.get("attachments").is_none(),
            "no files keeps today's wire shape"
        );
    }

    #[test]
    fn a_whatsapp_message_is_never_filed_under_an_always_approved_task() {
        let refused = Message::parse(
            &json!({ "platform": "whatsapp", "destination": "33612345678@s.whatsapp.net",
            "account": "+33612345678", "task": "Weekly check-ins", "body": "Hi" }),
        );
        assert!(refused.is_err());
        let one = Message::parse(&json!({ "platform": "whatsapp",
            "destination": "33612345678@s.whatsapp.net", "body": "Hi" }))
        .unwrap();
        assert_eq!(one.platform, Platform::WhatsApp);
    }

    #[test]
    fn a_task_files_messages_to_a_chat_or_user_never_public_posts() {
        let dm = Message::parse(&json!({ "platform": "x", "account": "@acme",
            "destination": "@ada", "task": " Initial   lead DMs ", "body": "Hi" }))
        .unwrap();
        assert_eq!(dm.task.as_deref(), Some("Initial lead DMs"));
        let post = Message::parse(&json!({ "platform": "x", "account": "@acme",
            "task": "Initial lead DMs", "body": "Hi all" }))
        .unwrap();
        assert_eq!(post.task, None, "a public post keeps its account-wide kind");
        assert!(
            Message::parse(&json!({ "platform": "x", "destination": "@ada",
                "task": "Initial lead DMs", "body": "Hi" }))
            .is_err(),
            "a task needs its sending account"
        );
    }

    #[test]
    fn an_email_parses_only_its_fields_and_refuses_header_injection() {
        let email = Email::parse(
            &json!({
                "from": "bot@example.com", "to": [" ada@example.com "], "cc": ["Bob <bob@example.com>"],
                "subject": " Lunch on Friday ", "body": "Hi Ada,\n\nLunch on Friday?\n",
            }),
            None,
        )
        .unwrap();
        assert_eq!(email.to, ["ada@example.com"]);
        assert_eq!(email.cc, ["Bob <bob@example.com>"]);
        assert!(email.bcc.is_empty());
        assert_eq!(email.subject, "Lunch on Friday");
        assert_eq!(email.body, "Hi Ada,\n\nLunch on Friday?\n");
        let ok = json!({ "from": "b@x.io", "to": ["a@x.io"], "subject": "", "body": "x" });
        assert!(
            Email::parse(&ok, None).is_ok(),
            "an empty subject is allowed"
        );
        for (key, bad) in [
            ("to", json!([])),
            ("to", json!(["nobody"])),
            ("to", json!(["a@x.io\r\nBcc: evil@x.io"])),
            ("subject", json!("Hi\r\nBcc: evil@x.io")),
            ("body", json!("  ")),
            ("from", json!("not an address")),
            ("platform", json!("x")),
        ] {
            let mut args = ok.clone();
            args[key] = bad;
            assert!(Email::parse(&args, None).is_err(), "{args}");
        }
    }

    #[test]
    fn a_connected_email_sends_from_its_account_and_keeps_its_binding() {
        let account = Uuid::new_v4();
        let thread = json!({ "folder": "INBOX", "uid_validity": 7, "uid": 42 });
        let args = json!({ "account_id": account, "reply_to": thread, "to": ["ada@example.com"],
                           "subject": "Re: Lunch", "body": "Friday works." });
        let email = Email::parse(&args, Some("owner@example.com")).unwrap();
        assert_eq!(email.from, "owner@example.com");
        assert_eq!(email.account_id, Some(account));
        assert_eq!(email.reply_to.as_ref().map(|m| m.uid), Some(42));
        let MailCommand::Send { draft } = email.send_command(Vec::new(), None) else {
            panic!("a send");
        };
        assert_eq!(
            (draft.subject.as_str(), draft.body.as_str()),
            ("Re: Lunch", "Friday works.")
        );
        assert_eq!(draft.reply_to, email.reply_to);
        for (key, bad) in [
            ("from", json!("someone-else@example.com")),
            ("to", json!(["Ada <ada@example.com>"])),
            ("body", json!("x".repeat(32_001))),
            ("reply_to", json!({ "folder": "INBOX" })),
        ] {
            let mut wrong = args.clone();
            wrong[key] = bad;
            assert!(
                Email::parse(&wrong, Some("owner@example.com")).is_err(),
                "{key}"
            );
        }
        let mut unbound = args.clone();
        unbound["account_id"] = Value::Null;
        unbound["from"] = json!("owner@example.com");
        assert!(
            Email::parse(&unbound, None).is_err(),
            "reply_to needs an account"
        );

        let bound = Content::Email(email.clone());
        let long = "x".repeat(32_001);
        assert!(
            bound
                .edited(Edits {
                    subject: None,
                    body: Some(&long),
                    attachments: None,
                })
                .is_err()
        );
        let other = Content::Email(Email {
            account_id: Some(Uuid::new_v4()),
            ..email.clone()
        });
        let unthreaded = Content::Email(Email {
            reply_to: None,
            ..email.clone()
        });
        let plain = Content::Email(Email {
            account_id: None,
            reply_to: None,
            ..email
        });
        for changed in [&other, &unthreaded, &plain] {
            assert!(
                !bound.same_channel(changed),
                "a revision keeps account and thread"
            );
        }
        let wire = serde_json::to_value(&plain).unwrap();
        assert!(
            wire.get("account_id").is_none(),
            "an unbound email keeps its wire shape"
        );
    }

    #[test]
    fn a_message_needs_no_address_or_subject_and_refuses_email_fields() {
        let post = Message::parse(&json!({ "platform": "x", "body": "Shipping today." })).unwrap();
        assert_eq!(post.platform, Platform::X);
        assert_eq!((post.account, post.destination), (None, None));
        let chat = Message::parse(&json!({
            "platform": "telegram", "account": " @acme_bot ", "destination": "Family group",
            "body": "Dinner at 8?",
        }))
        .unwrap();
        assert_eq!(chat.account.as_deref(), Some("@acme_bot"));
        assert_eq!(chat.destination.as_deref(), Some("Family group"));
        let ok = json!({ "platform": "telegram", "destination": "-100123", "body": "x" });
        for (key, bad) in [
            ("destination", Value::Null),
            ("destination", json!("a\nb")),
            ("platform", json!("email")),
            ("platform", json!("twitter")),
            ("subject", json!("Hi")),
            ("to", json!(["a@x.io"])),
            ("body", json!("")),
        ] {
            let mut args = ok.clone();
            args[key] = bad;
            assert!(Message::parse(&args).is_err(), "{args}");
        }
    }

    #[test]
    fn edits_change_only_the_body_and_an_email_subject() {
        let message = Content::Message(Message {
            account_id: None,
            reply_to: None,
            platform: Platform::Telegram,
            account: None,
            destination: Some("Family".into()),
            task: None,
            body: "Dinner?".into(),
            attachments: Vec::new(),
        });
        let edited = message
            .edited(Edits {
                subject: None,
                body: Some("Dinner at 8?"),
                attachments: None,
            })
            .unwrap();
        assert_eq!(edited.body(), "Dinner at 8?");
        assert!(edited.same_channel(&message));
        assert!(
            message
                .edited(Edits {
                    subject: Some(""),
                    body: None,
                    attachments: None,
                })
                .is_err(),
            "a message has no subject to edit"
        );
        let wire = serde_json::to_value(&message).unwrap();
        assert_eq!(
            wire,
            json!({ "channel": "message", "platform": "telegram", "account": null,
                    "destination": "Family", "body": "Dinner?" })
        );
        assert_eq!(serde_json::from_value::<Content>(wire).unwrap(), message);
        let post = Content::Message(Message {
            account_id: None,
            reply_to: None,
            platform: Platform::X,
            account: None,
            destination: None,
            task: None,
            body: "x".into(),
            attachments: Vec::new(),
        });
        assert!(!post.same_channel(&message), "platforms differ");
    }
}
