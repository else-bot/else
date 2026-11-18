//! Provider execution profiles and the adapter interface.
//!
//! Adapters translate one turn into provider-specific process communication.
//! They report progress only through a [`TurnSink`], which persists before
//! returning; SQLite stays authoritative for visible history. Provider
//! event formats never cross this boundary.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::messages::{Attachment, MessagePart};

/// One model an adapter applies exactly, with the efforts and tiers it
/// takes on that model. `priority` is the Fast tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Model {
    pub id: String,
    pub label: String,
    pub efforts: Vec<String>,
    pub tiers: Vec<String>,
    /// The newest model of its family, shown before the picker expands.
    pub featured: bool,
    /// The newest model of the same family, when this one is older.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<String>,
}

/// The models an adapter applies exactly: what the installed CLI last
/// listed, else the bundled list. Empty for a provider without profiles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// Featured models first, then older ones, newest first per family.
    pub models: Vec<Model>,
    /// When the CLI listed these; `None` for the bundled list.
    pub listed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Why the latest ask failed, while an older or bundled list is shown.
    pub refresh_error: Option<String>,
}

impl Catalog {
    pub fn model(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|model| model.id == id)
    }

    pub fn ids(&self) -> Vec<&str> {
        self.models.iter().map(|model| model.id.as_str()).collect()
    }
}

/// Older desktops read the flat lists and apply every effort and tier to
/// every model, so they get the first model's efforts and tiers and only
/// the models that take all of them. `details` carries each model's own.
impl Serialize for Catalog {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let (efforts, tiers): (&[String], &[String]) = self
            .models
            .first()
            .map_or((&[], &[]), |first| (&first.efforts, &first.tiers));
        let legacy: Vec<&Model> = self
            .models
            .iter()
            .filter(|model| {
                efforts.iter().all(|e| model.efforts.contains(e))
                    && tiers.iter().all(|t| model.tiers.contains(t))
            })
            .collect();
        let ids = |featured_only: bool| -> Vec<&str> {
            legacy
                .iter()
                .filter(|model| !featured_only || model.featured)
                .map(|model| model.id.as_str())
                .collect()
        };
        let mut out = serializer.serialize_struct("Catalog", 7)?;
        out.serialize_field("models", &ids(false))?;
        out.serialize_field("featured_models", &ids(true))?;
        out.serialize_field("efforts", efforts)?;
        out.serialize_field("tiers", tiers)?;
        out.serialize_field("details", &self.models)?;
        out.serialize_field("listed_at", &self.listed_at)?;
        out.serialize_field("refresh_error", &self.refresh_error)?;
        out.end()
    }
}

/// An execution profile: a provider's saved default, or a bot's own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub model: String,
    pub reasoning_effort: String,
    pub service_tier: String,
}

/// Profiles seeded by explicit setup when missing. GPT 6.1 Sol Medium Fast:
/// `priority` is the request value for the Fast tier.
pub fn default_profiles() -> Vec<(&'static str, Profile)> {
    vec![(
        "codex",
        Profile {
            model: "gpt-6.1-sol".to_owned(),
            reasoning_effort: "medium".to_owned(),
            service_tier: "priority".to_owned(),
        },
    )]
}

/// One prior history entry, used by adapters that rebuild context.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub turn_no: i64,
    pub kind: String,
    pub text: String,
    pub status: crate::db::turns::TurnStatus,
    /// Why a failed or uncertain turn ended that way.
    pub error: Option<String>,
    /// A conversation message's ordered parts; empty for plain text.
    pub parts: Vec<MessagePart>,
}

/// An attached file with its bytes, for an adapter to hand to its provider.
#[derive(Clone)]
pub struct InputFile {
    pub attachment: Attachment,
    pub data: std::sync::Arc<[u8]>,
}

impl std::fmt::Debug for InputFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputFile")
            .field("attachment", &self.attachment)
            .finish_non_exhaustive()
    }
}

/// A guest job of the bot, recorded by the API (not by any provider), so a
/// receiving session can inspect it by ID instead of starting it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobReceipt {
    pub id: Uuid,
    pub status: String,
    pub command: String,
    pub started_at: String,
}

/// Context handed to the receiving provider on a provider switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffContext {
    pub id: Uuid,
    pub source_provider: String,
    /// The outgoing provider's checkpoint; `None` uses the recorded history
    /// alone (the deterministic fallback).
    pub checkpoint: Option<String>,
    /// Why there is no checkpoint.
    pub reason: Option<String>,
}

/// Which history a packet carried, for the handoff record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// Owner turns whose history items were included.
    pub selected: Vec<i64>,
    /// Owner turns whose history items were left out for the budget.
    pub omitted: Vec<i64>,
}

/// A request for a bounded checkpoint from an outgoing provider. It must
/// run with tools disabled and verified so.
#[derive(Debug, Clone)]
pub struct CheckpointRequest {
    pub bot_id: Uuid,
    /// The outgoing provider's saved profile, if it uses one.
    pub profile: Option<Profile>,
    pub prompt: String,
    pub max_bytes: usize,
}

/// Everything an adapter needs for one turn. Identity comes from the
/// scheduler's records, never from model output.
#[derive(Debug, Clone)]
pub struct TurnInput {
    pub bot_id: Uuid,
    pub conversation_id: Uuid,
    pub turn_id: Uuid,
    pub turn_no: i64,
    pub bot_name: String,
    pub text: String,
    /// The owner message's ordered parts; empty for plain text. `text` is
    /// their flattening.
    pub parts: Vec<MessagePart>,
    /// Newer owner messages joined to this turn, oldest first: those an
    /// earlier attempt delivered and those this request delivers. Their
    /// files are in [`TurnInput::files`].
    pub steered: Vec<Steered>,
    /// Bytes of the files this turn's packet may name: the message's own,
    /// then those of missed history within a budget, newest first.
    pub files: Vec<InputFile>,
    /// The profile snapshot taken when the message was accepted.
    pub profile: Option<Profile>,
    /// Settled conversation history before this turn, oldest first.
    pub history: Vec<HistoryEntry>,
    /// The bot's active native session with this provider, if any.
    pub session: Option<SessionState>,
    /// Task grant for else.bot tools during this turn; revoked when it ends.
    pub tool_grant: Option<String>,
    /// The bot's saved memory, bounded, for a session that lacks this revision.
    pub memory: Option<MemoryContext>,
    /// Provider attempt number of this turn (continuations count up).
    pub attempt: i32,
    pub delegation: DelegationContext,
    /// The owner's answers to this turn's secret requests that no attempt
    /// has received yet. Names only, never values.
    pub secrets: Vec<SecretOutcome>,
    /// The owner's answers to this turn's questions and approval requests
    /// that no attempt has received yet.
    pub questions: Vec<QuestionOutcome>,
    pub standing_permissions: Vec<super::questions::PermissionTarget>,
    /// The recurring message tasks the owner approves always, for drafts to
    /// name exactly; empty when this turn's drafts reach no standing grant.
    pub draft_tasks: Vec<super::outgoing::DraftKind>,
    /// Owner decisions on this turn's outgoing drafts not yet received,
    /// and the draft open when the owner sent this turn's message.
    pub outgoing_drafts: Vec<super::outgoing::Note>,
    pub social_setups: Vec<crate::socials::store::Setup>,
    /// Settled waits on MCP jobs that no attempt has received yet.
    pub mcp_waits: Vec<McpWaitOutcome>,
    /// Waits on MCP jobs this turn still holds, including those an earlier
    /// owner message started and this one carried forward.
    pub mcp_waits_in_progress: Vec<McpWaitInProgress>,
    /// Settled waits on the bot's background computer jobs that no
    /// attempt has received yet.
    pub computer_job_waits: Vec<crate::db::computer_job_waits::Outcome>,
    /// The shared MCPs installed for every bot, for a turn that may use
    /// them; `None` when it may not or the library could not be read.
    pub mcps: Option<Vec<InstalledMcp>>,
    /// The owner's answers to this turn's skill install cards that no
    /// attempt has received yet.
    pub skill_installs: Vec<crate::db::skills::Outcome>,
    /// What the owner did with this turn's take-over requests, not yet
    /// received by an attempt.
    pub computer_handoffs: Vec<crate::db::computer_handoffs::Outcome>,
    /// The bot's latest take-over request the owner accepted, so every turn
    /// knows whether the owner still holds its computer.
    pub computer_control: Option<crate::db::computer_handoffs::Handoff>,
    /// The installed skills this bot may use, for a turn that may use them;
    /// `None` when it may not or the list could not be read.
    pub skills: Option<Vec<crate::db::skills::SkillInfo>>,
    /// Set when this turn switches provider and the session missed turns.
    pub handoff: Option<HandoffContext>,
    /// The bot's active and unresolved guest jobs, given with any rebuilt
    /// or handed-off context regardless of which history was selected.
    pub jobs: Vec<JobReceipt>,
    /// Set when this turn runs a saved routine or webhook wake: `text` is
    /// its prompt, not an owner message.
    pub run: Option<SavedRun>,
    /// The bot's current rules file, given at the start of every turn.
    pub rules: Option<String>,
    /// Owner-approved calls that ran in recent turns of this conversation
    /// that did not complete, oldest first.
    pub ran_actions: Vec<crate::db::owner_questions::RanAction>,
    /// This turn's replies from its earlier attempts, oldest first and
    /// bounded, for a continuation in a session that never saw them: the
    /// owner's chat folds them under the next reply.
    pub earlier_replies: Vec<String>,
}

/// One shared MCP as a turn lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledMcp {
    pub id: String,
    pub name: String,
    pub description: String,
}

impl TurnInput {
    pub fn grant_reach(&self) -> super::questions::GrantReach {
        super::questions::GrantReach::of(self.delegation.origin, !self.delegation.delegated_task())
    }
}

/// The saved task a turn runs instead of an owner message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SavedRun {
    Routine(RoutineRun),
    Webhook(WebhookRun),
}

/// The webhook wake a turn runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookRun {
    pub webhook_id: Uuid,
    pub name: String,
    pub requested_at: chrono::DateTime<chrono::Utc>,
}

/// The routine occurrence a turn runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineRun {
    pub routine_id: Uuid,
    pub name: String,
    pub due_at: chrono::DateTime<chrono::Utc>,
    /// The routine is a reminder, due once.
    pub reminder: bool,
}

/// A provider's native session for one conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    pub native_id: String,
    pub fingerprint: String,
    /// History through this owner turn was already given to the session.
    pub considered_through_turn: i64,
    /// The memory view revision given to the session, if any.
    pub memory_revision: Option<i64>,
    /// Content hashes of the bundled default skills acknowledged by this session.
    pub default_skills: std::collections::BTreeMap<String, String>,
    /// Content hashes of the context blocks (registry, permissions) it holds.
    pub context: std::collections::BTreeMap<String, String>,
}

/// The recorded outcome of a delegated child, given to the parent's
/// continuation as data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationOutcome {
    pub id: Uuid,
    pub target_bot_id: Uuid,
    pub target_name: String,
    pub task: String,
    pub status: String,
    pub result: Option<String>,
    pub error: Option<String>,
    /// The delegating bot's copies of files the task's replies attached.
    pub files: Vec<crate::app::messages::Attachment>,
    /// Every message the task sent, when it sent more than one.
    pub messages: Option<crate::app::messages::Attachment>,
    /// Started for an earlier owner message and carried into this turn.
    pub earlier_request: bool,
    /// Owner-approved calls the task made in attempts that did not complete.
    pub ran: Vec<crate::db::owner_questions::RanAction>,
    /// What the task told the owner: `asked` while a notice of it or of a
    /// task below it waits on them, otherwise its declared need, if any.
    pub owner: Option<String>,
}

/// The owner's answer to a secret request: which task secret and
/// variables, and whether it was stored or declined. Never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretOutcome {
    pub id: Uuid,
    pub name: String,
    pub env: Vec<String>,
    pub stored: bool,
}

/// How a wait on an MCP job ended, with the last status result, given to
/// the continuation as remote data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpWaitOutcome {
    pub id: Uuid,
    pub mcp_id: String,
    pub tool: String,
    pub purpose: String,
    /// `finished`, `failed`, or `expired`.
    pub status: String,
    pub checks: i64,
    pub result: Option<String>,
}

/// A wait on an MCP job that still runs; its result arrives later as a
/// continuation of the turn that holds it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct McpWaitInProgress {
    pub id: Uuid,
    pub mcp_id: String,
    pub tool: String,
    pub purpose: String,
    pub checks: i64,
}

pub use super::questions::{Choice as QuestionChoice, Outcome as QuestionOutcome};

/// What a turn's work traces back to, derived by the server from stored
/// rows: a delegated task inherits the origin of the request that
/// delegated it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Origin {
    /// An owner message in the bot's owner conversation.
    OwnerRequest,
    /// A scheduled run of a saved routine.
    RoutineRun,
    /// A webhook wake: a saved prompt an outside caller triggered.
    WebhookWake,
    ConnectorEvent,
    SocialEvent,
    /// Anything else, such as the greeting that opens a conversation.
    #[default]
    Other,
}

impl Origin {
    /// Whether a turn `depth` delegations below its request may hand work
    /// to another bot. An email turn may pass mail on as the owner's
    /// standing instructions say, but its task may not hand it further; a
    /// social event may not delegate at all.
    pub fn delegates_at(self, depth: u8) -> bool {
        match self {
            Origin::SocialEvent => false,
            Origin::ConnectorEvent => depth == 0,
            _ => depth < crate::db::delegations::MAX_DEPTH,
        }
    }
}

/// What an attempt knows about delegation, all derived by the server.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DelegationContext {
    pub origin: Origin,
    /// Delegations above this turn: 0 in a bot's own conversation, 1 for a
    /// task, 2 for a task that task delegated. See `delegations::MAX_DEPTH`.
    pub task_depth: u8,
    /// For a delegated task: the bot that delegated it, ID and name.
    pub delegated_by: Option<(Uuid, String)>,
    /// For a delegated task: the owner's answer on a card the delegating
    /// bot asked. It lets the task manage its own routines and changes no
    /// other permission.
    pub owner_answer: Option<OwnerAnswer>,
    pub main_bot: bool,
    /// An owner request in this bot's own conversation.
    pub can_create_bot: bool,
    /// Other active bots: ID and name.
    pub registry: Vec<(Uuid, String)>,
    pub remaining: i64,
    /// Child outcomes not yet given to an attempt of this turn.
    pub outcomes: Vec<DelegationOutcome>,
    /// Tasks this turn still waits on, including those an earlier owner
    /// message started and this one carried forward.
    pub in_progress: Vec<DelegationInProgress>,
}

/// The owner's answer on a card, as else.bot recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerAnswer {
    pub question: String,
    pub context: Option<String>,
    pub answered_at: chrono::DateTime<chrono::Utc>,
    /// The labels of the choices the owner selected.
    pub chosen: Vec<String>,
}

/// A delegated task with no outcome yet; its result arrives later as a
/// continuation of the turn that waits on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationInProgress {
    pub id: Uuid,
    pub target_bot_id: Uuid,
    pub target_name: String,
    pub task: String,
}

impl DelegationContext {
    /// A task another bot delegated to this turn.
    pub fn delegated_task(&self) -> bool {
        self.task_depth > 0
    }

    /// Whether this turn may hand work to another bot. See
    /// [`Origin::delegates_at`].
    pub fn can_delegate(&self) -> bool {
        self.origin.delegates_at(self.task_depth)
    }

    /// A task an email turn handed on. Its task text came from email, so it
    /// works with nothing of its own bot's but the task: no memory or mail
    /// the email could read back through the reply.
    pub fn email_task(&self) -> bool {
        self.origin == Origin::ConnectorEvent && self.delegated_task()
    }

    /// Work done for the owner: the bot's own conversation, or a task another
    /// bot delegated for an owner request, a routine run, or a webhook wake,
    /// which may ask the owner itself. It may use mail and social tools and
    /// show the owner cards, which a task shows in its own bot's chat.
    pub fn acts_for_owner(&self) -> bool {
        !self.delegated_task()
            || matches!(
                self.origin,
                Origin::OwnerRequest | Origin::RoutineRun | Origin::WebhookWake
            )
    }

    /// A bot manages its own routines for work rooted in an owner message,
    /// or for a task delegated with the owner's answer on a card; never for
    /// a routine run or another task one delegated.
    pub fn can_manage_routines(&self) -> bool {
        self.origin == Origin::OwnerRequest || self.owner_answer.is_some()
    }

    /// A bot edits its rules while answering the owner directly in its own
    /// conversation, or in a task delegated with the owner's answer on a
    /// card; never for a routine, event, or other delegated task.
    pub fn can_edit_rules(&self) -> bool {
        (self.origin == Origin::OwnerRequest && !self.delegated_task())
            || self.owner_answer.is_some()
    }

    /// The provider's own host shell, file, and web tools. Never for work an
    /// incoming connector event started: its content is untrusted, so it
    /// gets only the tools its task grant names.
    pub fn host_tools(&self) -> bool {
        !matches!(self.origin, Origin::ConnectorEvent | Origin::SocialEvent)
    }
}

/// Bounded saved memory at one view revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryContext {
    pub revision: i64,
    /// The whole view block, for a session that holds none of it.
    pub text: String,
    /// The view's lines, oldest first; empty when the saved view is damaged.
    pub lines: Vec<MemoryLine>,
}

/// One rendered view line and the end of the entry range it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryLine {
    pub end: i64,
    pub text: String,
}

/// Terminal result of a provider attempt. Text alone never implies success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    /// Stopped on request, with the provider's confirmation.
    Interrupted,
    Failed {
        code: String,
        message: String,
    },
    /// Delivery or effects are unknown, e.g. the process died mid-turn.
    Uncertain {
        message: String,
    },
}

/// The sink refused a write because the turn is no longer the bot's active
/// work (cancelled, deleted, or reconciled). The adapter must stop.
#[derive(Debug, Clone, thiserror::Error)]
#[error("turn is no longer active")]
pub struct Fenced;

/// Tokens one provider report counted. `input` is every prompt token,
/// cached ones included, so providers that count cache reads separately
/// (Claude, Grok) and those that count them inside input (Codex) compare.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: i64,
    pub cached_input: i64,
    pub cache_write: i64,
    pub output: i64,
}

impl Usage {
    /// Reads the `usage` of a Claude or Grok stream result, whose
    /// `input_tokens` leaves out cache reads and writes. `None` when the
    /// result reports no usage.
    pub fn from_result(event: &serde_json::Value) -> Option<Self> {
        let usage = event.get("usage")?;
        let count = |key: &str| usage[key].as_i64().unwrap_or(0).max(0);
        let cached_input = count("cache_read_input_tokens");
        let cache_write = count("cache_creation_input_tokens");
        Some(Self {
            input: count("input_tokens") + cached_input + cache_write,
            cached_input,
            cache_write,
            output: count("output_tokens"),
        })
    }
}

/// One rolling subscription window a provider reported: how much of its cap
/// the owner's account has used, from else.bot and every other client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitWindow {
    pub minutes: i64,
    /// Hundredths of a percent, so 6200 is 62%.
    pub used_basis_points: i64,
    pub resets_at: chrono::DateTime<chrono::Utc>,
}

impl LimitWindow {
    pub const FIVE_HOURS: i64 = 5 * 60;
    pub const WEEK: i64 = 7 * 24 * 60;

    pub fn used_percent(&self) -> f64 {
        self.used_basis_points as f64 / 100.0
    }
}

/// Durable progress reporting for one turn. Lifecycle transitions persist
/// before returning; assistant text is batched in a bounded buffer.
/// Adapters must await each call to completion and never race it in a
/// `select!` that could drop it midway.
#[async_trait]
pub trait TurnSink: Send + Sync {
    /// Record the packet about to be sent (its history selection and a
    /// digest), before [`TurnSink::sending`]. Bookkeeping only.
    async fn packet(&self, selection: &Selection, payload: &str) -> Result<(), Fenced> {
        let _ = (selection, payload);
        Ok(())
    }
    /// Retire the native session this attempt used: its native turn may be
    /// unfinished, so it must never be resumed. Bookkeeping only.
    async fn retire_session(&self) {}
    /// Called immediately before the first write of the owner's message to
    /// the provider. After this, a crash leaves delivery uncertain.
    async fn sending(&self) -> Result<(), Fenced>;
    /// The provider acknowledged the input.
    async fn running(&self) -> Result<(), Fenced>;
    /// Append assistant text through a bounded actor mailbox. Small chunks
    /// are batched for about 100 ms; reaching the flush threshold waits for
    /// persistence, applying backpressure to the provider.
    async fn text(&self, chunk: &str) -> Result<(), Fenced>;
    /// End the current agent message: later text starts a new assistant item.
    async fn end_message(&self) -> Result<(), Fenced>;
    /// Add the tokens a provider reported for this turn. Bookkeeping only.
    async fn usage(&self, usage: Usage) {
        let _ = usage;
    }
    /// Record the subscription windows the provider reported. Bookkeeping
    /// only.
    async fn limits(&self, windows: Vec<LimitWindow>) {
        let _ = windows;
    }
    /// Record the native session this turn uses (new or resumed). Replaces
    /// any other active session of this provider for the conversation.
    async fn session(&self, state: &SessionState) -> Result<(), Fenced>;
    /// Record the settings the provider reported (unknown fields null) and
    /// its turn ID.
    async fn reported(
        &self,
        settings: &serde_json::Value,
        native_turn_id: &str,
    ) -> Result<(), Fenced>;
    /// Newer owner messages steered into this running turn and not written
    /// to the provider yet, oldest first.
    async fn steers(&self) -> Vec<Steer> {
        Vec::new()
    }
    /// Record, before writing it, that steered message `turn_id` goes to
    /// the provider; after this its delivery is unknown until its echo.
    /// False when it is no longer pending.
    async fn steer_sending(&self, turn_id: Uuid) -> Result<bool, Fenced> {
        let _ = turn_id;
        Ok(false)
    }
    /// The provider answered that it did not take steered message
    /// `turn_id`: it waits for this turn's next attempt.
    async fn steer_refused(&self, turn_id: Uuid) {
        let _ = turn_id;
    }
    /// The provider took steered message `turn_id` into this turn (its
    /// echo arrived). Later text starts a new assistant item.
    async fn steered(&self, turn_id: Uuid) -> Result<(), Fenced> {
        let _ = turn_id;
        Ok(())
    }
}

/// A newer owner message joined to a running or parked turn.
#[derive(Debug, Clone)]
pub struct Steer {
    /// The message's own turn, which ends with the reply that takes it.
    pub turn_id: Uuid,
    pub text: String,
    /// Its ordered parts; empty for plain text.
    pub parts: Vec<crate::app::messages::MessagePart>,
    /// The bytes of its files, for an attempt that writes it mid-turn.
    pub files: Vec<InputFile>,
}

/// A joined message as a turn's request carries it.
#[derive(Debug, Clone)]
pub struct Steered {
    pub message: Steer,
    /// An earlier attempt delivered it, so a resumed native session has
    /// already seen it.
    pub seen: bool,
}

/// The owner's stop requests for one attempt. [`Stop::request`] asks the
/// provider to interrupt its turn gracefully. [`Stop::escalate`] follows
/// when that takes too long: the provider must then stop at once, verify
/// its cleanup, and return. Its caller keeps awaiting it either way, so
/// capacity is never released while provider processes may remain.
///
/// It also carries the wake for owner messages steered into the running
/// turn ([`Stop::steer`]); a provider that steers reads them from its sink.
#[derive(Debug, Clone, Default)]
pub struct Stop {
    graceful: CancellationToken,
    hard: CancellationToken,
    steer: Arc<tokio::sync::Notify>,
}

impl Stop {
    /// A stop whose graceful requests come from `graceful`.
    pub fn new(graceful: CancellationToken) -> Self {
        Self {
            graceful,
            hard: CancellationToken::new(),
            steer: Arc::default(),
        }
    }

    /// A newer owner message joined this turn: wake the attempt to take it.
    pub fn steer(&self) {
        self.steer.notify_one();
    }

    /// Wait for [`Stop::steer`]. Cancellation safe; a wake that came while
    /// nobody waited is kept.
    pub async fn steered(&self) {
        self.steer.notified().await;
    }

    pub fn request(&self) {
        self.graceful.cancel();
    }

    pub fn escalate(&self) {
        self.graceful.cancel();
        self.hard.cancel();
    }

    pub fn is_requested(&self) -> bool {
        self.graceful.is_cancelled()
    }

    pub async fn requested(&self) {
        self.graceful.cancelled().await;
    }

    pub async fn escalated(&self) {
        self.hard.cancelled().await;
    }

    /// The graceful request, for work that only stops gracefully.
    pub fn token(&self) -> &CancellationToken {
        &self.graceful
    }
}

/// Availability as reported in capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Available,
    Unavailable(String),
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Opaque stable ID used in requests and records.
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    /// The bot's own project, where files in assistant image replies live.
    fn project_dir(&self, _bot_id: Uuid) -> Option<std::path::PathBuf> {
        None
    }
    fn availability(&self) -> Availability;
    /// Whether accepted turns snapshot a saved profile for this provider.
    fn uses_profile(&self) -> bool;
    /// Reject a profile the adapter cannot apply exactly. No substitution.
    fn validate_profile(&self, profile: &Profile) -> Result<(), String>;
    /// The models this provider's CLI lists, kept current by
    /// [`crate::providers::models::run`]. `None` without profiles.
    fn models(&self) -> Option<&crate::providers::models::Models> {
        None
    }
    /// Ask the installed CLI which models it runs now.
    async fn list_models(&self) -> Result<Vec<crate::providers::models::Listed>, String> {
        Err(format!("{} does not list its models", self.label()))
    }
    /// The exact values `validate_profile` accepts, for the owner's
    /// pickers. Empty for a provider without profiles.
    fn catalog(&self) -> Arc<Catalog> {
        self.models()
            .map_or_else(Arc::default, crate::providers::models::Models::current)
    }
    /// A checkpoint of recorded history with no tools: only when the adapter
    /// can enforce and verify that nothing but text is produced. Errors mean
    /// the deterministic fallback is used. Cancellation stops it and reaps
    /// its process.
    async fn checkpoint(
        &self,
        request: CheckpointRequest,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let _ = (request, cancel);
        Err("this provider cannot produce a verified no-tool checkpoint".into())
    }
    /// Run one turn to a terminal outcome. A stop request must interrupt
    /// the provider; an escalation must end it at once. Either way it
    /// returns only after its processes have stopped.
    async fn run(&self, input: TurnInput, sink: &dyn TurnSink, stop: Stop) -> TurnOutcome;
}
