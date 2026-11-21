wire_state! {
    pub enum SetupStatus {
        Pending = "pending",
        Connected = "connected",
        Cancelled = "cancelled",
    }
    _ => Unknown
}

wire_state! {
    /// A connection's state as the server sends it; a newer server's state
    /// keeps its text so the panel can still show it.
    pub enum ConnectionState {
        Active = "active",
        Paused = "paused",
        Removed = "removed",
    }
    _ => Other(String)
}

wire_state! {
    /// How a connection's listener is doing; a newer server's health keeps
    /// its text so the panel can still show it.
    pub enum Health {
        Connecting = "connecting",
        Connected = "connected",
        Reconnecting = "reconnecting",
        /// Another receiver took the platform's single listener slot.
        ReceiverConflict = "receiver_conflict",
        /// The account signed this device out; the owner must pair it again.
        SignedOut = "signed_out",
        Paused = "paused",
        Removed = "removed",
        Restored = "restored",
    }
    _ => Other(String)
}
