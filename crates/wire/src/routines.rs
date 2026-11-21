wire_state! {
    /// Where a routine's last due run is: awaiting its check, skipped, ended
    /// with no news, or its turn's status with live turns as `queued` or
    /// `running`. A newer server's status keeps its text so it can be shown.
    pub enum RunStatus {
        Checking = "checking",
        Queued = "queued",
        Running = "running",
        Skipped = "skipped",
        NoNews = "no_news",
        Completed = "completed",
        Interrupted = "interrupted",
        Failed = "failed",
        Uncertain = "uncertain",
    }
    _ => Other(String)
}

wire_state! {
    /// Why a routine's due run did not start a turn.
    pub enum SkipReason {
        Overlap = "overlap",
        QueueFull = "queue_full",
        ProviderUnavailable = "provider_unavailable",
        NothingNew = "nothing_new",
        CheckCancelled = "check_cancelled",
        CheckUnavailable = "check_unavailable",
    }
    _ => Unknown
}

wire_state! {
    /// Why else.bot switched a routine off.
    pub enum PausedReason {
        /// Its last failure needs the owner; retrying won't fix it.
        NeedsAction = "needs_action",
        /// It failed several runs in a row.
        Repeated = "repeated",
    }
    _ => Unknown
}
