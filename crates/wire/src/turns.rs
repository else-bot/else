wire_state! {
    pub enum TurnStatus {
        Queued = "queued",
        Preparing = "preparing",
        Sending = "sending",
        Running = "running",
        Cancelling = "cancelling",
        Waiting = "waiting",
        Completed = "completed",
        Interrupted = "interrupted",
        Failed = "failed",
        Uncertain = "uncertain",
    }
    _ => Unknown
}

impl TurnStatus {
    /// Not yet ended: queued, parked, or holding the bot's active slot.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Self::Queued
                | Self::Preparing
                | Self::Sending
                | Self::Running
                | Self::Cancelling
                | Self::Waiting
        )
    }

    /// A provider is working on it now.
    pub fn is_working(self) -> bool {
        matches!(self, Self::Preparing | Self::Sending | Self::Running)
    }

    /// Ended without completing.
    pub fn is_unsuccessful(self) -> bool {
        matches!(self, Self::Interrupted | Self::Failed | Self::Uncertain)
    }
}
