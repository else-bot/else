//! The client's conversation model: a snapshot plus contiguously applied
//! stream events. Any inconsistency (a gap, an unknown item, an offset
//! mismatch, an unknown kind) asks the caller to reload a snapshot rather than
//! guessing, so a bubble is never duplicated or corrupted.

mod skills;
pub use skills::*;

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use else_wire::items::ItemKind;
use else_wire::questions::{QuestionKind, QuestionStatus};
use else_wire::routines::{PausedReason, RunStatus, SkipReason};
use else_wire::rules::RuleAuthor;
use else_wire::secrets::SecretRequestStatus;
use else_wire::turns::TurnStatus;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::MailTrigger;
use crate::api::socials::Trigger;
use else_wire::bots::Activity;
use else_wire::bots::Bot;

mod outgoing;
pub use outgoing::{DraftAttachment, DraftEdit, DraftFile, DraftStatus, OutgoingDraft};

/// One event of a `bot:<id>` channel: its cursor, kind, and JSON data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub id: Option<String>,
    pub event: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Item {
    pub id: Uuid,
    pub seq: i64,
    pub turn_id: Uuid,
    pub request_id: Option<Uuid>,
    pub kind: ItemKind,
    pub text: String,
    #[serde(default)]
    pub parts: Vec<crate::attachments::MessagePart>,
    pub provider_id: Option<String>,
    /// Absent from older servers.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// Set on the owner's feedback message sent as a reaction.
    #[serde(default)]
    pub reaction: Option<Reaction>,
    #[serde(default)]
    pub bot_task: Option<BotTask>,
    #[serde(default)]
    pub bot_reply: Option<BotReply>,
    #[serde(default)]
    pub routine_update: Option<RoutineUpdate>,
    #[serde(default)]
    pub bot_created: Option<BotCreated>,
    #[serde(default)]
    pub rules_update: Option<RulesUpdate>,
    /// The provider attempt that wrote an assistant item. Absent from older servers.
    #[serde(default)]
    pub attempt: Option<i64>,
}

/// How far a joined message got into the reply it joined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteerStatus {
    Pending,
    Sending,
    /// The reply took it and answers it.
    Delivered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleChange {
    Added,
    Changed,
    Removed,
    Restored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffOp {
    Same,
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DiffLine {
    pub op: DiffOp,
    pub text: String,
}

/// The card for one saved version of a bot's rules.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RulesUpdate {
    pub version: i64,
    pub section: String,
    pub change: RuleChange,
    pub owner_words: String,
    pub diff: Vec<DiffLine>,
    /// When the owner undid this change.
    #[serde(default)]
    pub undone_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RuleSection {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RulesVersion {
    pub version: i64,
    pub section: String,
    pub change: RuleChange,
    pub owner_words: String,
    pub author: RuleAuthor,
    pub restored_from: Option<i64>,
    pub diff: Vec<DiffLine>,
    pub created_at: DateTime<Utc>,
    /// When the owner undid this version by restoring the one before it.
    #[serde(default)]
    pub undone_at: Option<DateTime<Utc>>,
}

/// A bot's rules file with every saved version, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RulesFile {
    pub version: i64,
    pub markdown: String,
    pub sections: Vec<RuleSection>,
    pub history: Vec<RulesVersion>,
}

impl RulesFile {
    /// Titles of the rules changed after version `seen`, lowercased.
    pub fn changed_since(&self, seen: i64) -> std::collections::HashSet<String> {
        self.history
            .iter()
            .filter(|v| {
                v.version > seen && v.change != RuleChange::Removed && !v.section.is_empty()
            })
            .map(|v| v.section.to_lowercase())
            .collect()
    }
}

/// The main bot's notice that `creator_bot_id` created `bot_id`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BotCreated {
    pub bot_id: Uuid,
    pub name: String,
    pub creator_bot_id: Uuid,
    pub creator_name: String,
    pub purpose: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RoutineUpdate {
    pub routine_id: Uuid,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BotTask {
    pub sender_bot_id: Uuid,
    pub sender_name: String,
    pub prompt: String,
    /// The delegating bot's short name for the task. Absent from older
    /// servers and older tasks.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(flatten)]
    pub declared: Declared,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BotReply {
    pub sender_bot_id: Uuid,
    pub sender_name: String,
    pub prompt: String,
    pub status: BotReplyStatus,
    /// Why a failed task failed, apart from the progress it wrote. Absent
    /// from older servers.
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(flatten)]
    pub declared: Declared,
    /// The owner notices of the task and of the tasks it delegated, oldest
    /// first.
    #[serde(default)]
    pub asks: Vec<ExchangeAsk>,
}

/// What a delegated task declared it needs from the owner. The bot's
/// judgment, shown as its words, never an approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerNeed {
    Nothing,
    /// Done, with the bot's one line for the owner.
    Fyi(String),
    Decision,
    Blocked,
}

/// A task's declared need. `None` when it declared nothing: an older
/// server or task, or a bot that forgot. Never read as "nothing".
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(from = "DeclaredNeed")]
pub struct Declared(pub Option<OwnerNeed>);

#[derive(Deserialize)]
struct DeclaredNeed {
    #[serde(default)]
    owner_need: Option<String>,
    #[serde(default)]
    owner_line: Option<String>,
}

impl From<DeclaredNeed> for Declared {
    fn from(raw: DeclaredNeed) -> Self {
        Self(match (raw.owner_need.as_deref(), raw.owner_line) {
            (Some("nothing"), _) => Some(OwnerNeed::Nothing),
            (Some("fyi"), Some(line)) => Some(OwnerNeed::Fyi(line)),
            (Some("decision"), _) => Some(OwnerNeed::Decision),
            (Some("blocked"), _) => Some(OwnerNeed::Blocked),
            _ => None,
        })
    }
}

/// An owner notice shown on an exchange, with the name of the bot asking.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ExchangeAsk {
    #[serde(flatten)]
    pub notice: OwnerNotice,
    pub bot_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotReplyStatus {
    Completed,
    Failed,
    Cancelled,
}

/// The owner's reaction to one of the bot's messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub item_id: Uuid,
    pub kind: ReactionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactionKind {
    ThumbsUp,
    ThumbsDown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TurnError {
    pub code: String,
    pub message: Option<String>,
}

/// A bot's request for credentials: metadata only, never a value.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SecretRequest {
    pub description: Option<String>,
    pub website: Option<String>,
    pub id: Uuid,
    pub name: String,
    pub reason: String,
    pub env: Vec<String>,
    pub status: SecretRequestStatus,
}

impl SecretRequest {
    pub fn is_pending(&self) -> bool {
        self.status == SecretRequestStatus::Pending
    }
}

/// A bot's request that the owner take over its computer. Its turn waits
/// until the owner hands the computer back or declines.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ComputerHandoff {
    pub id: Uuid,
    pub reason: String,
    pub status: HandoffStatus,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub taken_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub settled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffStatus {
    Pending,
    /// The owner controls the computer and has not handed it back.
    Taken,
    HandedBack,
    Dismissed,
    Skipped,
    Cancelled,
}

impl HandoffStatus {
    pub fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::Taken)
    }
}

pub const APPROVE: &str = "approve";
pub const DENY: &str = "deny";
pub const APPROVE_FOREVER: &str = "approve_forever";
pub const APPROVE_ROUTINE: &str = "approve_routine";
pub const APPROVE_TOOL: &str = "approve_tool";
pub const APPROVE_TASK: &str = "approve_task";

/// One option of a bot's question, by its stable id and shown label.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct QuestionChoice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Question {
    pub id: Uuid,
    pub kind: QuestionKind,
    pub title: String,
    #[serde(default)]
    pub action: Option<QuestionAction>,
    #[serde(default)]
    pub context: Option<String>,
    pub choices: Vec<QuestionChoice>,
    #[serde(default)]
    pub multiple: bool,
    pub status: QuestionStatus,
    /// The selected option id or ids once answered.
    #[serde(default)]
    pub answer: Option<QuestionAnswer>,
    #[serde(default)]
    pub settled_at: Option<DateTime<Utc>>,
    /// What became of the approved tool call once it was let run.
    #[serde(default)]
    pub used: Option<QuestionUse>,
}

/// An approved tool call the server let run, and what its tool returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct QuestionAction {
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub args: serde_json::Value,
    #[serde(default)]
    pub mcp_connection: Option<crate::api::McpConsent>,
    #[serde(default)]
    pub email_connection: Option<crate::api::EmailConsent>,
    #[serde(default)]
    pub task_scope: Option<TaskScope>,
    #[serde(default)]
    pub permission: Option<QuestionPermission>,
    #[serde(default)]
    pub bot_deletion: Option<BotDeletion>,
    #[serde(default)]
    pub mail_attachments: Option<MailAttachments>,
}

/// Email attachments a bot asks to read, as the card lists them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MailAttachments {
    pub address: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub from: Vec<String>,
    pub files: Vec<MailFile>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MailFile {
    pub name: String,
    pub media_type: String,
    pub size: u64,
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

/// The bot a deletion card would delete, as it was when the card was made.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BotDeletion {
    pub name: String,
    #[serde(default)]
    pub avatar: else_wire::bots::Avatar,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TaskScope {
    pub task: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "scope", rename_all = "lowercase")]
pub enum QuestionPermission {
    #[serde(rename = "mcp_tool")]
    McpTool { mcp_id: String, tool: String },
    Task {
        #[serde(flatten)]
        task_scope: TaskScope,
    },
    #[serde(other)]
    Other,
}

impl Question {
    pub fn mcp_tool_scope(&self) -> Option<(&str, &str)> {
        let action = self.action.as_ref()?;
        if action.tool.as_deref() == Some("mcp_request") {
            return Some((action.args["id"].as_str()?, action.args["tool"].as_str()?));
        }
        match &action.permission {
            Some(QuestionPermission::McpTool { mcp_id, tool }) => Some((mcp_id, tool)),
            _ => None,
        }
    }

    pub fn task_scope(&self) -> Option<&TaskScope> {
        let action = self.action.as_ref()?;
        action.task_scope.as_ref().or(match &action.permission {
            Some(QuestionPermission::Task { task_scope }) => Some(task_scope),
            _ => None,
        })
    }

    pub fn mcp_connection(&self) -> Option<&crate::api::McpConsent> {
        self.action.as_ref()?.mcp_connection.as_ref()
    }

    pub fn mail_attachments(&self) -> Option<&MailAttachments> {
        self.action.as_ref()?.mail_attachments.as_ref()
    }

    pub fn bot_deletion(&self) -> Option<&BotDeletion> {
        self.action.as_ref()?.bot_deletion.as_ref()
    }

    pub fn email_connection(&self) -> Option<&crate::api::EmailConsent> {
        self.action.as_ref()?.email_connection.as_ref()
    }

    pub fn is_pending(&self) -> bool {
        self.status == QuestionStatus::Pending
    }

    /// An approval card, as opposed to a plain choice.
    pub fn is_approval(&self) -> bool {
        self.kind != QuestionKind::Choice
    }

    pub fn chosen(&self) -> Option<&QuestionChoice> {
        let answer = self.answer.as_ref()?.single()?;
        self.choices.iter().find(|c| c.id == answer)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerNoticeKind {
    Blocked,
    Decision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerNoticeStatus {
    Pending,
    Replied,
    Dismissed,
    /// A card on the same turn asks the owner instead.
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OwnerNotice {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub turn_id: Uuid,
    pub kind: OwnerNoticeKind,
    pub summary: String,
    pub tried: Option<String>,
    pub status: OwnerNoticeStatus,
    pub created_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
}

/// How often the attempt limit cut a turn that continued, of how many
/// continuations a request may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct LimitContinued {
    pub count: i64,
    pub max: i64,
    pub minutes: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Turn {
    pub id: Uuid,
    pub request_id: Uuid,
    pub turn_no: i64,
    /// The turn's latest provider attempt. Absent from older servers.
    #[serde(default)]
    pub attempt: Option<i64>,
    /// Set once the attempt limit cut the turn and it continued. Absent
    /// from older servers.
    #[serde(default)]
    pub limit_continued: Option<LimitContinued>,
    pub provider_id: String,
    pub status: TurnStatus,
    pub error: Option<TurnError>,
    pub effect_uncertain: bool,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub reaction: Option<Reaction>,
    /// The running reply this message joined. Absent from older servers.
    #[serde(default)]
    pub steered_into: Option<Uuid>,
    /// How far this message got into that reply.
    #[serde(default)]
    pub steer_status: Option<SteerStatus>,
    #[serde(default)]
    pub secret_requests: Vec<SecretRequest>,
    #[serde(default)]
    pub skill_installs: Vec<SkillInstall>,
    /// Absent from servers without owner questions.
    #[serde(default)]
    pub questions: Vec<Question>,
    #[serde(default)]
    pub routine: Option<RoutineRun>,
    /// The emails whose arrival started this turn. Absent from older servers.
    #[serde(default)]
    pub mail: Vec<TurnMail>,
    #[serde(default)]
    pub outgoing_drafts: Vec<OutgoingDraft>,
    #[serde(default)]
    pub social_setups: Vec<crate::api::socials::SocialSetup>,
    /// Tasks this turn handed to other bots. Absent from older servers.
    #[serde(default)]
    pub delegations: Vec<Delegation>,
    /// MCP jobs this turn waits on. Absent from older servers.
    #[serde(default)]
    pub mcp_waits: Vec<McpWait>,
    /// Background computer jobs this turn waits on. Absent from older servers.
    #[serde(default)]
    pub computer_job_waits: Vec<ComputerJobWait>,
    /// Requests that the owner take over the computer. Absent from older servers.
    #[serde(default)]
    pub computer_handoffs: Vec<ComputerHandoff>,
    #[serde(default)]
    pub owner_notices: Vec<OwnerNotice>,
}

/// An email that started a turn, which the owner can open in the reader.
/// `message` is where it arrived; the server follows it from there.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TurnMail {
    pub account_id: Uuid,
    pub message: crate::api::MessageRef,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MailLocation {
    #[default]
    Arrived,
    Archived(MailMove),
    Trashed(MailMove),
    Moved(MailMove),
    Deleted(MailDeletion),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MailMove {
    pub folder: String,
    pub by_bot_id: Option<Uuid>,
    pub by_turn_id: Option<Uuid>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MailDeletion {
    pub by_bot_id: Option<Uuid>,
    pub by_turn_id: Option<Uuid>,
    pub at: DateTime<Utc>,
}

impl MailLocation {
    /// The turn that moved or deleted the email, when a bot did.
    pub fn by_turn(&self) -> Option<Uuid> {
        match self {
            Self::Arrived => None,
            Self::Archived(m) | Self::Trashed(m) | Self::Moved(m) => m.by_turn_id,
            Self::Deleted(d) => d.by_turn_id,
        }
    }
}

/// A job in an installed MCP that a waiting turn rechecks until it ends.
/// A pending wait is the only hold of its attempt. The details are absent
/// from older servers.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct McpWait {
    pub id: Uuid,
    pub mcp_id: String,
    #[serde(default)]
    pub tool: String,
    /// What the bot waits for, in its own words.
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub interval_seconds: i64,
    #[serde(default)]
    pub deadline_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub checks: i64,
    /// The job's status in the last check, when it was readable.
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub last_result: Option<String>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

/// A background job in the bot's computer that a waiting turn continues
/// after. A pending wait is the only hold of its attempt. The details are
/// absent from older servers.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ComputerJobWait {
    pub id: Uuid,
    pub job_id: Uuid,
    #[serde(default)]
    pub command: String,
    /// What the bot said the job is for. Absent for jobs started without
    /// one and from older servers.
    #[serde(default)]
    pub purpose: Option<String>,
    #[serde(default)]
    pub job_status: Option<String>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

/// One thing a bot waits on, as the owner sees it above the composer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Wait<'a> {
    Mcp(&'a McpWait),
    Job(&'a ComputerJobWait),
    Task(&'a Delegation),
}

impl Wait<'_> {
    /// The ID the server stops it by.
    pub fn id(&self) -> Uuid {
        match self {
            Self::Mcp(wait) => wait.id,
            Self::Job(wait) => wait.id,
            Self::Task(task) => task.id,
        }
    }

    pub fn created_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Mcp(wait) => wait.created_at,
            Self::Job(wait) => wait.created_at,
            Self::Task(task) => Some(task.created_at),
        }
    }

    /// Whether the owner can still stop it: a task that ended only waits
    /// for its sender to read the outcome.
    pub fn stoppable(&self) -> bool {
        match self {
            Self::Mcp(_) | Self::Job(_) => true,
            Self::Task(task) => !task.status.settled(),
        }
    }
}

impl Turn {
    /// The reply this message joined took it, so that reply answers it.
    pub fn taken_by_reply(&self) -> bool {
        self.steer_status == Some(SteerStatus::Delivered)
    }

    /// Whether the turn waits on work of its own rather than on the owner.
    pub fn waits_on_job(&self) -> bool {
        !self.mcp_waits.is_empty() || !self.computer_job_waits.is_empty()
    }

    /// The tasks it handed to other bots that have not ended.
    pub fn open_delegations(&self) -> Vec<&Delegation> {
        self.delegations
            .iter()
            .filter(|d| !d.status.settled())
            .collect()
    }

    /// Whether the owner holds it: a pending skill install, secret, draft,
    /// question, or account setup.
    pub fn waits_on_owner(&self) -> bool {
        self.skill_installs
            .iter()
            .any(|r| r.status == SkillInstallStatus::Pending)
            || self.secret_requests.iter().any(|r| r.is_pending())
            || self
                .outgoing_drafts
                .iter()
                .any(|d| d.status == DraftStatus::Pending)
            || self.questions.iter().any(|q| q.is_pending())
            || self.social_setups.iter().any(|setup| setup.is_pending())
    }

    /// Parked only on tasks it handed to other bots.
    pub fn waits_on_bots(&self) -> bool {
        self.status == TurnStatus::Waiting
            && !self.waits_on_job()
            && !self.waits_on_owner()
            && !self.open_delegations().is_empty()
    }
}

/// A task a turn handed to another bot. Its settled reply is the item
/// with the same ID.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Delegation {
    pub id: Uuid,
    /// The provider attempt that sent it; a council's tasks share one.
    pub attempt: i64,
    pub target_bot_id: Uuid,
    pub target_name: String,
    pub task: String,
    pub status: DelegationStatus,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// The delegating turn has read the outcome. Absent from older servers.
    #[serde(default)]
    pub seen: bool,
    /// Messages the task has written so far.
    #[serde(default)]
    pub updates: i64,
    /// While the task runs, the start of its latest message.
    #[serde(default)]
    pub progress: Option<String>,
}

impl Delegation {
    /// Tasks one provider attempt sent share a handoff. A task carried over
    /// from an earlier owner message has attempt 0 and stands alone.
    pub fn shares_handoff(&self, other: &Delegation) -> bool {
        self.attempt == other.attempt && (self.attempt > 0 || self.id == other.id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl DelegationStatus {
    pub fn settled(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RoutineRun {
    pub routine_id: Uuid,
    pub name: String,
    pub due_at: DateTime<Utc>,
    /// The routine is a reminder, due once. Absent from older servers.
    #[serde(default)]
    pub reminder: bool,
    /// The run completed with nothing to report, so it alerts no one.
    #[serde(default)]
    pub no_news: bool,
}

impl RoutineRun {
    /// What started the run, as the conversation labels it.
    pub fn label(&self) -> &'static str {
        if self.reminder {
            "Reminder"
        } else {
            "Scheduled routine"
        }
    }
}

/// The last due occurrence of a routine: the turn it started, or why it
/// was skipped.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LastRun {
    pub due_at: DateTime<Utc>,
    pub status: RunStatus,
    pub skip_reason: Option<SkipReason>,
    pub error: Option<TurnError>,
}

/// The days (`mon` to `sun`) and the span, end excluded, when a routine's
/// runs may start, in its time zone.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ActiveHours {
    pub days: Vec<String>,
    pub start: String,
    pub end: String,
}

/// A bot's saved task, run on a cron schedule in its conversation, or
/// once at `run_at` for a reminder.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Routine {
    pub id: Uuid,
    pub name: String,
    pub prompt: String,
    pub cron: String,
    /// When a reminder runs once; `None` for a routine on a cron schedule.
    #[serde(default)]
    pub run_at: Option<DateTime<Utc>>,
    pub timezone: String,
    /// When runs may start; `None` is any time.
    #[serde(default)]
    pub active_hours: Option<ActiveHours>,
    pub enabled: bool,
    #[serde(default)]
    pub model: Option<crate::api::ProfileChoice>,
    #[serde(default)]
    pub requested_model: Option<String>,
    pub next_run_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub last_run: Option<LastRun>,
    /// Consecutive failed runs the owner has not dismissed.
    #[serde(default)]
    pub failed_runs: i64,
    /// Why else.bot switched the routine off.
    #[serde(default)]
    pub paused_reason: Option<PausedReason>,
    /// The last run is the owner's retry of the paused routine, not yet
    /// settled.
    #[serde(default)]
    pub retrying: bool,
    /// A run with nothing to report may end with no news, alerting no one.
    #[serde(default)]
    pub quiet: bool,
}

/// A bot's webhook: an authenticated call to its URL wakes the bot with
/// `prompt`, at most as often as its pace allows.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Webhook {
    pub id: Uuid,
    pub name: String,
    pub prompt: String,
    pub min_interval_minutes: i64,
    pub enabled: bool,
    pub last_wake: Option<WebhookWake>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WebhookWake {
    pub woke_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WatcherStatus {
    Waiting {
        next_check_at: Option<DateTime<Utc>>,
    },
    Checking,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Watcher {
    pub id: Uuid,
    pub purpose: String,
    pub until: DateTime<Utc>,
    #[serde(flatten)]
    pub status: WatcherStatus,
    pub last_status: Option<String>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub last_output: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    pub selected_provider_id: Option<String>,
    pub event_cursor: String,
    pub items: Vec<Item>,
    pub turns: Vec<Turn>,
    pub activity: Activity,
    pub older_before_seq: Option<i64>,
    /// Absent from servers without routines.
    #[serde(default)]
    pub routines: Vec<Routine>,
    #[serde(default)]
    pub social_triggers: Vec<Trigger>,
    /// Absent from servers without email wake-ups.
    #[serde(default)]
    pub mail_triggers: Vec<MailTrigger>,
    /// Absent from servers that don't list webhooks in the snapshot.
    #[serde(default)]
    pub webhooks: Vec<Webhook>,
    #[serde(default)]
    pub watchers: Vec<Watcher>,
    /// The kinds of drafts the owner approves always.
    #[serde(default)]
    pub draft_grants: Vec<serde_json::Value>,
    /// Absent from servers without rules.
    #[serde(default)]
    pub rules_version: i64,
}

/// Shown items loaded around one message, between two the owner already
/// has.
#[derive(Debug, Clone, Deserialize)]
pub struct Window {
    pub items: Vec<Item>,
    pub turns: Vec<Turn>,
    /// Shown items still missing below the first item.
    pub older_gap: i64,
    /// Shown items still missing above the last item.
    pub newer_gap: i64,
}

/// The messages a search found, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SearchHits {
    /// Matches in all of them.
    pub total: usize,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct SearchHit {
    pub item_id: Uuid,
    pub seq: i64,
    /// Matches in this message.
    pub count: usize,
}

/// How many shown items an unloaded stretch holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapSize {
    Counted(i64),
    /// A live item landed inside a counted stretch and split it; loading
    /// either side counts it again.
    Uncounted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reset {
    Gap { expected: i64, got: i64 },
    BadCursor,
    UnknownItem,
    OffsetMismatch,
    UnknownKind(String),
    Malformed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Already applied (a replayed duplicate); nothing changed.
    Duplicate,
    Changed,
}

#[derive(Debug, Clone)]
pub struct Conversation {
    /// `<server_instance_id>:<bot_id>`; events must carry the same prefix.
    cursor_prefix: String,
    applied: i64,
    items: BTreeMap<i64, Item>,
    seq_of: HashMap<Uuid, i64>,
    turns: HashMap<Uuid, Turn>,
    pub older_before_seq: Option<i64>,
    /// Unloaded stretches between loaded items, keyed by the seq of the
    /// loaded item just above each.
    gaps: BTreeMap<i64, GapSize>,
    pub selected_provider_id: Option<String>,
    pub activity: Option<Activity>,
    routines: Vec<Routine>,
    social_triggers: Vec<Trigger>,
    mail_triggers: Vec<MailTrigger>,
    webhooks: Vec<Webhook>,
    watchers: Vec<Watcher>,
    draft_grants: Vec<serde_json::Value>,
    rules_version: i64,
    /// Read from the page saved on disk, so possibly behind the server.
    saved: bool,
}

fn split_cursor(cursor: &str) -> Option<(&str, i64)> {
    let (prefix, seq) = cursor.rsplit_once(':')?;
    Some((prefix, seq.parse().ok()?))
}

impl Conversation {
    pub fn from_snapshot(snapshot: Snapshot) -> Result<Self, Reset> {
        let (prefix, applied) = split_cursor(&snapshot.event_cursor).ok_or(Reset::BadCursor)?;
        let mut conversation = Self {
            cursor_prefix: prefix.to_owned(),
            applied,
            items: BTreeMap::new(),
            seq_of: HashMap::new(),
            turns: HashMap::new(),
            older_before_seq: snapshot.older_before_seq,
            gaps: BTreeMap::new(),
            selected_provider_id: snapshot.selected_provider_id,
            activity: Some(snapshot.activity),
            routines: snapshot.routines,
            social_triggers: snapshot.social_triggers,
            mail_triggers: snapshot.mail_triggers,
            webhooks: snapshot.webhooks,
            watchers: snapshot.watchers,
            draft_grants: snapshot.draft_grants,
            rules_version: snapshot.rules_version,
            saved: false,
        };
        for item in snapshot.items {
            conversation.upsert_item(item);
        }
        for turn in snapshot.turns {
            conversation.turns.insert(turn.id, turn);
        }
        Ok(conversation)
    }

    /// A conversation from a page saved on disk, shown until the server's.
    pub fn from_saved(snapshot: Snapshot) -> Result<Self, Reset> {
        Ok(Self {
            saved: true,
            ..Self::from_snapshot(snapshot)?
        })
    }

    pub fn is_saved(&self) -> bool {
        self.saved
    }

    /// The cursor of the last applied event, for `Last-Event-ID`.
    pub fn cursor(&self) -> String {
        format!("{}:{}", self.cursor_prefix, self.applied)
    }

    fn upsert_item(&mut self, item: Item) {
        if let Some(old_seq) = self.seq_of.insert(item.id, item.seq)
            && old_seq != item.seq
        {
            self.items.remove(&old_seq);
        }
        self.items.insert(item.seq, item);
    }

    /// Apply the next stream event. Replays at or below the applied cursor
    /// are ignored; anything else out of order requires a fresh snapshot.
    pub fn apply(&mut self, frame: &Frame) -> Result<Applied, Reset> {
        let (prefix, seq) = frame
            .id
            .as_deref()
            .and_then(split_cursor)
            .ok_or(Reset::BadCursor)?;
        if prefix != self.cursor_prefix {
            return Err(Reset::BadCursor);
        }
        if seq <= self.applied {
            return Ok(Applied::Duplicate);
        }
        if seq != self.applied + 1 {
            return Err(Reset::Gap {
                expected: self.applied + 1,
                got: seq,
            });
        }
        let data: serde_json::Value =
            serde_json::from_str(&frame.data).map_err(|_| Reset::Malformed)?;
        match frame.event.as_str() {
            "item.upsert" => {
                let item: Item =
                    serde_json::from_value(data["item"].clone()).map_err(|_| Reset::Malformed)?;
                if self.seq_of.contains_key(&item.id) {
                    self.upsert_item(item);
                } else {
                    self.insert_unseen(item);
                }
            }
            "item.delta" => {
                let item_id: Uuid = serde_json::from_value(data["item_id"].clone())
                    .map_err(|_| Reset::Malformed)?;
                let offset = data["offset_utf8"].as_u64().ok_or(Reset::Malformed)? as usize;
                let text = data["text"].as_str().ok_or(Reset::Malformed)?;
                let seq_of_item = *self.seq_of.get(&item_id).ok_or(Reset::UnknownItem)?;
                let item = self.items.get_mut(&seq_of_item).ok_or(Reset::UnknownItem)?;
                if item.text.len() != offset {
                    return Err(Reset::OffsetMismatch);
                }
                item.text.push_str(text);
            }
            "turn.updated" => {
                let turn: Turn =
                    serde_json::from_value(data["turn"].clone()).map_err(|_| Reset::Malformed)?;
                self.turns.insert(turn.id, turn);
            }
            "bot.updated" => {
                let bot: Bot =
                    serde_json::from_value(data["bot"].clone()).map_err(|_| Reset::Malformed)?;
                self.activity = bot.activity;
                if bot.selected_provider_id.is_some() {
                    self.selected_provider_id = bot.selected_provider_id;
                }
                if let Some(routines) = data.get("routines") {
                    self.routines =
                        serde_json::from_value(routines.clone()).map_err(|_| Reset::Malformed)?;
                }
                if let Some(version) = data.get("rules_version").and_then(|v| v.as_i64()) {
                    self.rules_version = version;
                }
                self.social_triggers = data
                    .get("social_triggers")
                    .map(|triggers| serde_json::from_value(triggers.clone()))
                    .transpose()
                    .map_err(|_| Reset::Malformed)?
                    .unwrap_or_default();
                self.mail_triggers = data
                    .get("mail_triggers")
                    .map(|triggers| serde_json::from_value(triggers.clone()))
                    .transpose()
                    .map_err(|_| Reset::Malformed)?
                    .unwrap_or_default();
                self.webhooks = data
                    .get("webhooks")
                    .map(|webhooks| serde_json::from_value(webhooks.clone()))
                    .transpose()
                    .map_err(|_| Reset::Malformed)?
                    .unwrap_or_default();
                self.watchers = data
                    .get("watchers")
                    .map(|watchers| serde_json::from_value(watchers.clone()))
                    .transpose()
                    .map_err(|_| Reset::Malformed)?
                    .unwrap_or_default();
                self.draft_grants = data
                    .get("draft_grants")
                    .map(|grants| serde_json::from_value(grants.clone()))
                    .transpose()
                    .map_err(|_| Reset::Malformed)?
                    .unwrap_or_default();
            }
            other => return Err(Reset::UnknownKind(other.to_owned())),
        }
        self.applied = seq;
        Ok(Applied::Changed)
    }

    /// Merge an older page. It never replaces newer loaded text, the
    /// current routines, or the live cursor.
    pub fn merge_older(&mut self, page: Snapshot) {
        for item in page.items {
            if !self.seq_of.contains_key(&item.id) {
                self.upsert_item(item);
            }
        }
        for turn in page.turns {
            self.turns.entry(turn.id).or_insert(turn);
        }
        self.older_before_seq = page.older_before_seq;
    }

    /// Merge a window loaded strictly between the loaded items `below` and
    /// `above`, recording what is still missing on each side. Returns false
    /// and changes nothing when that region no longer matches what is
    /// loaded, because another load or a snapshot landed first.
    pub fn merge_window(&mut self, window: Window, below: Option<i64>, above: Option<i64>) -> bool {
        let still_loaded = |seq: Option<i64>| seq.is_none_or(|seq| self.items.contains_key(&seq));
        let low = below.map_or(i64::MIN, |seq| seq.saturating_add(1));
        let high = above.unwrap_or(i64::MAX);
        if !still_loaded(below)
            || !still_loaded(above)
            || self.items.range(low..high).next().is_some()
        {
            return false;
        }
        if let Some(above) = above {
            self.gaps.remove(&above);
        }
        let Some(first) = window.items.first().map(|item| item.seq) else {
            if below.is_none() {
                self.older_before_seq = None;
            }
            return true;
        };
        if let Some(above) = above.filter(|_| window.newer_gap > 0) {
            self.gaps.insert(above, GapSize::Counted(window.newer_gap));
        }
        match below {
            Some(_) if window.older_gap > 0 => {
                self.gaps.insert(first, GapSize::Counted(window.older_gap));
            }
            Some(_) => {}
            None => self.older_before_seq = (window.older_gap > 0).then_some(first),
        }
        for item in window.items {
            if !self.seq_of.contains_key(&item.id) {
                self.upsert_item(item);
            }
        }
        for turn in window.turns {
            self.turns.entry(turn.id).or_insert(turn);
        }
        true
    }

    /// Insert an item not loaded yet, keeping both sides of an unloaded
    /// stretch it lands in loadable.
    fn insert_unseen(&mut self, item: Item) {
        let seq = item.seq;
        self.upsert_item(item);
        let (below, Some(above)) = self.loaded_around(seq) else {
            return;
        };
        let unloaded = match below {
            Some(_) => self.gaps.remove(&above).is_some(),
            None => self.older_before_seq.is_some(),
        };
        if !unloaded {
            return;
        }
        if above > seq + 1 {
            self.gaps.insert(above, GapSize::Uncounted);
        }
        match below {
            Some(below) if below < seq - 1 => {
                self.gaps.insert(seq, GapSize::Uncounted);
            }
            Some(_) => {}
            None => self.older_before_seq = Some(seq),
        }
    }

    /// How many shown items are missing just below the loaded item `seq`.
    pub fn gap_below(&self, seq: i64) -> Option<GapSize> {
        self.gaps.get(&seq).copied()
    }

    /// The loaded items just below and just above `seq`.
    pub fn loaded_around(&self, seq: i64) -> (Option<i64>, Option<i64>) {
        (
            self.items.range(..seq).next_back().map(|(seq, _)| *seq),
            self.items.range(seq + 1..).next().map(|(seq, _)| *seq),
        )
    }

    /// Show the canonical owner item from a send response before its event
    /// arrives. Idempotent: the later upsert replaces it by ID.
    pub fn insert_item(&mut self, item: Item) {
        if !self.seq_of.contains_key(&item.id) {
            self.insert_unseen(item);
        }
    }

    /// Record a turn from a send response; a later event replaces it.
    pub fn insert_turn(&mut self, turn: Turn) {
        self.turns.entry(turn.id).or_insert(turn);
    }

    pub fn items(&self) -> impl DoubleEndedIterator<Item = &Item> {
        self.items.values()
    }

    /// The newest reply; a run with no news is silent, so never the latest.
    pub fn latest_reply(&self) -> Option<&Item> {
        self.items
            .values()
            .rev()
            .find(|item| item.kind == ItemKind::Assistant && !self.no_news(item.turn_id))
    }

    pub fn item(&self, id: Uuid) -> Option<&Item> {
        self.seq_of.get(&id).and_then(|seq| self.items.get(seq))
    }

    pub fn turn(&self, id: Uuid) -> Option<&Turn> {
        self.turns.get(&id)
    }

    /// Whether turn `id` is a routine run that completed with no news.
    pub fn no_news(&self, id: Uuid) -> bool {
        self.turn(id)
            .and_then(|turn| turn.routine.as_ref())
            .is_some_and(|run| run.no_news)
    }

    pub fn latest_unshown_turn(&self) -> Option<&Turn> {
        self.turns
            .values()
            .max_by_key(|turn| turn.turn_no)
            .filter(|turn| {
                (self.reaction_in(turn.id).is_some() && turn.status.is_live())
                    || !self.items().any(|item| item.turn_id == turn.id)
            })
    }

    pub fn turns_in_order(&self) -> Vec<&Turn> {
        let mut turns: Vec<_> = self.turns.values().collect();
        turns.sort_by_key(|turn| turn.turn_no);
        turns
    }

    /// Every secret request of the loaded turns, by turn number.
    pub fn secret_requests(&self) -> Vec<(&Turn, &SecretRequest)> {
        self.turns_in_order()
            .into_iter()
            .flat_map(|t| t.secret_requests.iter().map(move |r| (t, r)))
            .collect()
    }

    pub fn skill_install(&self, id: Uuid) -> Option<&SkillInstall> {
        self.turns
            .values()
            .flat_map(|t| &t.skill_installs)
            .find(|r| r.id == id)
    }

    pub fn apply_skill_install(&mut self, install: SkillInstall) {
        for turn in self.turns.values_mut() {
            if let Some(current) = turn.skill_installs.iter_mut().find(|r| r.id == install.id) {
                if current.status == SkillInstallStatus::Pending {
                    *current = install;
                }
                return;
            }
        }
    }

    pub fn owner_notice(&self, id: Uuid) -> Option<&OwnerNotice> {
        self.turns
            .values()
            .flat_map(|t| &t.owner_notices)
            .find(|n| n.id == id)
    }

    pub fn apply_owner_notice(&mut self, notice: OwnerNotice) {
        if let Some(turn) = self.turns.get_mut(&notice.turn_id)
            && let Some(current) = turn.owner_notices.iter_mut().find(|n| n.id == notice.id)
            && current.status == OwnerNoticeStatus::Pending
        {
            *current = notice;
        }
    }

    pub fn computer_handoff(&self, id: Uuid) -> Option<&ComputerHandoff> {
        self.turns
            .values()
            .flat_map(|t| &t.computer_handoffs)
            .find(|h| h.id == id)
    }

    /// The bot's open request to take over its computer (at most one):
    /// waiting on the owner, or taken and not yet handed back.
    pub fn open_handoff(&self) -> Option<&ComputerHandoff> {
        self.turns
            .values()
            .flat_map(|t| &t.computer_handoffs)
            .filter(|h| h.status.is_open())
            .max_by_key(|h| h.created_at)
    }

    pub fn secret_request(&self, id: Uuid) -> Option<&SecretRequest> {
        self.turns
            .values()
            .flat_map(|t| &t.secret_requests)
            .find(|r| r.id == id)
    }

    pub fn questions(&self) -> impl Iterator<Item = &Question> {
        self.turns.values().flat_map(|t| &t.questions)
    }

    pub fn question(&self, id: Uuid) -> Option<&Question> {
        self.turns
            .values()
            .flat_map(|t| &t.questions)
            .find(|q| q.id == id)
    }

    /// The tasks sent in the same provider attempt as delegation `id`.
    pub fn handoff(&self, id: Uuid) -> Option<(&Turn, Vec<&Delegation>)> {
        let turn = self
            .turns
            .values()
            .find(|t| t.delegations.iter().any(|d| d.id == id))?;
        let first = turn.delegations.iter().find(|d| d.id == id)?;
        let group = turn
            .delegations
            .iter()
            .filter(|d| d.shares_handoff(first))
            .collect();
        Some((turn, group))
    }

    /// What this conversation waits on, oldest first: the jobs of its
    /// unfinished turns, and each single task with no outcome yet or whose
    /// outcome no attempt has read. A council keeps its card in the
    /// transcript.
    pub fn waiting_on(&self) -> Vec<Wait<'_>> {
        let mut waits: Vec<Wait<'_>> = self
            .turns
            .values()
            .filter(|turn| turn.status.is_live())
            .flat_map(|turn| {
                let tasks = turn.delegations.iter().filter(|d| {
                    (!d.status.settled() || !d.seen)
                        && turn
                            .delegations
                            .iter()
                            .filter(|other| other.shares_handoff(d))
                            .count()
                            == 1
                });
                turn.mcp_waits
                    .iter()
                    .map(Wait::Mcp)
                    .chain(turn.computer_job_waits.iter().map(Wait::Job))
                    .chain(tasks.map(Wait::Task))
            })
            .collect();
        waits.sort_by_key(|wait| (wait.created_at(), wait.id()));
        waits
    }

    pub fn social_setup(&self, id: Uuid) -> Option<&crate::api::socials::SocialSetup> {
        self.turns
            .values()
            .flat_map(|t| &t.social_setups)
            .find(|s| s.id == id)
    }

    pub fn outgoing_draft(&self, id: Uuid) -> Option<&OutgoingDraft> {
        self.turns
            .values()
            .flat_map(|t| &t.outgoing_drafts)
            .find(|d| d.id == id)
    }

    pub fn open_draft(&self) -> Option<&OutgoingDraft> {
        self.turns_in_order()
            .into_iter()
            .flat_map(|t| &t.outgoing_drafts)
            .rfind(|d| d.status.can_follow_up())
    }

    /// The owner approves `draft`'s kind always.
    pub fn approves_always(&self, draft: &OutgoingDraft) -> bool {
        draft
            .kind
            .as_ref()
            .is_some_and(|kind| self.draft_grants.contains(kind))
    }

    pub fn update_draft(&mut self, draft: OutgoingDraft) {
        for turn in self.turns.values_mut() {
            if let Some(current) = turn.outgoing_drafts.iter_mut().find(|d| d.id == draft.id) {
                if current.status == DraftStatus::Pending && current.updated_at <= draft.updated_at
                {
                    *current = draft;
                }
                return;
            }
        }
    }

    /// The owner message that skipped `question`. The server saves the
    /// message and skips pending questions in one transaction, so it is the
    /// owner message after the question's turn stamped closest to the moment
    /// the question settled.
    pub fn skipping_reply(&self, question: &Question) -> Option<&Item> {
        let settled = question
            .settled_at
            .filter(|_| question.status == QuestionStatus::Skipped)?;
        let turn = self
            .turns
            .values()
            .find(|t| t.questions.iter().any(|q| q.id == question.id))?;
        let after = self
            .items
            .values()
            .filter(|item| item.turn_id == turn.id)
            .map(|item| item.seq)
            .max()?;
        self.items
            .range(after + 1..)
            .map(|(_, item)| item)
            .filter(|item| item.kind == ItemKind::User)
            .filter_map(|item| Some((item, (item.created_at? - settled).abs())))
            .filter(|(_, gap)| *gap <= chrono::TimeDelta::seconds(2))
            .min_by_key(|(_, gap)| *gap)
            .map(|(item, _)| item)
    }

    /// The turn's last loaded item from before the card settled. A settled
    /// card with none (a mail event's source item never loads) follows the
    /// last earlier item of any turn. It has no anchor when that item may be
    /// on an unloaded older page.
    pub fn question_anchor(&self, turn: Uuid, settled_at: Option<DateTime<Utc>>) -> Option<Uuid> {
        if let Some(item) = self.items.values().rev().find(|item| {
            item.turn_id == turn
                && settled_at.is_none_or(|at| item.created_at.is_none_or(|created| created <= at))
        }) {
            return Some(item.id);
        }
        let at = settled_at?;
        self.items
            .values()
            .rev()
            .find(|item| item.created_at.is_some_and(|created| created <= at))
            .or_else(|| {
                self.older_before_seq
                    .is_none()
                    .then(|| self.items.values().next())
                    .flatten()
            })
            .map(|item| item.id)
    }

    pub fn social_triggers(&self) -> &[Trigger] {
        &self.social_triggers
    }

    pub fn mail_triggers(&self) -> &[MailTrigger] {
        &self.mail_triggers
    }

    pub fn webhooks(&self) -> &[Webhook] {
        &self.webhooks
    }

    pub fn watchers(&self) -> &[Watcher] {
        &self.watchers
    }

    pub fn routines(&self) -> &[Routine] {
        &self.routines
    }

    pub fn rules_version(&self) -> i64 {
        self.rules_version
    }

    /// A routine from a change response. Only a newer version replaces the
    /// one the stream delivered, so a late response never undoes an event.
    pub fn apply_routine(&mut self, routine: Routine) {
        if let Some(current) = self.routines.iter_mut().find(|r| r.id == routine.id)
            && routine.updated_at > current.updated_at
        {
            *current = routine;
        }
    }

    /// The owner's latest reaction to `item_id`. Items are ordered by
    /// sequence, so the result does not depend on how pages were merged.
    pub fn reaction_for(&self, item_id: Uuid) -> Option<ReactionKind> {
        self.items
            .values()
            .rev()
            .find_map(|item| item.reaction.filter(|r| r.item_id == item_id))
            .map(|r| r.kind)
    }

    pub fn reaction_in(&self, turn_id: Uuid) -> Option<ReactionKind> {
        self.turn(turn_id)
            .and_then(|turn| turn.reaction.map(|reaction| reaction.kind))
            .or_else(|| {
                self.items()
                    .filter(|item| item.turn_id == turn_id)
                    .find_map(|item| item.reaction.map(|reaction| reaction.kind))
            })
    }

    /// Turns that ran a task another bot sent. The sender's chat shows their
    /// replies, so this chat keeps only the exchange row.
    pub fn delegated_turns(&self) -> std::collections::HashSet<Uuid> {
        self.items()
            .filter(|item| item.bot_task.is_some())
            .map(|item| item.turn_id)
            .collect()
    }

    /// This bot's replies to the task whose receipt is `task`: each one
    /// carrying a file, then the latest, in order.
    pub fn task_answers(&self, task: &Item) -> Vec<&Item> {
        if task.bot_task.is_none() {
            return Vec::new();
        }
        let replies: Vec<_> = self
            .items()
            .filter(|item| item.turn_id == task.turn_id && item.kind == ItemKind::Assistant)
            .collect();
        let last = replies.len().saturating_sub(1);
        replies
            .into_iter()
            .enumerate()
            .filter(|(ix, item)| {
                *ix == last
                    || item
                        .parts
                        .iter()
                        .any(|part| matches!(part, crate::attachments::MessagePart::File { .. }))
            })
            .map(|(_, item)| item)
            .collect()
    }

    /// The receipt of the task another bot sent, for a turn that ran it.
    pub fn task_receipt(&self, turn: Uuid) -> Option<&Item> {
        self.items()
            .find(|item| item.turn_id == turn && item.bot_task.is_some())
    }

    pub fn is_feedback_acknowledgement(&self, item: &Item) -> bool {
        item.kind == ItemKind::Assistant
            && self.reaction_in(item.turn_id).is_some()
            && item
                .text
                .trim()
                .trim_end_matches('.')
                .eq_ignore_ascii_case("Thanks, noted")
            && !item
                .parts
                .iter()
                .any(|part| matches!(part, crate::attachments::MessagePart::File { .. }))
    }

    pub fn has_request(&self, request_id: Uuid) -> bool {
        self.items
            .values()
            .any(|item| item.request_id == Some(request_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PREFIX: &str =
        "11111111-1111-1111-1111-111111111111:22222222-2222-2222-2222-222222222222";

    fn bot() -> Uuid {
        "22222222-2222-2222-2222-222222222222".parse().unwrap()
    }

    fn item(id: u128, seq: i64, kind: &str, text: &str) -> serde_json::Value {
        json!({
            "id": Uuid::from_u128(id), "seq": seq, "turn_id": Uuid::from_u128(900 + id),
            "request_id": if kind == "user" { json!(Uuid::from_u128(500 + id)) } else { json!(null) },
            "kind": kind, "text": text, "provider_id": null, "created_at": "2026-11-02T00:00:00Z",
        })
    }

    fn snapshot_json(
        seq: i64,
        items: Vec<serde_json::Value>,
        older: Option<i64>,
    ) -> serde_json::Value {
        json!({
            "conversation_id": Uuid::new_v4(), "bot_id": bot(), "selected_provider_id": null,
            "event_cursor": format!("{PREFIX}:{seq}"), "items": items, "turns": [],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 },
            "older_before_seq": older,
        })
    }

    fn snapshot(seq: i64, items: Vec<serde_json::Value>, older: Option<i64>) -> Snapshot {
        serde_json::from_value(snapshot_json(seq, items, older)).unwrap()
    }

    fn frame(seq: i64, event: &str, data: serde_json::Value) -> Frame {
        Frame {
            id: Some(format!("{PREFIX}:{seq}")),
            event: event.into(),
            data: data.to_string(),
        }
    }

    fn delta(seq: i64, id: u128, offset: usize, text: &str) -> Frame {
        frame(
            seq,
            "item.delta",
            json!({ "item_id": Uuid::from_u128(id), "offset_utf8": offset, "text": text }),
        )
    }

    fn window(items: Vec<serde_json::Value>, older_gap: i64, newer_gap: i64) -> Window {
        serde_json::from_value(json!({
            "items": items, "turns": [], "older_gap": older_gap, "newer_gap": newer_gap,
        }))
        .unwrap()
    }

    fn loaded(c: &Conversation) -> Vec<i64> {
        c.items().map(|item| item.seq).collect()
    }

    #[test]
    fn a_window_below_the_tail_leaves_gaps_that_close_when_loaded() {
        let tail = vec![item(90, 90, "user", "latest")];
        let mut c = Conversation::from_snapshot(snapshot(5, tail, Some(90))).unwrap();
        let found = vec![item(40, 40, "user", "a"), item(41, 41, "assistant", "b")];
        assert!(c.merge_window(window(found, 12, 30), None, Some(90)));
        assert_eq!(loaded(&c), [40, 41, 90]);
        assert_eq!(c.older_before_seq, Some(40));
        assert_eq!(c.gap_below(90), Some(GapSize::Counted(30)));
        assert_eq!(c.cursor(), format!("{PREFIX}:5"), "the live cursor stays");

        let between = vec![item(60, 60, "user", "c"), item(70, 70, "user", "d")];
        assert!(c.merge_window(window(between, 3, 0), Some(41), Some(90)));
        assert_eq!(loaded(&c), [40, 41, 60, 70, 90]);
        assert_eq!(c.gap_below(90), None);
        assert_eq!(c.gap_below(60), Some(GapSize::Counted(3)));

        let rest = vec![item(50, 50, "user", "e")];
        assert!(c.merge_window(window(rest, 0, 0), Some(41), Some(60)));
        assert_eq!(c.gap_below(60), None);

        let oldest = vec![item(1, 1, "user", "first")];
        assert!(c.merge_window(window(oldest, 0, 0), None, Some(40)));
        assert_eq!(c.older_before_seq, None);
        assert_eq!(c.loaded_around(45), (Some(41), Some(50)));
    }

    #[test]
    fn an_empty_window_closes_the_gap_it_was_loaded_for() {
        let tail = vec![item(90, 90, "user", "latest")];
        let mut c = Conversation::from_snapshot(snapshot(5, tail, Some(90))).unwrap();
        let found = vec![item(40, 40, "user", "a")];
        assert!(c.merge_window(window(found, 5, 30), None, Some(90)));
        assert!(c.merge_window(window(vec![], 0, 0), Some(40), Some(90)));
        assert_eq!(c.gap_below(90), None);
        assert!(c.merge_window(window(vec![], 0, 0), None, Some(40)));
        assert_eq!(c.older_before_seq, None);
    }

    #[test]
    fn a_window_for_a_region_that_changed_is_dropped() {
        let tail = vec![item(90, 90, "user", "latest")];
        let mut c = Conversation::from_snapshot(snapshot(5, tail, Some(90))).unwrap();
        let found = vec![item(40, 40, "user", "a")];
        assert!(c.merge_window(window(found, 5, 30), None, Some(90)));
        let between = vec![item(60, 60, "user", "b")];
        assert!(c.merge_window(window(between, 10, 19), Some(40), Some(90)));

        let stale = vec![item(50, 50, "user", "c")];
        assert!(!c.merge_window(window(stale, 0, 0), Some(40), Some(90)));
        assert_eq!(loaded(&c), [40, 60, 90]);
        assert_eq!(c.gap_below(90), Some(GapSize::Counted(19)));
        assert_eq!(c.gap_below(60), Some(GapSize::Counted(10)));
        let gone = vec![item(85, 85, "user", "d")];
        assert!(!c.merge_window(window(gone, 0, 0), Some(80), Some(90)));
    }

    #[test]
    fn a_live_edit_inside_a_gap_keeps_both_sides_loadable() {
        let tail = vec![item(90, 90, "user", "latest")];
        let mut c = Conversation::from_snapshot(snapshot(5, tail, Some(90))).unwrap();
        let found = vec![item(20, 20, "user", "a")];
        assert!(c.merge_window(window(found, 0, 69), None, Some(90)));

        let edit = frame(
            6,
            "item.upsert",
            json!({ "item": item(50, 50, "user", "undone") }),
        );
        assert_eq!(c.apply(&edit), Ok(Applied::Changed));
        assert_eq!(loaded(&c), [20, 50, 90]);
        assert_eq!(c.gap_below(90), Some(GapSize::Uncounted));
        assert_eq!(c.gap_below(50), Some(GapSize::Uncounted));
        let read_before_the_edit = vec![item(50, 50, "user", "old")];
        assert!(!c.merge_window(window(read_before_the_edit, 0, 0), Some(20), Some(90)));
        assert_eq!(c.item(Uuid::from_u128(50)).unwrap().text, "undone");

        let upper = vec![item(51, 51, "user", "b"), item(89, 89, "user", "c")];
        assert!(c.merge_window(window(upper, 0, 0), Some(50), Some(90)));
        assert_eq!(c.gap_below(90), None);
        assert_eq!(c.gap_below(50), Some(GapSize::Uncounted));
        let lower = vec![item(21, 21, "user", "d"), item(49, 49, "user", "e")];
        assert!(c.merge_window(window(lower, 0, 0), Some(20), Some(50)));
        assert_eq!(c.gap_below(50), None);
        assert_eq!(loaded(&c), [20, 21, 49, 50, 51, 89, 90]);
    }

    #[test]
    fn a_live_item_next_to_a_loaded_row_leaves_no_empty_gap() {
        let tail = vec![item(90, 90, "user", "latest")];
        let mut c = Conversation::from_snapshot(snapshot(5, tail, Some(90))).unwrap();
        let found = vec![item(20, 20, "user", "a")];
        assert!(c.merge_window(window(found, 0, 69), None, Some(90)));

        let reply = item(21, 21, "assistant", "");
        assert_eq!(
            c.apply(&frame(6, "item.upsert", json!({ "item": reply }))),
            Ok(Applied::Changed)
        );
        assert_eq!(c.apply(&delta(7, 21, 0, "hi")), Ok(Applied::Changed));
        assert_eq!(c.gap_below(21), None);
        assert_eq!(c.gap_below(90), Some(GapSize::Uncounted));

        let edit = item(89, 89, "user", "top");
        assert_eq!(
            c.apply(&frame(8, "item.upsert", json!({ "item": edit }))),
            Ok(Applied::Changed)
        );
        assert_eq!(c.gap_below(90), None);
        assert_eq!(c.gap_below(89), Some(GapSize::Uncounted));
    }

    #[test]
    fn a_late_send_response_inside_a_gap_keeps_both_sides_loadable() {
        let tail = vec![item(90, 90, "user", "latest")];
        let mut c = Conversation::from_snapshot(snapshot(5, tail, Some(90))).unwrap();
        let found = vec![item(20, 20, "user", "a")];
        assert!(c.merge_window(window(found, 0, 69), None, Some(90)));
        let sent: Item = serde_json::from_value(item(50, 50, "user", "sent")).unwrap();
        c.insert_item(sent);
        assert_eq!(c.gap_below(90), Some(GapSize::Uncounted));
        assert_eq!(c.gap_below(50), Some(GapSize::Uncounted));
    }

    #[test]
    fn a_live_edit_below_the_oldest_row_keeps_older_rows_loadable() {
        let tail = vec![item(90, 90, "user", "latest")];
        let mut c = Conversation::from_snapshot(snapshot(5, tail, Some(90))).unwrap();
        let edit = frame(
            6,
            "item.upsert",
            json!({ "item": item(50, 50, "user", "undone") }),
        );
        assert_eq!(c.apply(&edit), Ok(Applied::Changed));
        assert_eq!(c.older_before_seq, Some(50));
        assert_eq!(c.gap_below(90), Some(GapSize::Uncounted));

        let found = vec![item(20, 20, "user", "a")];
        assert!(c.merge_window(window(found, 4, 29), None, Some(50)));
        assert_eq!(c.gap_below(90), Some(GapSize::Uncounted));
        assert_eq!(c.gap_below(50), Some(GapSize::Counted(29)));
        assert_eq!(c.older_before_seq, Some(20));
    }

    #[test]
    fn owner_notices_follow_turn_events_and_reload_from_snapshots() {
        let mut c = Conversation::from_snapshot(snapshot(0, vec![], None)).unwrap();
        let mut turn = json!({
            "id": Uuid::from_u128(900), "request_id": Uuid::from_u128(901), "turn_no": 1,
            "provider_id": "codex", "status": "completed", "effect_uncertain": false,
            "owner_notices": [{
                "id": Uuid::from_u128(400), "bot_id": bot(), "turn_id": Uuid::from_u128(900),
                "kind": "decision", "summary": "Choose a bot", "tried": null,
                "status": "pending", "created_at": "2026-11-01T00:00:00Z", "settled_at": null
            }]
        });
        for (seq, status) in [(1, "pending"), (2, "replied")] {
            turn["owner_notices"][0]["status"] = json!(status);
            if status == "replied" {
                turn["owner_notices"][0]["settled_at"] = json!("2026-11-01T00:01:00Z");
            }
            c.apply(&frame(seq, "turn.updated", json!({"turn": turn})))
                .unwrap();
            let mut snap = snapshot(seq, vec![], None);
            snap.turns = serde_json::from_value(json!([turn])).unwrap();
            let reloaded = Conversation::from_snapshot(snap).unwrap();
            assert_eq!(
                reloaded.owner_notice(Uuid::from_u128(400)),
                c.owner_notice(Uuid::from_u128(400))
            );
        }
        assert_eq!(
            c.owner_notice(Uuid::from_u128(400)).unwrap().status,
            OwnerNoticeStatus::Replied
        );
    }

    #[test]
    fn a_skipped_question_finds_the_owner_message_that_skipped_it() {
        let stamped = |id, seq, kind, text, at: &str| {
            let mut item = item(id, seq, kind, text);
            item["turn_id"] = json!(Uuid::from_u128(9));
            item["created_at"] = json!(at);
            item
        };
        let mut answer = item(3, 3, "user", "Markdown, for Notion");
        answer["created_at"] = json!("2026-11-02T00:00:01.500Z");
        let mut c = Conversation::from_snapshot(snapshot(
            3,
            vec![
                stamped(1, 1, "user", "make a digest", "2026-11-02T00:00:00.000Z"),
                stamped(
                    2,
                    2,
                    "assistant",
                    "Which format?",
                    "2026-11-02T00:00:00.500Z",
                ),
                answer,
            ],
            None,
        ))
        .unwrap();
        let question = Uuid::from_u128(40);
        let turn = |status: &str| {
            json!({ "turn": {
                "id": Uuid::from_u128(9), "request_id": Uuid::from_u128(8), "turn_no": 1,
                "provider_id": "synthetic", "status": "completed", "error": null,
                "effect_uncertain": false,
                "questions": [{ "id": question, "kind": "choice", "title": "Which format?",
                    "choices": [], "status": status,
                    "settled_at": "2026-11-02T00:00:01.510Z" }],
            }})
        };
        c.apply(&frame(4, "turn.updated", turn("skipped"))).unwrap();
        let reply = c.skipping_reply(c.question(question).unwrap());
        assert_eq!(reply.map(|i| i.text.as_str()), Some("Markdown, for Notion"));
        c.apply(&frame(5, "turn.updated", turn("cancelled")))
            .unwrap();
        assert!(c.skipping_reply(c.question(question).unwrap()).is_none());
    }

    #[test]
    fn states_a_newer_server_adds_still_load() {
        let mut page = snapshot(1, vec![item(1, 1, "summary", "Digest")], None);
        page.turns = vec![
            serde_json::from_value(json!({
                "id": Uuid::from_u128(901), "request_id": Uuid::from_u128(501), "turn_no": 1,
                "provider_id": "codex", "status": "rewinding", "error": null,
                "effect_uncertain": false,
                "computer_job_waits": [{ "id": Uuid::from_u128(902), "job_id": Uuid::from_u128(903) }]
            }))
            .unwrap(),
        ];
        let c = Conversation::from_snapshot(page).unwrap();
        assert_eq!(c.item(Uuid::from_u128(1)).unwrap().kind, ItemKind::Unknown);
        assert!(c.latest_reply().is_none(), "an unknown item is not a reply");
        assert_eq!(
            c.turn(Uuid::from_u128(901)).unwrap().status,
            TurnStatus::Unknown
        );
        assert!(c.waiting_on().is_empty(), "an unknown turn is not live");
    }

    #[test]
    fn a_state_that_is_not_a_string_is_malformed() {
        let mut c = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        let turn = json!({"id": Uuid::from_u128(901), "request_id": Uuid::from_u128(501), "turn_no": 1,
            "provider_id": "codex", "status": {"running": null}, "error": null, "effect_uncertain": false});
        assert!(matches!(
            c.apply(&frame(2, "turn.updated", json!({"turn": turn}))),
            Err(Reset::Malformed)
        ));
        assert!(c.turn(Uuid::from_u128(901)).is_none());
    }

    #[test]
    fn hidden_turn_status_is_available_until_its_first_visible_item() {
        let mut c = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        assert!(c.latest_unshown_turn().is_none());
        for (seq, status, expected) in [
            (2, "running", TurnStatus::Running),
            (3, "failed", TurnStatus::Failed),
            (4, "interrupted", TurnStatus::Interrupted),
        ] {
            let turn = json!({"id": Uuid::from_u128(901), "request_id": Uuid::from_u128(501), "turn_no": 1,
                "provider_id": "codex", "status": status, "error": null, "effect_uncertain": false});
            c.apply(&frame(seq, "turn.updated", json!({"turn":turn})))
                .unwrap();
            assert_eq!(c.latest_unshown_turn().unwrap().status, expected);
        }
        c.apply(&frame(
            5,
            "item.upsert",
            json!({"item":item(1, 1, "assistant", "What is this inbox for?")}),
        ))
        .unwrap();
        assert!(c.latest_unshown_turn().is_none());
    }

    #[test]
    fn feedback_acknowledgements_are_hidden_but_useful_replies_and_files_stay_visible() {
        let mut feedback = reaction(2, 2, 1, "thumbs_up");
        let mut reply = item(3, 3, "assistant", "  Thanks, noted.\n");
        reply["turn_id"] = feedback["turn_id"].clone();
        let normal = item(4, 4, "assistant", "Thanks, noted.");
        let c = Conversation::from_snapshot(snapshot(
            1,
            vec![
                item(1, 1, "assistant", "Done"),
                feedback.clone(),
                reply.clone(),
                normal,
            ],
            None,
        ))
        .unwrap();
        assert!(c.is_feedback_acknowledgement(c.item(Uuid::from_u128(3)).unwrap()));
        assert!(!c.is_feedback_acknowledgement(c.item(Uuid::from_u128(4)).unwrap()));
        assert_eq!(c.latest_reply().unwrap().id, Uuid::from_u128(4));
        for text in [
            "What went wrong?",
            "Thanks, noted. I updated the task.",
            "Thanks, noted. Should I update the task?",
        ] {
            let mut useful: Item = serde_json::from_value(reply.clone()).unwrap();
            useful.text = text.into();
            assert!(!c.is_feedback_acknowledgement(&useful));
        }
        let mut file: Item = serde_json::from_value(reply.clone()).unwrap();
        file.parts.push(crate::attachments::MessagePart::File {
            file: crate::attachments::Attachment {
                id: Uuid::new_v4(),
                name: "report.txt".into(),
                media_type: "text/plain".into(),
                size: 12,
            },
        });
        assert!(!c.is_feedback_acknowledgement(&file));
        feedback["reaction"]["kind"] = json!("thumbs_down");
        let c = Conversation::from_snapshot(snapshot(1, vec![feedback, reply], None)).unwrap();
        assert!(c.is_feedback_acknowledgement(c.latest_reply().unwrap()));
        assert_eq!(
            c.latest_reply().unwrap().seq,
            3,
            "hidden text still has a read position"
        );
    }

    #[test]
    fn feedback_keeps_its_working_status_available() {
        let feedback = reaction(2, 2, 1, "thumbs_up");
        let turn_id = feedback["turn_id"].clone();
        let mut page = snapshot(1, vec![feedback], None);
        for (status, expected) in [
            ("queued", TurnStatus::Queued),
            ("running", TurnStatus::Running),
            ("waiting", TurnStatus::Waiting),
            ("cancelling", TurnStatus::Cancelling),
        ] {
            page.turns = vec![
                serde_json::from_value(json!({
                    "id": turn_id, "request_id": Uuid::new_v4(), "turn_no": 2,
                    "provider_id": "synthetic", "status": status, "error": null,
                    "effect_uncertain": false
                }))
                .unwrap(),
            ];
            let c = Conversation::from_snapshot(page.clone()).unwrap();
            assert_eq!(c.latest_unshown_turn().unwrap().status, expected);
        }
    }

    #[test]
    fn child_replies_keep_their_attribution_across_replay_and_reload() {
        let own_reply = item(1, 1, "assistant", "I'll ask Marketing.");
        let mut child_reply = item(2, 2, "status", "**Conversions:** 120");
        child_reply["bot_reply"] = json!({
            "sender_bot_id": Uuid::from_u128(10), "sender_name": "Marketing",
            "prompt": "Report this week's KPIs.", "status": "completed",
        });
        let mut live =
            Conversation::from_snapshot(snapshot(1, vec![own_reply.clone()], None)).unwrap();
        let event = frame(2, "item.upsert", json!({ "item": child_reply.clone() }));
        assert_eq!(live.apply(&event), Ok(Applied::Changed));
        assert_eq!(live.apply(&event), Ok(Applied::Duplicate));
        let reloaded =
            Conversation::from_snapshot(snapshot(2, vec![own_reply, child_reply], None)).unwrap();
        let shown = live.item(Uuid::from_u128(2)).unwrap();
        assert_eq!(Some(shown), reloaded.item(shown.id));
        let reply = shown.bot_reply.as_ref().unwrap();
        assert_eq!(reply.sender_name, "Marketing");
        assert_eq!(reply.prompt, "Report this week's KPIs.");
        assert_eq!(reply.status, BotReplyStatus::Completed);
        assert_eq!(live.items().count(), 2);
        assert_eq!(live.latest_reply().unwrap().id, Uuid::from_u128(1));
    }

    #[test]
    fn routine_update_notices_survive_live_replays_reload_and_paging() {
        let mut notice = item(2, 2, "status", "Updated routine: Daily digest");
        notice["routine_update"] = json!({
            "routine_id": Uuid::from_u128(71), "name": "Daily digest"
        });
        let initial = snapshot(1, vec![notice.clone()], None);
        let reloaded = Conversation::from_snapshot(initial.clone()).unwrap();
        let mut live = Conversation::from_snapshot(snapshot(0, vec![], None)).unwrap();
        let event = frame(1, "item.upsert", json!({ "item": notice }));
        live.apply(&event).unwrap();
        assert_eq!(live.apply(&event), Ok(Applied::Duplicate));
        assert_eq!(live.items().count(), 1);
        let mut paged = Conversation::from_snapshot(snapshot(2, vec![], Some(3))).unwrap();
        paged.merge_older(initial);
        let expected = reloaded.items().next().unwrap();
        assert_eq!(
            expected.routine_update.as_ref().unwrap().name,
            "Daily digest"
        );
        assert_eq!(live.items().next(), Some(expected));
        assert_eq!(paged.items().next(), Some(expected));
    }

    #[test]
    fn routine_provenance_survives_events_reload_and_history_paging() {
        let id = Uuid::from_u128(901);
        let turn = json!({
            "id": id, "request_id": Uuid::from_u128(501), "turn_no": 1,
            "provider_id": "codex", "status": "waiting", "error": null,
            "effect_uncertain": false,
            "routine": { "routine_id": Uuid::from_u128(71), "name": "Merged branch cleanup",
                         "due_at": "2026-11-05T04:00:00Z" },
        });
        let mut initial = snapshot(
            1,
            vec![item(1, 1, "assistant", "Ready for approval.")],
            None,
        );
        initial.turns = vec![serde_json::from_value(turn.clone()).unwrap()];
        let mut live = Conversation::from_snapshot(snapshot(0, vec![], None)).unwrap();
        live.apply(&frame(1, "turn.updated", json!({ "turn": turn })))
            .unwrap();
        let reloaded = Conversation::from_snapshot(initial.clone()).unwrap();
        let mut paged = Conversation::from_snapshot(snapshot(2, vec![], Some(2))).unwrap();
        paged.merge_older(initial);
        let expected = reloaded.turn(id).unwrap().routine.as_ref().unwrap();
        assert_eq!(expected.name, "Merged branch cleanup");
        assert_eq!(live.turn(id).unwrap().routine.as_ref(), Some(expected));
        assert_eq!(paged.turn(id).unwrap().routine.as_ref(), Some(expected));
    }

    #[test]
    fn a_run_is_labelled_by_whether_a_reminder_started_it() {
        let run = |extra: serde_json::Value| {
            let mut run = json!({ "routine_id": Uuid::from_u128(71), "name": "DevOps check",
                                  "due_at": "2026-11-13T13:37:00Z" });
            run.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value::<RoutineRun>(run).unwrap().label()
        };
        assert_eq!(run(json!({ "reminder": true })), "Reminder");
        assert_eq!(run(json!({ "reminder": false })), "Scheduled routine");
        assert_eq!(run(json!({})), "Scheduled routine", "an older server");
    }

    #[test]
    fn utf8_deltas_duplicates_and_cursor() {
        let mut c =
            Conversation::from_snapshot(snapshot(3, vec![item(1, 1, "user", "hi")], None)).unwrap();
        assert_eq!(c.cursor(), format!("{PREFIX}:3"));
        c.apply(&frame(
            4,
            "item.upsert",
            json!({ "item": item(2, 2, "assistant", "é") }),
        ))
        .unwrap();
        assert_eq!(c.apply(&delta(5, 2, 2, "✓日")), Ok(Applied::Changed));
        assert_eq!(c.apply(&delta(5, 2, 2, "✓日")), Ok(Applied::Duplicate));
        assert_eq!(c.apply(&delta(4, 2, 0, "x")), Ok(Applied::Duplicate));
        assert_eq!(c.item(Uuid::from_u128(2)).unwrap().text, "é✓日");
        assert_eq!(c.cursor(), format!("{PREFIX}:5"));
        assert_eq!(c.items().count(), 2);
    }

    #[test]
    fn inconsistencies_require_a_snapshot() {
        let mut c =
            Conversation::from_snapshot(snapshot(1, vec![item(2, 1, "assistant", "ab")], None))
                .unwrap();
        assert_eq!(
            c.apply(&delta(3, 2, 2, "c")),
            Err(Reset::Gap {
                expected: 2,
                got: 3
            })
        );
        assert_eq!(c.apply(&delta(2, 2, 1, "c")), Err(Reset::OffsetMismatch));
        assert_eq!(c.apply(&delta(2, 9, 0, "c")), Err(Reset::UnknownItem));
        assert_eq!(
            c.apply(&frame(2, "mystery", json!({}))),
            Err(Reset::UnknownKind("mystery".into()))
        );
        let mut other = delta(2, 2, 2, "c");
        other.id = Some(
            "33333333-3333-3333-3333-333333333333:22222222-2222-2222-2222-222222222222:2".into(),
        );
        assert_eq!(c.apply(&other), Err(Reset::BadCursor));
        assert_eq!(c.item(Uuid::from_u128(2)).unwrap().text, "ab");
        assert_eq!(c.apply(&delta(2, 2, 2, "c")), Ok(Applied::Changed));
    }

    #[test]
    fn older_pages_merge_without_replacing_newer_text() {
        let mut c = Conversation::from_snapshot(snapshot(
            9,
            vec![item(3, 3, "user", "c"), item(4, 4, "assistant", "newest")],
            Some(3),
        ))
        .unwrap();
        c.merge_older(snapshot(
            5,
            vec![
                item(1, 1, "user", "a"),
                item(2, 2, "assistant", "b"),
                item(4, 4, "assistant", "stale"),
            ],
            None,
        ));
        let texts: Vec<_> = c.items().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "b", "c", "newest"]);
        assert_eq!(c.older_before_seq, None);
        assert_eq!(c.cursor(), format!("{PREFIX}:9"), "live cursor unchanged");
        assert!(c.has_request(Uuid::from_u128(501)));
    }

    fn reaction(id: u128, seq: i64, target: u128, kind: &str) -> serde_json::Value {
        let mut item = item(
            id,
            seq,
            "user",
            "Feedback on your earlier message:\n> Done.",
        );
        item["reaction"] = json!({ "item_id": Uuid::from_u128(target), "kind": kind });
        item
    }

    #[test]
    fn the_newest_committed_reaction_marks_its_exact_target() {
        let history = || {
            vec![
                item(1, 1, "user", "Do it twice"),
                item(2, 2, "assistant", "Done."),
                reaction(3, 3, 2, "thumbs_up"),
                item(4, 4, "assistant", "Done."),
                reaction(5, 5, 2, "thumbs_down"),
                item(6, 6, "assistant", "Noted."),
            ]
        };
        let whole = Conversation::from_snapshot(snapshot(6, history(), None)).unwrap();
        let all = history();
        let (older, newer) = all.split_at(3);
        let mut paged = Conversation::from_snapshot(snapshot(6, newer.to_vec(), Some(4))).unwrap();
        assert_eq!(
            paged.reaction_for(Uuid::from_u128(2)),
            Some(ReactionKind::ThumbsDown),
            "a reaction newer than the loaded target is already loaded"
        );
        paged.merge_older(snapshot(6, older.to_vec(), None));
        for c in [&whole, &paged] {
            assert_eq!(
                c.reaction_for(Uuid::from_u128(2)),
                Some(ReactionKind::ThumbsDown)
            );
            assert_eq!(
                c.reaction_for(Uuid::from_u128(4)),
                None,
                "same text, other item"
            );
            assert_eq!(c.reaction_for(Uuid::from_u128(6)), None);
        }

        paged
            .apply(&frame(
                7,
                "item.upsert",
                json!({ "item": reaction(7, 7, 2, "thumbs_up") }),
            ))
            .unwrap();
        assert_eq!(
            paged.reaction_for(Uuid::from_u128(2)),
            Some(ReactionKind::ThumbsUp)
        );
    }

    fn routine(enabled: bool, updated: &str, status: Option<&str>) -> serde_json::Value {
        json!({
            "id": Uuid::from_u128(40), "bot_id": bot(), "name": "Morning digest",
            "prompt": "Summarize my mail.", "cron": "0 9 * * *", "timezone": "Europe/Paris",
            "enabled": enabled, "next_run_at": if enabled { json!("2026-11-05T08:00:00Z") } else { json!(null) },
            "created_at": "2026-11-01T00:00:00Z", "updated_at": updated,
            "last_run": status.map(|s| json!({ "due_at": "2026-11-04T08:00:00Z", "status": s,
                "skip_reason": null, "turn_id": Uuid::from_u128(41),
                "error": if s == "failed" { json!({ "code": "provider_refused", "message": "Refused." }) } else { json!(null) },
                "finished_at": null })),
        })
    }

    fn bot_updated(seq: i64, routines: Option<Vec<serde_json::Value>>) -> Frame {
        let mut data = json!({ "bot_id": bot(), "bot": {
            "id": bot(), "name": "Assistant",
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 } } });
        if let Some(routines) = routines {
            data["routines"] = json!(routines);
        }
        frame(seq, "bot.updated", data)
    }

    #[test]
    fn routines_come_from_the_snapshot_and_each_bot_update() {
        let mut first = snapshot(1, vec![], Some(5));
        first.routines =
            vec![serde_json::from_value(routine(true, "2026-11-01T00:00:00Z", None)).unwrap()];
        let mut c = Conversation::from_snapshot(first).unwrap();
        assert_eq!(c.routines().len(), 1);
        assert!(c.routines()[0].enabled);

        c.apply(&bot_updated(
            2,
            Some(vec![routine(true, "2026-11-01T00:00:00Z", Some("failed"))]),
        ))
        .unwrap();
        let last = c.routines()[0].last_run.as_ref().unwrap();
        assert_eq!(last.status, RunStatus::Failed);
        assert_eq!(last.error.as_ref().unwrap().code, "provider_refused");

        c.apply(&bot_updated(3, None)).unwrap();
        assert_eq!(c.routines().len(), 1);
        c.merge_older(snapshot(3, vec![], None));
        assert_eq!(c.routines().len(), 1);

        c.apply(&bot_updated(4, Some(vec![]))).unwrap();
        assert!(c.routines().is_empty(), "deleted");
        let malformed = frame(
            5,
            "bot.updated",
            json!({ "bot": { "id": bot(), "name": "A" }, "routines": [{ "id": 1 }] }),
        );
        assert_eq!(c.apply(&malformed), Err(Reset::Malformed));
    }

    #[test]
    fn the_rules_version_follows_the_snapshot_and_bot_updates() {
        let mut first = snapshot(1, vec![], None);
        first.rules_version = 2;
        let mut c = Conversation::from_snapshot(first).unwrap();
        assert_eq!(c.rules_version(), 2);
        c.apply(&bot_updated(2, None)).unwrap();
        assert_eq!(c.rules_version(), 2, "an older server sends no version");
        let update = frame(
            3,
            "bot.updated",
            json!({ "bot_id": bot(), "rules_version": 3, "bot": {
                "id": bot(), "name": "Assistant",
                "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 } } }),
        );
        c.apply(&update).unwrap();
        assert_eq!(c.rules_version(), 3);
    }

    #[test]
    fn a_rules_card_and_the_rules_file_parse_and_mark_changed_rules() {
        let update: RulesUpdate = serde_json::from_value(json!({
            "version": 2, "section": "New Linear issue", "change": "changed",
            "owner_words": "Just send fix issue ELSE-142.",
            "diff": [{ "op": "remove", "text": "Do: describe it" },
                     { "op": "add", "text": "Do: send `fix issue {ID}`" }]
        }))
        .unwrap();
        assert_eq!(update.change, RuleChange::Changed);
        assert_eq!(update.diff[1].op, DiffOp::Add);
        let file: RulesFile = serde_json::from_value(json!({
            "version": 3, "markdown": "## New Linear issue\nDo: x\n\n## Tone\nShort.\n",
            "sections": [{ "title": "New Linear issue", "body": "Do: x" },
                         { "title": "Tone", "body": "Short." }],
            "history": [
                { "version": 3, "section": "New Linear issue", "change": "changed",
                  "owner_words": "x", "author": "bot", "restored_from": null, "diff": [],
                  "created_at": "2026-11-08T10:00:00Z" },
                { "version": 2, "section": "Tone", "change": "added", "owner_words": "y",
                  "author": "bot", "restored_from": null, "diff": [],
                  "created_at": "2026-11-07T10:00:00Z" }
            ]
        }))
        .unwrap();
        let changed = file.changed_since(2);
        assert!(changed.contains("new linear issue"));
        assert!(!changed.contains("tone"));
        assert!(file.changed_since(3).is_empty());
    }

    fn social_trigger(enabled: bool, provider: &str) -> serde_json::Value {
        json!({
            "id": Uuid::from_u128(80), "connection_id": Uuid::from_u128(81),
            "bot_id": bot(), "provider": provider, "connection_name": "Team bot",
            "username": "team_bot", "destination": "-100123", "destination_name": "Proliquid",
            "wake": "mentions", "instructions": "Summarize questions.", "enabled": enabled,
            "created_at": "2026-11-01T07:00:00.123+00:00", "updated_at": "2026-11-02T07:00:00.456+00:00"
        })
    }

    #[test]
    fn social_triggers_parse_from_snapshots_and_bot_updates() {
        let first: Snapshot = serde_json::from_value(json!({
            "event_cursor": format!("{PREFIX}:1"), "items": [], "turns": [],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 },
            "social_triggers": [social_trigger(true, "telegram")]
        }))
        .unwrap();
        let mut conversation = Conversation::from_snapshot(first).unwrap();
        assert_eq!(conversation.social_triggers().len(), 1);
        let trigger = &conversation.social_triggers()[0];
        assert_eq!(trigger.id, Uuid::from_u128(80));
        assert_eq!(trigger.connection_id, Uuid::from_u128(81));
        assert_eq!(trigger.bot_id, bot());
        assert_eq!(trigger.title(), "Proliquid");
        assert_eq!(trigger.wake.label(), "on mention");
        assert_eq!(trigger.instructions, "Summarize questions.");
        assert!(trigger.enabled);
        let update = |seq, triggers| {
            frame(
                seq,
                "bot.updated",
                json!({
                    "bot": { "id": bot(), "name": "Assistant" }, "social_triggers": triggers
                }),
            )
        };
        conversation
            .apply(&update(2, json!([social_trigger(false, "discord")])))
            .unwrap();
        assert!(!conversation.social_triggers()[0].enabled);
        assert_eq!(
            conversation.social_triggers()[0].provider.label(),
            "Discord"
        );
        let mut older = snapshot(1, vec![], None);
        older.social_triggers =
            vec![serde_json::from_value(social_trigger(true, "telegram")).unwrap()];
        conversation.merge_older(older);
        assert!(
            !conversation.social_triggers()[0].enabled,
            "older pages keep the current triggers"
        );
        conversation.apply(&update(3, json!([]))).unwrap();
        assert!(
            conversation.social_triggers().is_empty(),
            "deleted triggers leave the list"
        );
        assert_eq!(
            conversation.apply(&update(4, json!([{ "id": 1 }]))),
            Err(Reset::Malformed)
        );
    }

    #[test]
    fn mail_triggers_parse_from_snapshots_and_bot_updates() {
        let inbox = |wake| {
            json!({
                "account_id": Uuid::from_u128(90), "address": "owner@example.com",
                "state": "active", "health": "connected", "wake": wake,
                "instructions": "Flag invoices.", "last_woke_at": "2026-11-02T08:00:00.000+00:00"
            })
        };
        let first: Snapshot = serde_json::from_value(json!({
            "event_cursor": format!("{PREFIX}:1"), "items": [], "turns": [],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 },
            "mail_triggers": [inbox(true)]
        }))
        .unwrap();
        let mut conversation = Conversation::from_snapshot(first).unwrap();
        let trigger = &conversation.mail_triggers()[0];
        assert_eq!(trigger.account_id, Uuid::from_u128(90));
        assert_eq!(trigger.address, "owner@example.com");
        assert!(trigger.wake);
        assert_eq!(trigger.instructions.as_deref(), Some("Flag invoices."));
        assert!(trigger.last_woke_at.is_some());
        let update = |seq, triggers| {
            frame(
                seq,
                "bot.updated",
                json!({ "bot": { "id": bot(), "name": "Assistant" }, "mail_triggers": triggers }),
            )
        };
        conversation
            .apply(&update(2, json!([inbox(false)])))
            .unwrap();
        assert!(!conversation.mail_triggers()[0].wake);
        conversation.apply(&update(3, json!([]))).unwrap();
        assert!(
            conversation.mail_triggers().is_empty(),
            "removed inboxes leave the list"
        );
    }

    #[test]
    fn webhooks_parse_from_snapshots_and_bot_updates() {
        let webhook = |enabled| {
            json!({
                "id": Uuid::from_u128(91), "bot_id": bot(), "name": "Deploys",
                "prompt": "Open an issue when a deploy fails.", "min_interval_minutes": 15,
                "daily_limit": 24, "enabled": enabled, "requested_at": null,
                "wake_not_before": null, "created_at": "2026-11-01T00:00:00Z",
                "updated_at": "2026-11-01T00:00:00Z",
                "last_wake": { "woke_at": "2026-11-02T08:00:00Z", "status": "completed",
                               "turn_id": Uuid::from_u128(92) }
            })
        };
        let mut first = snapshot(1, vec![], None);
        first.webhooks = vec![serde_json::from_value(webhook(true)).unwrap()];
        let mut conversation = Conversation::from_snapshot(first).unwrap();
        let listed = &conversation.webhooks()[0];
        assert_eq!(listed.name, "Deploys");
        assert_eq!(
            listed.last_wake.as_ref().unwrap().woke_at.to_rfc3339(),
            "2026-11-02T08:00:00+00:00"
        );
        let update = |seq, webhooks| {
            frame(
                seq,
                "bot.updated",
                json!({ "bot": { "id": bot(), "name": "Assistant" }, "webhooks": webhooks }),
            )
        };
        conversation
            .apply(&update(2, json!([webhook(false)])))
            .unwrap();
        assert!(!conversation.webhooks()[0].enabled);
        conversation.apply(&update(3, json!([]))).unwrap();
        assert!(conversation.webhooks().is_empty());
    }

    #[test]
    fn watchers_default_to_empty_on_older_servers() {
        let conversation = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        assert!(conversation.watchers().is_empty());
        let bot: Bot = serde_json::from_value(json!({ "id": bot(), "name": "Assistant" })).unwrap();
        assert_eq!(bot.watching, 0);
    }

    #[test]
    fn watchers_follow_the_snapshot_and_live_updates() {
        let mut page = snapshot_json(1, vec![], None);
        let mut watcher = json!({
            "id": "c7947c9e-2f91-4fb5-8fcc-ec4b20ca9307", "purpose": "Domain availability",
            "every_minutes": 15, "until": "2026-12-04T10:00:00.000+00:00", "status": "waiting",
            "last_status": null, "last_checked_at": null, "next_check_at": "2026-11-20T08:13:00Z",
            "last_output": null, "checks": 0, "failures": 0, "created_at": "2026-11-20T08:00:00Z",
            "created_turn_id": null,
        });
        page["watchers"] = json!([watcher]);
        let mut conversation =
            Conversation::from_snapshot(serde_json::from_value(page).unwrap()).unwrap();
        let [active] = conversation.watchers() else {
            panic!("expected one active watcher")
        };
        assert_eq!(active.purpose, "Domain availability");
        assert_eq!(active.until.to_rfc3339(), "2026-12-04T10:00:00+00:00");
        let WatcherStatus::Waiting { next_check_at } = active.status else {
            panic!("expected a watcher waiting for its next check")
        };
        assert_eq!(
            next_check_at.unwrap().to_rfc3339(),
            "2026-11-20T08:13:00+00:00"
        );
        assert_eq!(active.last_checked_at, None);
        let update = |seq, count, watchers| {
            frame(
                seq,
                "bot.updated",
                json!({ "bot": { "id": bot(), "name": "Assistant", "watching": count }, "watchers": watchers }),
            )
        };
        watcher["status"] = json!("checking");
        watcher["last_status"] = json!("unavailable");
        watcher["last_output"] = json!("Domain is registered");
        conversation.apply(&update(2, 1, json!([watcher]))).unwrap();
        let active = &conversation.watchers()[0];
        assert_eq!(active.status, WatcherStatus::Checking);
        assert_eq!(active.last_status.as_deref(), Some("unavailable"));
        assert_eq!(active.last_output.as_deref(), Some("Domain is registered"));
        conversation.apply(&update(3, 0, json!([]))).unwrap();
        assert!(conversation.watchers().is_empty());
    }

    #[test]
    fn older_servers_without_social_triggers_use_an_empty_list() {
        let mut conversation = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        assert!(conversation.social_triggers().is_empty());
        conversation.social_triggers =
            vec![serde_json::from_value(social_trigger(true, "telegram")).unwrap()];
        conversation.apply(&bot_updated(2, None)).unwrap();
        assert!(conversation.social_triggers().is_empty());
    }

    #[test]
    fn a_late_change_response_never_undoes_a_newer_event() {
        let mut first = snapshot(1, vec![], None);
        first.routines =
            vec![serde_json::from_value(routine(true, "2026-11-01T00:00:00Z", None)).unwrap()];
        let mut c = Conversation::from_snapshot(first).unwrap();
        let paused: Routine =
            serde_json::from_value(routine(false, "2026-11-02T00:00:00Z", None)).unwrap();
        c.apply_routine(paused.clone());
        assert!(!c.routines()[0].enabled, "a newer response applies");
        c.apply(&bot_updated(
            2,
            Some(vec![routine(true, "2026-11-03T00:00:00Z", None)]),
        ))
        .unwrap();
        c.apply_routine(paused);
        assert!(c.routines()[0].enabled);
    }

    #[test]
    fn turn_updates_carry_secret_request_metadata() {
        let mut c = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        let request = Uuid::from_u128(77);
        let turn = |status: &str, request_status: &str| {
            json!({ "turn": {
                "id": Uuid::from_u128(9), "request_id": Uuid::from_u128(8), "turn_no": 1,
                "provider_id": "synthetic", "status": status, "error": null,
                "effect_uncertain": false,
                "secret_requests": [{ "id": request, "name": "deploy", "reason": "Deploy",
                    "env": ["DEPLOY_KEY"], "status": request_status }],
            }})
        };
        c.apply(&frame(2, "turn.updated", turn("waiting", "pending")))
            .unwrap();
        assert!(c.secret_request(request).unwrap().is_pending());
        assert_eq!(c.secret_requests().len(), 1);
        c.apply(&frame(3, "turn.updated", turn("queued", "stored")))
            .unwrap();
        assert_eq!(
            c.secret_request(request).unwrap().status,
            SecretRequestStatus::Stored
        );
        let plain = json!({ "turn": {
            "id": Uuid::from_u128(10), "request_id": Uuid::from_u128(11), "turn_no": 2,
            "provider_id": "synthetic", "status": "queued", "error": null,
            "effect_uncertain": false,
        }});
        assert_eq!(
            c.apply(&frame(4, "turn.updated", plain)),
            Ok(Applied::Changed)
        );
    }

    #[test]
    fn answered_questions_stay_before_their_continuation() {
        let mut before = item(2, 2, "assistant", "Please choose.");
        before["turn_id"] = json!(Uuid::from_u128(901));
        let mut after = item(3, 3, "assistant", "Continuing with your answer.");
        after["turn_id"] = json!(Uuid::from_u128(901));
        after["created_at"] = json!("2026-11-02T00:00:02Z");
        let conversation = Conversation::from_snapshot(snapshot(
            3,
            vec![item(1, 1, "user", "Help me choose."), before, after],
            None,
        ))
        .unwrap();
        assert_eq!(
            conversation.question_anchor(
                Uuid::from_u128(901),
                Some("2026-11-02T00:00:01Z".parse().unwrap())
            ),
            Some(Uuid::from_u128(2))
        );
        assert_eq!(
            conversation.question_anchor(Uuid::from_u128(901), None),
            Some(Uuid::from_u128(3))
        );
    }

    #[test]
    fn outgoing_revisions_and_receipts_survive_events_and_snapshot_reload() {
        let id = Uuid::from_u128(700);
        let next = Uuid::from_u128(701);
        let make_turn = |turn: u128, number, draft_id, status: &str, version| {
            json!({
                "id": Uuid::from_u128(turn), "request_id": Uuid::from_u128(turn + 10),
                "turn_no": number, "provider_id": "synthetic", "status": "waiting", "error": null,
                "effect_uncertain": false,
                "outgoing_drafts": [{"id": draft_id, "lineage_id": id, "revision": number,
                    "content": {"channel":"email", "from": "main@example.invalid", "to": ["ji@example.invalid"], "cc": [], "bcc": [],
                    "subject": "Edited subject", "body": "Edited message"}, "version": version,
                    "status": status, "can_approve": status == "pending", "result_detail": null,
                    "updated_at": "2026-11-05T00:00:02Z", "settled_at": null}]
            })
        };
        let mut c = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        c.apply(&frame(
            2,
            "turn.updated",
            json!({"turn": make_turn(900, 1, id, "pending", 2)}),
        ))
        .unwrap();
        assert_eq!(
            c.open_draft().unwrap().edit().subject.as_deref(),
            Some("Edited subject")
        );
        let old = make_turn(900, 1, id, "superseded", 2);
        let revision = make_turn(901, 2, next, "pending", 1);
        c.apply(&frame(3, "turn.updated", json!({"turn": old})))
            .unwrap();
        c.apply(&frame(4, "turn.updated", json!({"turn": revision})))
            .unwrap();
        assert_eq!(c.open_draft().unwrap().id, next);
        let mut snap = snapshot(4, vec![], None);
        snap.turns = serde_json::from_value(json!([old, revision])).unwrap();
        let reloaded = Conversation::from_snapshot(snap).unwrap();
        assert_eq!(reloaded.open_draft(), c.open_draft());
        let approval: Turn =
            serde_json::from_value(make_turn(901, 2, next, "approved", 1)).unwrap();
        let response = approval.outgoing_drafts[0].clone();
        c.apply(&frame(
            5,
            "turn.updated",
            json!({"turn": make_turn(901, 2, next, "approved", 1)}),
        ))
        .unwrap();
        assert!(c.open_draft().is_none());
        assert_eq!(
            c.outgoing_draft(next).unwrap().status,
            DraftStatus::Approved
        );
        c.apply(&frame(
            6,
            "turn.updated",
            json!({"turn": make_turn(901, 2, next, "sent", 1)}),
        ))
        .unwrap();
        c.update_draft(response);
        assert_eq!(
            c.outgoing_draft(next).unwrap().status,
            DraftStatus::Sent,
            "a delayed approval response cannot undo a sent event"
        );
    }

    #[test]
    fn turn_updates_carry_owner_questions_through_their_states() {
        let mut c = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        let question = Uuid::from_u128(78);
        let turn = |status: &str, question_status: &str, answer: Option<&str>| {
            json!({ "turn": {
                "id": Uuid::from_u128(9), "request_id": Uuid::from_u128(8), "turn_no": 1,
                "provider_id": "synthetic", "status": status, "error": null,
                "effect_uncertain": false,
                "questions": [{ "id": question, "kind": "destructive",
                    "title": "Remove the old site", "context": "rm -rf ~/work/old-site",
                    "choices": [{ "id": "approve", "label": "Approve once" },
                                { "id": "deny", "label": "Deny" },
                                { "id": "approve_forever", "label": "Always allow destructive actions" }],
                    "status": question_status, "answer": answer }],
            }})
        };
        c.apply(&frame(2, "turn.updated", turn("waiting", "pending", None)))
            .unwrap();
        let shown = c.question(question).unwrap();
        assert!(shown.is_pending() && shown.is_approval());
        assert_eq!(shown.choices.len(), 3);
        c.apply(&frame(
            3,
            "turn.updated",
            turn("queued", "answered", Some("approve")),
        ))
        .unwrap();
        let shown = c.question(question).unwrap();
        assert_eq!(shown.chosen().unwrap().label, "Approve once");
        c.apply(&frame(
            4,
            "turn.updated",
            turn("interrupted", "skipped", None),
        ))
        .unwrap();
        assert_eq!(
            c.question(question).unwrap().status,
            QuestionStatus::Skipped
        );
        assert_eq!(c.question(question).unwrap().chosen(), None);
    }

    #[test]
    fn multiple_question_answers_survive_events_and_snapshot_reload() {
        let mut c = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        let question = Uuid::from_u128(79);
        let turn = |status: &str, answer: serde_json::Value| {
            json!({
                "id": Uuid::from_u128(9), "request_id": Uuid::from_u128(8), "turn_no": 1,
                "provider_id": "synthetic", "status": "waiting", "error": null,
                "effect_uncertain": false,
                "questions": [{ "id": question, "kind": "choice", "title": "Which tasks?",
                    "choices": [{"id": "a", "label": "Archive"}, {"id": "b", "label": "Summarize"}],
                    "multiple": true, "status": status, "answer": answer }],
            })
        };
        c.apply(&frame(
            2,
            "turn.updated",
            json!({"turn": turn("pending", serde_json::Value::Null)}),
        ))
        .unwrap();
        assert!(c.question(question).unwrap().multiple);
        let answered = turn("answered", json!(["a", "b"]));
        c.apply(&frame(3, "turn.updated", json!({"turn": answered})))
            .unwrap();
        let answer = c.question(question).unwrap().answer.as_ref().unwrap();
        assert!(answer.contains("a") && answer.contains("b"));
        assert!(answer.single().is_none());
        let mut snap = snapshot(3, vec![], None);
        snap.turns = serde_json::from_value(json!([answered])).unwrap();
        let reloaded = Conversation::from_snapshot(snap).unwrap();
        assert_eq!(reloaded.question(question), c.question(question));
        c.apply(&frame(
            4,
            "turn.updated",
            json!({"turn": turn("skipped", serde_json::Value::Null)}),
        ))
        .unwrap();
        assert!(!c.question(question).unwrap().is_pending());
        assert!(c.question(question).unwrap().answer.is_none());
    }
}

#[cfg(test)]
mod vault_request_tests {
    use super::SecretRequest;
    #[test]
    fn vault_requests_read_optional_metadata_and_older_payloads() {
        let mut data = serde_json::json!({"id": uuid::Uuid::nil(), "name": "x_login", "reason": "Sign in", "env": ["USERNAME", "PASSWORD"], "status": "pending"});
        let old: SecretRequest = serde_json::from_value(data.clone()).unwrap();
        assert!(old.description.is_none());
        assert!(old.website.is_none());
        data["description"] = serde_json::json!("X, @owner");
        data["website"] = serde_json::json!("x.com");
        let request: SecretRequest = serde_json::from_value(data).unwrap();
        assert_eq!(request.description.as_deref(), Some("X, @owner"));
        assert_eq!(request.website.as_deref(), Some("x.com"));
        assert!(request.is_pending());
    }
}
