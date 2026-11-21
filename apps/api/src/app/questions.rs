use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use else_wire::questions as wire;
pub use else_wire::questions::QuestionAnswer as Answer;

mod permission;
mod spec;

pub use permission::{Covers, DESTRUCTIVE_SCOPE, MAX_TASK_SCOPE, PermissionTarget, TaskScope};
pub use spec::{Approval, Ask, Spec, permission_authorization};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
}

impl From<&Choice> for wire::QuestionChoice {
    fn from(value: &Choice) -> Self {
        Self {
            id: value.id.clone(),
            label: value.label.clone(),
        }
    }
}

/// The owner's answer to a question or approval request: which question
/// (its kind, title, and bound action) and the choices the owner made, or
/// that it ended unanswered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub id: Uuid,
    /// `choice`, `destructive`, or `external`.
    pub kind: Kind,
    pub title: String,
    /// For an approval: the tool call or described host action.
    pub action: Option<Action>,
    /// `answered`, `skipped`, or `cancelled`.
    pub status: Status,
    pub answers: Vec<Choice>,
}

/// Questions per owner request (turn).
pub const MAX_PER_TURN: i64 = 8;
pub const MAX_TITLE: usize = 500;
pub const MAX_CONTEXT: usize = 4000;
pub const MAX_CHOICES: usize = 5;
pub const MIN_CHOICES: usize = 2;
pub const MAX_CHOICE_LABEL: usize = 120;

pub const APPROVE: &str = "approve";
pub const DENY: &str = "deny";
pub const APPROVE_FOREVER: &str = "approve_forever";
pub const APPROVE_ROUTINE: &str = "approve_routine";
pub const APPROVE_TOOL: &str = "approve_tool";
pub const APPROVE_TASK: &str = "approve_task";

/// Where a standing approval applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Standing {
    /// Every request of the bot except routine runs and their tasks.
    Bot,
    /// One effect in runs of one routine admitted with its current prompt.
    Routine,
    /// One MCP tool, or one computer tool's external calls, in the bot's
    /// owner requests, routine runs and delegated tasks.
    Tool,
    /// One declared task on one target, with one effect, in the bot's owner
    /// requests and scheduled runs of its routines.
    Task,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, strum::IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Kind {
    Choice,
    Destructive,
    External,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// The [`Kind`] of an action to approve: never `choice`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    Destructive,
    External,
}

impl ApprovalKind {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "destructive" => Some(Self::Destructive),
            "external" => Some(Self::External),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        Kind::from(self).as_str()
    }
}

impl From<ApprovalKind> for Kind {
    fn from(kind: ApprovalKind) -> Self {
        match kind {
            ApprovalKind::Destructive => Self::Destructive,
            ApprovalKind::External => Self::External,
        }
    }
}

impl TryFrom<Kind> for ApprovalKind {
    type Error = ();

    fn try_from(kind: Kind) -> Result<Self, ()> {
        match kind {
            Kind::Choice => Err(()),
            Kind::Destructive => Ok(Self::Destructive),
            Kind::External => Ok(Self::External),
        }
    }
}

/// The effect a bot declares for a tool call; all but `local` need the
/// owner's approval of that [`Kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Local,
    Destructive,
    External,
}

impl Effect {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "local" => Some(Self::Local),
            "destructive" => Some(Self::Destructive),
            "external" => Some(Self::External),
            _ => None,
        }
    }

    pub fn approval(self) -> Option<ApprovalKind> {
        match self {
            Self::Local => None,
            Self::Destructive => Some(ApprovalKind::Destructive),
            Self::External => Some(ApprovalKind::External),
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, strum::IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Status {
    Pending,
    Answered,
    Skipped,
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Action {
    McpConnection {
        mcp_connection: McpConnection,
    },
    EmailConnection {
        email_connection: EmailConnection,
    },
    Tool {
        tool: String,
        args: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_scope: Option<TaskScope>,
    },
    Host {
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_scope: Option<TaskScope>,
    },
    Permission {
        permission: PermissionTarget,
    },
    BotDeletion {
        bot_deletion: BotDeletion,
    },
    EmailRemoval {
        email_removal: EmailRemoval,
    },
    MailAttachments {
        mail_attachments: MailAttachments,
    },
}

impl Action {
    /// Whether a saved permission may ever cover this action. Deleting a bot,
    /// removing an inbox, or opening email attachments always needs its own
    /// card.
    pub fn standing_allowed(&self) -> bool {
        !matches!(
            self,
            Self::BotDeletion { .. } | Self::EmailRemoval { .. } | Self::MailAttachments { .. }
        )
    }

    /// Whether the bot-wide destructive grant and a routine's grant cover
    /// this action. A computer resize takes host memory, so only a grant
    /// for its size covers it.
    pub fn broad_grants_cover(&self) -> bool {
        !matches!(self, Self::Tool { tool, .. } if tool == "computer_resize")
    }

    /// Whether a routine's grant for `kind` covers this action. A destructive
    /// grant covers what the bot-wide one does. An external grant covers only
    /// host actions and computer tool calls: an MCP tool has its own grant,
    /// and other external calls (replacing an MCP, a mail trigger, a webhook
    /// pace, a social post) always ask.
    pub fn routine_grant_covers(&self, kind: Kind) -> bool {
        match kind {
            Kind::Destructive => self.broad_grants_cover(),
            Kind::External => match self {
                Self::Host { .. } => true,
                Self::Tool { tool, .. } => computer_tool(tool),
                _ => false,
            },
            Kind::Choice => false,
        }
    }
}

/// A tool whose calls one standing grant per tool and effect may cover.
fn computer_tool(tool: &str) -> bool {
    crate::app::tools::ToolDef::named(tool).is_some_and(crate::app::tools::ToolDef::standing_grants)
}

/// Whether an owner message says only to always allow the card it answers,
/// such as "Always approve." or "yes, always allow it please". Any other
/// words, a limit or a condition among them, are not this answer.
pub fn says_always_allow(text: &str) -> bool {
    let text = text.to_lowercase();
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '.' | '!'))
        .filter(|word| !word.is_empty())
        .collect();
    let mut words = words.as_slice();
    if let ["yes" | "ok" | "okay" | "sure", rest @ ..] = words {
        words = rest;
    }
    if let ["please", rest @ ..] = words {
        words = rest;
    }
    if let [rest @ .., "please"] = words {
        words = rest;
    }
    if let [rest @ .., "it" | "this" | "that"] = words {
        words = rest;
    }
    matches!(
        words,
        ["always", "allow" | "approve"] | ["allow" | "approve", "always"]
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConnection {
    pub id: crate::mcp::PackageId,
    pub provider: Option<String>,
    pub scopes: Vec<String>,
}

/// An email account the owner signs in to from a chat card. Gmail reports
/// the mailbox it authorized, so `address` is only a sign-in hint there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailConnection {
    pub provider: crate::connectors::types::Provider,
    pub address: String,
    /// The generation of a connected inbox's rejected credential that this
    /// card replaces, so an earlier card's approval cannot settle it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<i64>,
}

/// The bot a `delete_bot` card would delete, as it was when the card was
/// made. The card's context lists the work that stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotDeletion {
    pub bot_id: Uuid,
    pub name: String,
    pub avatar: crate::app::bots::Avatar,
    pub model: Option<String>,
    pub category: Option<String>,
}

/// The inbox a `mail_disconnect` card would remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailRemoval {
    pub account_id: Uuid,
    pub address: String,
}

/// Email attachments a bot asks to read, as the card lists them. Approving
/// lets the bot read and search the text of exactly these files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MailAttachments {
    pub account_id: Uuid,
    pub address: String,
    pub message: crate::connectors::types::MessageRef,
    pub subject: Option<String>,
    pub from: Vec<String>,
    pub files: Vec<MailFile>,
}

/// One attachment on a [`MailAttachments`] card: its part index from
/// `read`, and the name, type and decoded size the message declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MailFile {
    pub part: usize,
    pub name: String,
    pub media_type: String,
    pub size: u64,
}

fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
        hasher.update(b"\0");
    }
    hasher.finalize().into()
}

impl From<Kind> for wire::QuestionKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Choice => Self::Choice,
            Kind::Destructive => Self::Destructive,
            Kind::External => Self::External,
        }
    }
}

impl From<Status> for wire::QuestionStatus {
    fn from(status: Status) -> Self {
        match status {
            Status::Pending => Self::Pending,
            Status::Answered => Self::Answered,
            Status::Skipped => Self::Skipped,
            Status::Cancelled => Self::Cancelled,
        }
    }
}

impl From<&Action> for wire::QuestionAction {
    fn from(action: &Action) -> Self {
        match action {
            Action::McpConnection { mcp_connection } => Self::McpConnection {
                mcp_connection: wire::McpConsent {
                    id: mcp_connection.id.clone().into(),
                    provider: mcp_connection.provider.clone(),
                    scopes: mcp_connection.scopes.clone(),
                },
            },
            Action::EmailConnection { email_connection } => {
                use crate::connectors::types::Provider;
                Self::EmailConnection {
                    email_connection: wire::EmailConsent {
                        provider: match email_connection.provider {
                            Provider::Gmail => else_wire::email::EmailProvider::Gmail,
                            Provider::Microsoft => else_wire::email::EmailProvider::Microsoft,
                            Provider::Yahoo => else_wire::email::EmailProvider::Yahoo,
                            Provider::Icloud => else_wire::email::EmailProvider::Icloud,
                            Provider::Imap => else_wire::email::EmailProvider::Imap,
                        },
                        address: email_connection.address.clone(),
                        replaces: email_connection.replaces,
                    },
                }
            }
            Action::Tool {
                tool,
                args,
                task_scope,
            } => Self::Tool {
                tool: tool.clone(),
                args: args.clone(),
                task_scope: task_scope.as_ref().map(Into::into),
            },
            Action::Host { host, task_scope } => Self::Host {
                host: host.clone(),
                task_scope: task_scope.as_ref().map(Into::into),
            },
            Action::Permission { permission } => Self::Permission {
                permission: permission.into(),
            },
            Action::BotDeletion { bot_deletion } => Self::BotDeletion {
                bot_deletion: wire::BotDeletion {
                    bot_id: Some(bot_deletion.bot_id),
                    name: bot_deletion.name.clone(),
                    avatar: bot_deletion.avatar.into(),
                    model: bot_deletion.model.clone(),
                    category: bot_deletion.category.clone(),
                },
            },
            Action::EmailRemoval { email_removal } => Self::EmailRemoval {
                email_removal: wire::EmailRemoval {
                    account_id: email_removal.account_id,
                    address: email_removal.address.clone(),
                },
            },
            Action::MailAttachments { mail_attachments } => Self::MailAttachments {
                mail_attachments: wire::MailAttachments {
                    account_id: Some(mail_attachments.account_id),
                    address: mail_attachments.address.clone(),
                    message: Some(wire::MailMessageRef {
                        folder: mail_attachments.message.folder.clone(),
                        uid_validity: mail_attachments.message.uid_validity,
                        uid: mail_attachments.message.uid,
                    }),
                    subject: mail_attachments.subject.clone(),
                    from: mail_attachments.from.clone(),
                    files: mail_attachments
                        .files
                        .iter()
                        .map(|file| wire::MailFile {
                            part: Some(file.part),
                            name: file.name.clone(),
                            media_type: file.media_type.clone(),
                            size: file.size,
                        })
                        .collect(),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_bare_always_allow_reply_answers_a_card() {
        for text in [
            "Always approve.",
            "always allow",
            "Yes, always allow it please!",
            "  Approve always ",
            "ok always allow this",
        ] {
            assert!(says_always_allow(text), "{text}");
        }
        for text in [
            "",
            "always",
            "Always allow, but not on main.",
            "Don't always allow this",
            "always allow deletes too",
            "approve",
            "Always allow for every bot",
        ] {
            assert!(!says_always_allow(text), "{text}");
        }
    }
}
