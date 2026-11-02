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
    /// Append assistant text. May be buffered briefly before persistence.
    async fn text(&self, chunk: &str) -> Result<(), Fenced>;
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
