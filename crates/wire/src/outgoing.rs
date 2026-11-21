use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

wire_state! {
    pub enum DraftStatus {
        Pending = "pending",
        Approved = "approved",
        Sending = "sending",
        Sent = "sent",
        Failed = "failed",
        Uncertain = "uncertain",
        Discarded = "discarded",
        Superseded = "superseded",
    }
    _ => Unknown
}

wire_state! {
    pub enum ApprovalSource {
        Button = "button",
        Message = "message",
        /// Approved as proposed, because the owner approves its kind always.
        Standing = "standing",
    }
    _ => Unknown
}

wire_state! {
    pub enum DraftPlatform {
        X = "x",
        Telegram = "telegram",
        Discord = "discord",
        WhatsApp = "whatsapp",
    }
    _ => Other(String)
}

impl DraftStatus {
    pub fn can_follow_up(self) -> bool {
        matches!(self, Self::Pending | Self::Failed | Self::Uncertain)
    }

    /// What became of the draft, without who approved it.
    pub fn label(self, content: &DraftContent) -> &'static str {
        match self {
            Self::Pending => "Needs approval",
            Self::Approved if content.is_post() => "Waiting to post",
            Self::Approved => "Waiting to send",
            Self::Sending if content.is_post() => "Posting…",
            Self::Sending if content.is_email() => "Sending email…",
            Self::Sending => "Sending message…",
            Self::Sent if content.is_post() => "Posted",
            Self::Sent if content.is_email() => "Email sent",
            Self::Sent => "Message sent",
            Self::Failed if content.is_post() => "Post not published",
            Self::Failed if content.is_email() => "Email not sent",
            Self::Failed => "Message not sent",
            Self::Uncertain if content.is_post() => "May have been posted",
            Self::Uncertain => "May have been sent",
            Self::Discarded => "Draft discarded",
            Self::Superseded => "Draft replaced",
            Self::Unknown => "Draft status unavailable",
        }
    }
}

/// A file the owner can review and approve with a draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftFile {
    pub id: Uuid,
    pub name: String,
    pub media_type: String,
    pub size: u64,
    /// Lowercase hex SHA-256 of the approved bytes.
    pub sha256: String,
}

/// What "Approve always" covers: messages on one platform from one sending
/// account to one destination, whatever they say, or, for a message the bot
/// files under a recurring task, every message of that task from that
/// account, to any destination. Emails have no kind (each goes to its own
/// recipients), nor do X replies without a task (each answers its own
/// post). WhatsApp sends as the owner's own account, so its kind is always
/// one chat, never a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftKind {
    pub platform: DraftPlatform,
    pub account_id: Option<Uuid>,
    /// The account as the platform shows it, lowercased.
    pub account: Option<String>,
    pub destination: Option<String>,
    /// The recurring task, lowercased.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
}

/// The one call that sent (or is sending) an approved draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "via", rename_all = "lowercase")]
pub enum DraftDelivery {
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
    #[serde(other)]
    Unknown,
}

/// Where the email being replied to arrived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftEmailReplyTo {
    pub folder: String,
    pub uid_validity: u32,
    pub uid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "channel", rename_all = "lowercase")]
pub enum DraftContent {
    Email {
        from: String,
        to: Vec<String>,
        cc: Vec<String>,
        bcc: Vec<String>,
        subject: String,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<Uuid>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<DraftEmailReplyTo>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<DraftFile>,
    },
    Message {
        platform: DraftPlatform,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<Uuid>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
        account: Option<String>,
        destination: Option<String>,
        /// The recurring task the bot filed it under.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
        body: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<DraftFile>,
    },
    /// A channel a newer server added. Only a reader builds it.
    #[serde(other)]
    Unknown,
}

impl DraftContent {
    pub fn attachments(&self) -> &[DraftFile] {
        match self {
            Self::Email { attachments, .. } | Self::Message { attachments, .. } => attachments,
            Self::Unknown => &[],
        }
    }

    pub fn attachment_ids(&self) -> Option<Vec<Uuid>> {
        let files = self.attachments();
        (!files.is_empty()).then(|| files.iter().map(|file| file.id).collect())
    }

    pub fn is_email(&self) -> bool {
        matches!(self, Self::Email { .. })
    }
    pub fn is_post(&self) -> bool {
        matches!(self, Self::Message { platform, .. } if *platform == DraftPlatform::X)
    }
    /// Messages go out as the owner's own account, so Approve always covers
    /// one chat.
    pub fn sends_as_owner(&self) -> bool {
        matches!(self, Self::Message { platform, .. } if *platform == DraftPlatform::WhatsApp)
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Email { .. } => "Mail",
            Self::Message { platform, .. } => match platform {
                DraftPlatform::X => "X",
                DraftPlatform::Telegram => "Telegram",
                DraftPlatform::Discord => "Discord",
                DraftPlatform::WhatsApp => "WhatsApp",
                DraftPlatform::Other(text) => text,
            },
            Self::Unknown => "Draft",
        }
    }

    pub fn body(&self) -> &str {
        match self {
            Self::Email { body, .. } | Self::Message { body, .. } => body,
            Self::Unknown => "",
        }
    }

    pub fn subject(&self) -> Option<&str> {
        match self {
            Self::Email { subject, .. } => Some(subject),
            Self::Message { .. } | Self::Unknown => None,
        }
    }

    /// What a receipt inside a task leads with: the email's subject, else
    /// what the draft is.
    pub fn title(&self) -> String {
        match self {
            Self::Email { subject, .. } if !subject.is_empty() => subject.clone(),
            Self::Email { .. } => "Email".into(),
            Self::Message { .. } if self.is_post() => "X post".into(),
            Self::Message { .. } => format!("{} message", self.label()),
            Self::Unknown => "Draft".into(),
        }
    }

    /// Who it goes to, when it names someone.
    fn recipient(&self) -> Option<String> {
        match self {
            Self::Email { to, .. } => Some(to.join(", ")),
            Self::Message { destination, .. } => destination.clone(),
            Self::Unknown => None,
        }
    }

    pub fn audience(&self) -> String {
        match self {
            Self::Email { to, .. } => to.join(", "),
            Self::Message {
                destination: Some(destination),
                ..
            } => destination.clone(),
            Self::Message {
                account: Some(account),
                ..
            } => account.clone(),
            Self::Message { .. } if self.is_post() => "Public post".into(),
            Self::Message { .. } | Self::Unknown => String::new(),
        }
    }

    pub fn metadata(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::Email {
                from, to, cc, bcc, ..
            } => {
                let mut rows = vec![("To", to.join(", ")), ("From", from.clone())];
                if !cc.is_empty() {
                    rows.push(("Cc", cc.join(", ")));
                }
                if !bcc.is_empty() {
                    rows.push(("Bcc", bcc.join(", ")));
                }
                rows
            }
            Self::Message {
                platform,
                account,
                destination,
                task,
                ..
            } => {
                let mut rows = Vec::new();
                if let Some(task) = task {
                    rows.push(("Task", task.clone()));
                }
                if let Some(account) = account {
                    rows.push(("Account", account.clone()));
                }
                if let Some(destination) = destination {
                    rows.push((
                        match platform {
                            DraftPlatform::Telegram => "Chat",
                            DraftPlatform::X => "Reply to",
                            _ => "Destination",
                        },
                        destination.clone(),
                    ));
                }
                rows
            }
            Self::Unknown => Vec::new(),
        }
    }

    /// Who and where "Approve always" covers, as in "X posts from @acme",
    /// or for a task, "Initial lead DMs: X messages from @acme to anyone".
    pub fn kind_label(&self) -> String {
        let Self::Message {
            account,
            destination,
            task,
            ..
        } = self
        else {
            return if self.is_email() { "emails" } else { "drafts" }.into();
        };
        let mut label = format!(
            "{}{} {}",
            task.as_ref().map(|t| format!("{t}: ")).unwrap_or_default(),
            self.label(),
            if self.is_post() && destination.is_none() && task.is_none() {
                "posts"
            } else {
                "messages"
            }
        );
        if let Some(account) = account {
            label.push_str(&format!(" from {account}"));
        }
        match (task, destination) {
            (Some(_), _) => label.push_str(" to anyone"),
            (None, Some(destination)) => label.push_str(&format!(" to {destination}")),
            (None, None) => {}
        }
        label
    }

    pub fn approve_label(&self) -> &'static str {
        if self.is_post() {
            "Approve & post"
        } else {
            "Approve & send"
        }
    }
    pub fn body_label(&self) -> &'static str {
        if self.is_email() {
            "Email body"
        } else if self.is_post() {
            "Post text"
        } else {
            "Message text"
        }
    }
}

/// The detail the API writes when it closes a draft because the owner's reply
/// moved on (migration `0037_reply_closes_drafts`), not by the Discard button.
const REPLY_MOVED_ON: &str = "Your reply moved on without sending it.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingDraft {
    pub id: Uuid,
    pub lineage_id: Uuid,
    pub revision: i64,
    pub bot_id: Option<Uuid>,
    pub turn_id: Option<Uuid>,
    pub content: DraftContent,
    pub version: i64,
    pub status: DraftStatus,
    pub can_approve: bool,
    /// Handed to the live request of the owner reply it was sent with.
    #[serde(default)]
    pub in_reply: bool,
    #[serde(default)]
    pub approval_source: Option<ApprovalSource>,
    pub approved_version: Option<i64>,
    pub approved_item_id: Option<Uuid>,
    pub approved_at: Option<DateTime<Utc>>,
    pub delivery: Option<DraftDelivery>,
    pub result_detail: Option<String>,
    pub superseded_by: Option<Uuid>,
    /// What "Approve always" would cover; none for an email or an X reply.
    #[serde(default)]
    pub kind: Option<DraftKind>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftResponse {
    pub draft: OutgoingDraft,
    #[serde(default)]
    pub requeued: bool,
}

impl OutgoingDraft {
    /// Open on its card for the owner to edit, approve, or discard. A draft
    /// on a channel this build does not know stays closed: its card cannot
    /// show what the owner would approve.
    pub fn is_open(&self) -> bool {
        self.status == DraftStatus::Pending
            && !self.in_reply
            && self.content != DraftContent::Unknown
    }

    /// The receipt line: who acted, then what became of the draft.
    pub fn label(&self) -> String {
        match self.status {
            DraftStatus::Pending if self.in_reply => "Continued in your reply".into(),
            DraftStatus::Pending if self.content == DraftContent::Unknown => {
                "Update else.bot to review this draft".into()
            }
            DraftStatus::Discarded if self.result_detail.as_deref() == Some(REPLY_MOVED_ON) => {
                "Closed by your reply".into()
            }
            // Otherwise only the owner's Discard button discards a draft.
            DraftStatus::Discarded => "You discarded this draft".into(),
            _ => {
                let outcome = self.status.label(&self.content);
                match self.approver() {
                    Some(who) => format!("{who} · {outcome}"),
                    None => outcome.into(),
                }
            }
        }
    }

    /// Who approved the draft, once it has left review.
    fn approver(&self) -> Option<&'static str> {
        match (self.status, self.approval_source) {
            (DraftStatus::Pending | DraftStatus::Superseded, _) => None,
            (_, Some(ApprovalSource::Button)) => Some("You approved"),
            (_, Some(ApprovalSource::Message)) => Some("You approved in chat"),
            (_, Some(ApprovalSource::Standing)) => Some("Approved by your standing rule"),
            (DraftStatus::Approved, None | Some(ApprovalSource::Unknown)) => Some("Approved"),
            (_, None | Some(ApprovalSource::Unknown)) => None,
        }
    }

    /// The receipt's title and outcome lines inside a task, whose meta row
    /// already carries the time.
    pub fn task_receipt(&self) -> (String, String) {
        let outcome = match self.content.recipient() {
            Some(to) => format!("{} · to {to}", self.label()),
            None => self.label(),
        };
        (self.content.title(), outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::REPLY_MOVED_ON;

    #[test]
    fn the_reply_closed_detail_matches_the_migration_that_writes_it() {
        assert!(
            include_str!("../../../apps/api/migrations/0037_reply_closes_drafts.sql")
                .contains(REPLY_MOVED_ON),
            "rewording the trigger's detail would credit the owner with its discards"
        );
    }
}
