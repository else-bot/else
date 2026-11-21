wire_state! {
    /// A bot's sidebar activity, projected from its turns.
    pub enum Phase {
        Idle = "idle",
        Queued = "queued",
        Working = "working",
        Cancelling = "cancelling",
    }
    _ => Unknown
}
