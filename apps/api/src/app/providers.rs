//! Provider execution profiles and the adapter interface.
//!
//! Adapters translate one turn into provider-specific process communication.
//! They report progress only through a [`TurnSink`], which persists before
//! returning; SQLite stays authoritative for visible history. Provider
//! event formats never cross this boundary.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::messages::{Attachment, MessagePart};

/// The models, reasoning efforts, and service tiers an adapter applies
/// exactly. `priority` is the Fast tier.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct Catalog {
    pub models: &'static [&'static str],
    /// Models shown before expanding the picker. Empty shows the full catalog.
    pub featured_models: &'static [&'static str],
    pub efforts: &'static [&'static str],
    pub tiers: &'static [&'static str],
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
    /// The owner turn's recorded status (`completed`, `interrupted`, ...).
    pub status: String,
    /// An owner message's ordered parts; empty for plain text.
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
    /// Owner decisions on this turn's outgoing drafts not yet received,
    /// and the draft open when the owner sent this turn's message.
    pub outgoing_drafts: Vec<super::outgoing::Note>,
    pub social_setups: Vec<crate::socials::store::Setup>,
    /// Set when this turn switches provider and the session missed turns.
    pub handoff: Option<HandoffContext>,
    /// The bot's active and unresolved guest jobs, given with any rebuilt
    /// or handed-off context regardless of which history was selected.
    pub jobs: Vec<JobReceipt>,
    /// Set when this turn runs a saved routine: `text` is its prompt, not
    /// an owner message.
    pub routine: Option<RoutineRun>,
}

/// The routine occurrence a turn runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineRun {
    pub routine_id: Uuid,
    pub name: String,
    pub due_at: chrono::DateTime<chrono::Utc>,
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
    ConnectorEvent,
    SocialEvent,
    /// Anything else, such as the greeting that opens a conversation.
    #[default]
    Other,
}

/// What an attempt knows about delegation, all derived by the server.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DelegationContext {
    pub origin: Origin,
    /// This turn is a delegated task (a leaf): it cannot delegate.
    pub delegated_task: bool,
    /// For a delegated task: the bot that delegated it, ID and name.
    pub delegated_by: Option<(Uuid, String)>,
    /// The installation's main bot in its owner conversation.
    pub can_create_bot: bool,
    /// Other active bots: ID and name.
    pub registry: Vec<(Uuid, String)>,
    pub remaining: i64,
    /// Child outcomes not yet given to an attempt of this turn.
    pub outcomes: Vec<DelegationOutcome>,
}

impl DelegationContext {
    /// Any bot's own conversation may hand work to another bot; a
    /// delegated task may not (one hop).
    pub fn can_delegate(&self) -> bool {
        !self.delegated_task && !matches!(self.origin, Origin::ConnectorEvent | Origin::SocialEvent)
    }

    /// A bot manages its own routines only for work rooted in an owner
    /// message, never for a routine run or a task one delegated.
    pub fn can_manage_routines(&self) -> bool {
        self.origin == Origin::OwnerRequest
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
    fn availability(&self) -> Availability;
    /// Whether accepted turns snapshot a saved profile for this provider.
    fn uses_profile(&self) -> bool;
    /// Reject a profile the adapter cannot apply exactly. No substitution.
    fn validate_profile(&self, profile: &Profile) -> Result<(), String>;
    /// The exact values `validate_profile` accepts, for the owner's
    /// pickers. Empty for a provider without profiles.
    fn catalog(&self) -> Catalog {
        Catalog::default()
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
    /// Run one turn to a terminal outcome. Cancellation must interrupt the
    /// provider and return only after it has stopped or been terminated.
    async fn run(
        &self,
        input: TurnInput,
        sink: &dyn TurnSink,
        cancel: CancellationToken,
    ) -> TurnOutcome;
}
