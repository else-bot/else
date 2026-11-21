use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

wire_state! {
    /// A question's kind; a newer server's kind keeps its text so an
    /// approval card can still name it.
    pub enum QuestionKind {
        Choice = "choice",
        Destructive = "destructive",
        External = "external",
    }
    _ => Other(String)
}

wire_state! {
    pub enum QuestionStatus {
        Pending = "pending",
        Answered = "answered",
        Skipped = "skipped",
        Cancelled = "cancelled",
    }
    _ => Unknown
}

/// One option of a bot's question, by its stable id and shown label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionChoice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum QuestionAnswer {
    Single(String),
    Multiple(Vec<String>),
}

impl QuestionAnswer {
    pub fn single(&self) -> Option<&str> {
        match self {
            Self::Single(id) => Some(id),
            Self::Multiple(_) => None,
        }
    }

    pub fn contains(&self, id: &str) -> bool {
        match self {
            Self::Single(chosen) => chosen == id,
            Self::Multiple(chosen) => chosen.iter().any(|chosen| chosen == id),
        }
    }
}

/// A bot's question to the owner: a choice between options, or approval of
/// one destructive or external action. Answered by choosing on its card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub id: Uuid,
    #[serde(default)]
    pub bot_id: Option<Uuid>,
    #[serde(default)]
    pub turn_id: Option<Uuid>,
    pub kind: QuestionKind,
    pub title: String,
    #[serde(default)]
    pub context: Option<String>,
    pub choices: Vec<QuestionChoice>,
    #[serde(default)]
    pub multiple: bool,
    #[serde(default)]
    pub action: Option<QuestionAction>,
    pub status: QuestionStatus,
    /// The selected option id or ids once answered.
    #[serde(default)]
    pub answer: Option<QuestionAnswer>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub settled_at: Option<DateTime<Utc>>,
    /// What became of the approved tool call once it was let run.
    #[serde(default)]
    pub used: Option<QuestionUse>,
}

/// An approved tool call the server let run, and what its tool returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "lowercase")]
pub enum QuestionUse {
    /// No result recorded yet: still running, or lost.
    Started {
        at: DateTime<Utc>,
    },
    Completed {
        at: DateTime<Utc>,
    },
    Error {
        at: DateTime<Utc>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum QuestionAction {
    McpConnection {
        mcp_connection: McpConsent,
    },
    EmailConnection {
        email_connection: EmailConsent,
    },
    Tool {
        tool: String,
        #[serde(default)]
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
        permission: QuestionPermission,
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
    Unknown(UnknownAction),
}

/// An action this client does not recognize. Known shapes still validate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct UnknownAction(serde_json::Map<String, Value>);

impl<'de> Deserialize<'de> for UnknownAction {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = serde_json::Map::<String, Value>::deserialize(deserializer)?;
        let known = [
            "mcp_connection",
            "email_connection",
            "tool",
            "host",
            "permission",
            "bot_deletion",
            "email_removal",
            "mail_attachments",
        ];
        if known
            .iter()
            .any(|key| fields.get(*key).is_some_and(|value| !value.is_null()))
        {
            return Err(serde::de::Error::custom("invalid known question action"));
        }
        Ok(Self(fields))
    }
}

impl QuestionAction {
    pub fn tool(&self) -> Option<&str> {
        match self {
            Self::Tool { tool, .. } => Some(tool),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpConsent {
    pub id: String,
    pub provider: Option<String>,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailConsent {
    pub provider: EmailProvider,
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<i64>,
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
            Self::Imap => "Other email",
            Self::Unknown => "Unknown email provider",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotDeletion {
    #[serde(default)]
    pub bot_id: Option<Uuid>,
    pub name: String,
    #[serde(default)]
    pub avatar: Value,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailRemoval {
    pub account_id: Uuid,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailAttachments {
    #[serde(default)]
    pub account_id: Option<Uuid>,
    pub address: String,
    #[serde(default)]
    pub message: Option<MailMessageRef>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub from: Vec<String>,
    pub files: Vec<MailFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailMessageRef {
    pub folder: String,
    pub uid_validity: u32,
    pub uid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailFile {
    #[serde(default)]
    pub part: Option<usize>,
    pub name: String,
    pub media_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskScope {
    pub task: String,
    pub target: String,
}

wire_state! {
    pub enum TaskCoverage {
        Host = "host",
        Computer = "computer",
        Both = "both",
    }
    _ => Unknown
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum QuestionPermission {
    Bot,
    Routine {
        routine_id: Uuid,
        prompt_revision: i64,
        #[serde(default = "destructive")]
        kind: QuestionKind,
    },
    McpTool {
        mcp_id: String,
        tool: String,
        #[serde(default)]
        kind: Option<QuestionKind>,
    },
    ComputerTool {
        tool: String,
        kind: QuestionKind,
    },
    Task {
        #[serde(flatten)]
        task_scope: TaskScope,
        #[serde(default)]
        kind: Option<QuestionKind>,
        #[serde(default)]
        covers: Option<TaskCoverage>,
    },
    #[serde(other)]
    Unknown,
}

fn destructive() -> QuestionKind {
    QuestionKind::Destructive
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionSettlement {
    pub question: Question,
    pub requeued: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permission {
    pub scope: String,
    pub granted_at: DateTime<Utc>,
    pub question_id: Option<Uuid>,
    pub target: Option<QuestionPermission>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineGrant {
    pub routine_id: Uuid,
    pub kind: QuestionKind,
    pub granted_at: DateTime<Utc>,
    pub question_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    pub permissions: Vec<Permission>,
    pub routine_grants: Vec<RoutineGrant>,
}

impl MailFile {
    /// The file's kind as people name it: PDF, HTML, or its subtype.
    pub fn kind(&self) -> String {
        match self.media_type.as_str() {
            "application/pdf" => "PDF".into(),
            "text/html" | "application/xhtml+xml" => "HTML".into(),
            "text/plain" => "Text".into(),
            other => other
                .rsplit('/')
                .next()
                .unwrap_or(other)
                .to_ascii_uppercase(),
        }
    }
}

impl Question {
    pub fn mcp_tool_scope(&self) -> Option<(&str, &str)> {
        match self.action.as_ref()? {
            QuestionAction::Tool { tool, args, .. } if tool == "mcp_request" => {
                Some((args["id"].as_str()?, args["tool"].as_str()?))
            }
            QuestionAction::Permission {
                permission: QuestionPermission::McpTool { mcp_id, tool, .. },
            } => Some((mcp_id, tool)),
            _ => None,
        }
    }

    pub fn task_scope(&self) -> Option<&TaskScope> {
        match self.action.as_ref()? {
            QuestionAction::Tool { task_scope, .. } | QuestionAction::Host { task_scope, .. } => {
                task_scope.as_ref()
            }
            QuestionAction::Permission {
                permission: QuestionPermission::Task { task_scope, .. },
            } => Some(task_scope),
            _ => None,
        }
    }

    pub fn mcp_connection(&self) -> Option<&McpConsent> {
        match self.action.as_ref()? {
            QuestionAction::McpConnection { mcp_connection } => Some(mcp_connection),
            _ => None,
        }
    }

    pub fn mail_attachments(&self) -> Option<&MailAttachments> {
        match self.action.as_ref()? {
            QuestionAction::MailAttachments { mail_attachments } => Some(mail_attachments),
            _ => None,
        }
    }

    pub fn bot_deletion(&self) -> Option<&BotDeletion> {
        match self.action.as_ref()? {
            QuestionAction::BotDeletion { bot_deletion } => Some(bot_deletion),
            _ => None,
        }
    }

    pub fn email_connection(&self) -> Option<&EmailConsent> {
        match self.action.as_ref()? {
            QuestionAction::EmailConnection { email_connection } => Some(email_connection),
            _ => None,
        }
    }

    pub fn is_pending(&self) -> bool {
        self.status == QuestionStatus::Pending
    }

    pub fn is_approval(&self) -> bool {
        self.kind != QuestionKind::Choice
    }

    pub fn chosen(&self) -> Option<&QuestionChoice> {
        let answer = self.answer.as_ref()?.single()?;
        self.choices.iter().find(|choice| choice.id == answer)
    }
}
