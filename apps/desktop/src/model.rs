//! The client's conversation model: a snapshot plus contiguously applied
//! stream events. Any inconsistency (a gap, an unknown item, an offset
//! mismatch, an unknown kind) asks the caller to reload a snapshot rather than
//! guessing, so a bubble is never duplicated or corrupted.

mod skills;
pub use skills::*;

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::MailTrigger;
use crate::api::socials::Trigger;
use crate::api::{Activity, Bot};

mod outgoing;
pub use outgoing::{DraftAttachment, DraftEdit, DraftStatus, OutgoingDraft};

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
    pub kind: String,
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
    /// `bot` or `owner`.
    pub author: String,
    pub restored_from: Option<i64>,
    pub diff: Vec<DiffLine>,
    pub created_at: DateTime<Utc>,
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
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BotReply {
    pub sender_bot_id: Uuid,
    pub sender_name: String,
    pub prompt: String,
    pub status: BotReplyStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotReplyStatus {
    Completed,
    Failed,
    Cancelled,
}

impl BotReplyStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Completed => "Reply",
            Self::Failed => "Reply · Failed",
            Self::Cancelled => "Reply · Stopped",
        }
    }
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
    pub id: Uuid,
    pub name: String,
    pub reason: String,
    pub env: Vec<String>,
    /// `pending`, `stored`, or `cancelled`.
    pub status: String,
}

impl SecretRequest {
    pub fn is_pending(&self) -> bool {
        self.status == "pending"
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
    /// `choice`, `destructive`, or `external`.
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub action: Option<QuestionAction>,
    #[serde(default)]
    pub context: Option<String>,
    pub choices: Vec<QuestionChoice>,
    #[serde(default)]
    pub multiple: bool,
    /// `pending`, `answered`, `skipped`, or `cancelled`.
    pub status: String,
    /// The selected option id or ids once answered.
    #[serde(default)]
    pub answer: Option<QuestionAnswer>,
    #[serde(default)]
    pub settled_at: Option<DateTime<Utc>>,
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

    pub fn email_connection(&self) -> Option<&crate::api::EmailConsent> {
        self.action.as_ref()?.email_connection.as_ref()
    }

    pub fn is_pending(&self) -> bool {
        self.status == "pending"
    }

    /// An approval card, as opposed to a plain choice.
    pub fn is_approval(&self) -> bool {
        self.kind != "choice"
    }

    pub fn chosen(&self) -> Option<&QuestionChoice> {
        let answer = self.answer.as_ref()?.single()?;
        self.choices.iter().find(|c| c.id == answer)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Turn {
    pub id: Uuid,
    pub request_id: Uuid,
    pub turn_no: i64,
    pub provider_id: String,
    pub status: String,
    pub error: Option<TurnError>,
    pub effect_uncertain: bool,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub reaction: Option<Reaction>,
    #[serde(default)]
    pub secret_requests: Vec<SecretRequest>,
    #[serde(default)]
    pub skill_installs: Vec<SkillInstall>,
    /// Absent from servers without owner questions.
    #[serde(default)]
    pub questions: Vec<Question>,
    #[serde(default)]
    pub routine: Option<RoutineRun>,
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
}

/// A job in an installed MCP that a waiting turn rechecks until it ends.
/// A pending wait is the only hold of its attempt.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct McpWait {
    pub id: Uuid,
    pub mcp_id: String,
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
}

/// The last due occurrence of a routine: the turn it started, or why it
/// was skipped.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LastRun {
    pub due_at: DateTime<Utc>,
    /// `queued`, `running`, `skipped`, or the turn's terminal status.
    pub status: String,
    pub skip_reason: Option<String>,
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

/// A bot's saved task, run on a cron schedule in its conversation.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Routine {
    pub id: Uuid,
    pub name: String,
    pub prompt: String,
    pub cron: String,
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
    /// Why else.bot switched the routine off: `needs_action` or `repeated`.
    #[serde(default)]
    pub paused_reason: Option<String>,
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
    /// Absent from servers without rules.
    #[serde(default)]
    pub rules_version: i64,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub first: bool,
    pub last: bool,
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
    pub selected_provider_id: Option<String>,
    pub activity: Option<Activity>,
    routines: Vec<Routine>,
    social_triggers: Vec<Trigger>,
    mail_triggers: Vec<MailTrigger>,
    rules_version: i64,
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
            selected_provider_id: snapshot.selected_provider_id,
            activity: Some(snapshot.activity),
            routines: snapshot.routines,
            social_triggers: snapshot.social_triggers,
            mail_triggers: snapshot.mail_triggers,
            rules_version: snapshot.rules_version,
        };
        for item in snapshot.items {
            conversation.upsert_item(item);
        }
        for turn in snapshot.turns {
            conversation.turns.insert(turn.id, turn);
        }
        Ok(conversation)
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
                self.upsert_item(item);
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

    /// Show the canonical owner item from a send response before its event
    /// arrives. Idempotent: the later upsert replaces it by ID.
    pub fn insert_item(&mut self, item: Item) {
        if !self.seq_of.contains_key(&item.id) {
            self.upsert_item(item);
        }
    }

    /// Record a turn from a send response; a later event replaces it.
    pub fn insert_turn(&mut self, turn: Turn) {
        self.turns.entry(turn.id).or_insert(turn);
    }

    pub fn items(&self) -> impl DoubleEndedIterator<Item = &Item> {
        self.items.values()
    }

    pub fn latest_reply(&self) -> Option<&Item> {
        self.items
            .values()
            .rev()
            .find(|item| item.kind == "assistant")
    }

    pub fn item(&self, id: Uuid) -> Option<&Item> {
        self.seq_of.get(&id).and_then(|seq| self.items.get(seq))
    }

    pub fn turn(&self, id: Uuid) -> Option<&Turn> {
        self.turns.get(&id)
    }

    pub fn latest_unshown_turn(&self) -> Option<&Turn> {
        self.turns
            .values()
            .max_by_key(|turn| turn.turn_no)
            .filter(|turn| {
                (self.reaction_in(turn.id).is_some()
                    && matches!(
                        turn.status.as_str(),
                        "queued" | "preparing" | "sending" | "running" | "cancelling" | "waiting"
                    ))
                    || !self.items().any(|item| item.turn_id == turn.id)
            })
    }

    /// Where `id` sits in its run of same-kind items from one turn, such as
    /// the agent messages of one reply.
    pub fn run(&self, id: Uuid) -> Run {
        let Some((seq, item)) = self.seq_of.get(&id).zip(self.item(id)) else {
            return Run {
                first: true,
                last: true,
            };
        };
        let same = |other: Option<(&i64, &Item)>| {
            other.is_some_and(|(_, o)| o.kind == item.kind && o.turn_id == item.turn_id)
        };
        Run {
            first: !same(self.items.range(..seq).next_back()),
            last: !same(self.items.range(seq + 1..).next()),
        }
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

    pub fn secret_request(&self, id: Uuid) -> Option<&SecretRequest> {
        self.turns
            .values()
            .flat_map(|t| &t.secret_requests)
            .find(|r| r.id == id)
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

    /// The tasks this conversation waits on, oldest first: each single task
    /// of an unfinished turn with no outcome yet, or whose outcome no
    /// attempt has read. A council keeps its card in the transcript.
    pub fn waiting_on(&self) -> Vec<&Delegation> {
        let mut tasks: Vec<&Delegation> = self
            .turns
            .values()
            .filter(|turn| {
                matches!(
                    turn.status.as_str(),
                    "queued" | "waiting" | "preparing" | "sending" | "running"
                )
            })
            .flat_map(|turn| {
                turn.delegations.iter().filter(|d| {
                    (!d.status.settled() || !d.seen)
                        && turn
                            .delegations
                            .iter()
                            .filter(|other| other.shares_handoff(d))
                            .count()
                            == 1
                })
            })
            .collect();
        tasks.sort_by_key(|d| (d.created_at, d.id));
        tasks
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

    pub fn question_anchor(&self, turn: Uuid, settled_at: Option<DateTime<Utc>>) -> Option<Uuid> {
        self.items
            .values()
            .rev()
            .find(|item| {
                item.turn_id == turn
                    && settled_at
                        .is_none_or(|at| item.created_at.is_none_or(|created| created <= at))
            })
            .map(|item| item.id)
    }

    pub fn social_triggers(&self) -> &[Trigger] {
        &self.social_triggers
    }

    pub fn mail_triggers(&self) -> &[MailTrigger] {
        &self.mail_triggers
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

    pub fn is_feedback_acknowledgement(&self, item: &Item) -> bool {
        item.kind == "assistant"
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

    fn snapshot(seq: i64, items: Vec<serde_json::Value>, older: Option<i64>) -> Snapshot {
        serde_json::from_value(json!({
            "conversation_id": Uuid::new_v4(), "bot_id": bot(), "selected_provider_id": null,
            "event_cursor": format!("{PREFIX}:{seq}"), "items": items, "turns": [],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 },
            "older_before_seq": older,
        }))
        .unwrap()
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

    #[test]
    fn hidden_turn_status_is_available_until_its_first_visible_item() {
        let mut c = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        assert!(c.latest_unshown_turn().is_none());
        for (seq, status) in [(2, "running"), (3, "failed"), (4, "interrupted")] {
            let turn = json!({"id": Uuid::from_u128(901), "request_id": Uuid::from_u128(501), "turn_no": 1,
                "provider_id": "codex", "status": status, "error": null, "effect_uncertain": false});
            c.apply(&frame(seq, "turn.updated", json!({"turn":turn})))
                .unwrap();
            assert_eq!(c.latest_unshown_turn().unwrap().status, status);
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
        for status in ["queued", "running", "waiting", "cancelling"] {
            page.turns = vec![
                serde_json::from_value(json!({
                    "id": turn_id, "request_id": Uuid::new_v4(), "turn_no": 2,
                    "provider_id": "synthetic", "status": status, "error": null,
                    "effect_uncertain": false
                }))
                .unwrap(),
            ];
            let c = Conversation::from_snapshot(page.clone()).unwrap();
            assert_eq!(c.latest_unshown_turn().unwrap().status, status);
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
    fn agent_messages_of_one_turn_form_a_run() {
        let reply = |id, seq, text| {
            let mut item = item(id, seq, "assistant", text);
            item["turn_id"] = json!(Uuid::from_u128(901));
            item
        };
        let c = Conversation::from_snapshot(snapshot(
            1,
            vec![
                item(1, 1, "user", "hi"),
                reply(2, 2, "one"),
                reply(3, 3, "two"),
                reply(4, 4, "three"),
                item(5, 5, "user", "next"),
            ],
            None,
        ))
        .unwrap();
        let runs: Vec<_> = (1..=5)
            .map(|id| c.run(Uuid::from_u128(id)))
            .map(|r| (r.first, r.last))
            .collect();
        assert_eq!(
            runs,
            vec![
                (true, true),
                (true, false),
                (false, false),
                (false, true),
                (true, true)
            ]
        );
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
        assert_eq!(last.status, "failed");
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
                "state": "active", "health": "connected", "wake": wake
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
        assert_eq!(c.secret_request(request).unwrap().status, "stored");
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
        assert_eq!(c.question(question).unwrap().status, "skipped");
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
