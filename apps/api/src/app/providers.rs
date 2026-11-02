//! Provider execution profiles and the adapter interface.
//!
//! Adapters translate one turn into provider-specific process communication.
//! They report progress only through a [`TurnSink`], which persists before
//! returning; PostgreSQL stays authoritative for visible history. Provider
//! event formats never cross this boundary.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// A saved default execution profile for one provider.
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
    pub bot_role: String,
    pub text: String,
    /// The profile snapshot taken when the message was accepted.
    pub profile: Option<Profile>,
    /// Settled history from owner turns before this one, oldest first.
    pub history: Vec<HistoryEntry>,
    /// The bot's active native session with this provider, if any.
    pub session: Option<SessionState>,
    /// Task grant for else.bot tools during this turn; revoked when it ends.
    pub tool_grant: Option<String>,
    /// The bot's saved memory, bounded, for a new session.
    pub memory: Option<MemoryContext>,
    /// Provider attempt number of this turn (continuations count up).
    pub attempt: i32,
    pub delegation: DelegationContext,
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

/// What an attempt knows about delegation, all derived by the server.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DelegationContext {
    /// This attempt may delegate (main bot, owner conversation).
    pub can_delegate: bool,
    /// This turn is a delegated task (a leaf).
    pub delegated_task: bool,
    /// For a delegated task: the bot that delegated it.
    pub parent_bot_id: Option<Uuid>,
    /// Other active bots: ID, name, role.
    pub registry: Vec<(Uuid, String, String)>,
    pub remaining: i64,
    /// Child outcomes not yet given to an attempt of this turn.
    pub outcomes: Vec<DelegationOutcome>,
}

/// Bounded saved memory for a session that has not received it.
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

/// Durable progress reporting for one turn. Every method persists before
/// returning, so a crash cannot lose a state the provider already acted on.
/// Adapters must await each call to completion and never race it in a
/// `select!` that could drop it midway.
#[async_trait]
pub trait TurnSink: Send + Sync {
    /// Called immediately before the first write of the owner's message to
    /// the provider. After this, a crash leaves delivery uncertain.
    async fn sending(&self) -> Result<(), Fenced>;
    /// The provider acknowledged the input.
    async fn running(&self) -> Result<(), Fenced>;
    /// Append assistant text. It is buffered and persisted by the scheduler
    /// within about 100 ms; this call performs no I/O.
    async fn text(&self, chunk: &str) -> Result<(), Fenced>;
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
    /// Run one turn to a terminal outcome. Cancellation must interrupt the
    /// provider and return only after it has stopped or been terminated.
    async fn run(
        &self,
        input: TurnInput,
        sink: &dyn TurnSink,
        cancel: CancellationToken,
    ) -> TurnOutcome;
}
