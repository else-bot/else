//! Why a computer operation failed or waits.

use super::{Observed, RuntimeError, memory};

#[derive(Debug, thiserror::Error)]
pub enum ComputerError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("the lease is not active")]
    LeaseEnded,
    #[error("the computer is not running")]
    NotRunning,
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    /// A test fault point stopped the operation (a simulated crash).
    #[error("interrupted at a fault point")]
    Interrupted,
    /// The computer holds live work or is mid-transition (a backup defers).
    #[error("the computer is busy: {0}")]
    Busy(Busy),
    /// An explicit owner operation that does not apply (image upgrades).
    #[error("{0}")]
    Refused(String),
    /// An upgrade to the image the computer already runs.
    #[error("the computer already uses {0}")]
    AlreadyUses(String),
}

/// Why an operation waits for the computer instead of acting on it now.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Busy {
    #[error("in use")]
    InUse,
    #[error("a guest job is active; wait for it or cancel it first")]
    ActiveJob,
    /// The computer is not in a state the operation can start from.
    #[error("state {0}")]
    State(Observed),
    #[error("the computer budget is in use; stop another computer first")]
    BudgetFull,
    /// The computer changed under the operation (named here).
    #[error("the computer changed during the {0}")]
    Changed(&'static str),
    #[error("the runtime does not report it stopped")]
    NotStopped,
    #[error("its browser checkpoint failed: {0}")]
    CheckpointFailed(String),
    #[error("the computer runtime has not been inspected yet")]
    NotInspected,
    #[error("the API is stopping")]
    Stopping,
    #[error("the computer is being upgraded; try again in a few minutes")]
    Upgrading,
    #[error("the host's computer budget has no room for {} now", memory::gib(*.0))]
    NoRoomFor(u32),
    #[error("the computer is changing state; try again shortly")]
    ChangingState,
    #[error("the computer did not stop; its size is unchanged until its next start")]
    DidNotStop,
    #[error("the computer is set to {} but did not start again: {why}", memory::gib(*memory_mib))]
    NotRestarted { memory_mib: u32, why: String },
}

impl Busy {
    /// The owner-facing reason to put an upgrade off, when this passes by
    /// itself. Anything else fails the upgrade, so a state that would never
    /// pass (a lost disk, a failing browser checkpoint) is shown instead of
    /// retried forever.
    pub fn defers(&self) -> Option<&'static str> {
        match self {
            Self::InUse => Some("The bot is using its computer."),
            Self::ActiveJob => Some("The bot is running a job on its computer."),
            Self::Changed(_) => Some("The computer changed during the upgrade."),
            Self::State(state) if state.is_transient() => {
                Some("The computer is starting or stopping.")
            }
            Self::BudgetFull => Some("Other computers use the whole budget."),
            _ => None,
        }
    }
}
