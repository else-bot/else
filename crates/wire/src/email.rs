wire_state! {
    pub enum AccountState {
        Active = "active",
        Paused = "paused",
        Removed = "removed",
    }
    _ => Unknown
}

wire_state! {
    /// What the server last saw of an account's mail server.
    pub enum AccountHealth {
        /// Not checked since the account or its credential changed.
        Unchecked = "unknown",
        Connected = "connected",
    }
    /// Any failure, sent as its error code.
    _ => Failing(String)
}
