wire_state! {
    pub enum SecretRequestStatus {
        Pending = "pending",
        Stored = "stored",
        Cancelled = "cancelled",
    }
    _ => Unknown
}
