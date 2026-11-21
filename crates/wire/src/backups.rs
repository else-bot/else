wire_state! {
    pub enum RemoteBackupStatus {
        Completed = "completed",
        Failed = "failed",
        /// Removed by retention.
        Retired = "retired",
    }
    /// Still uploading, or a status this desktop does not know yet.
    _ => Unknown
}
