use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A task a turn handed to another bot. Its settled reply is the item
/// with the same ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

wire_state! {
    pub enum DelegationStatus {
        Queued = "queued",
        Running = "running",
        Completed = "completed",
        Failed = "failed",
        Cancelled = "cancelled",
    }
    _ => Unknown
}

impl DelegationStatus {
    pub fn settled(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}
