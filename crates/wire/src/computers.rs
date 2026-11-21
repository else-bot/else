wire_state! {
    /// What the runtime last observed of a bot's computer.
    pub enum RuntimeState {
        Absent = "absent",
        Creating = "creating",
        Starting = "starting",
        Running = "running",
        Pausing = "pausing",
        Paused = "paused",
        Resuming = "resuming",
        Stopping = "stopping",
        Stopped = "stopped",
        Missing = "missing",
        Error = "error",
    }
    _ => Unknown
}

wire_state! {
    /// Whether the owner let the computer start on demand or stopped it.
    pub enum DesiredState {
        Auto = "auto",
        Stopped = "stopped",
    }
    _ => Unknown
}
