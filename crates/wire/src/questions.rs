wire_state! {
    /// A question's kind; a newer server's kind keeps its text so an
    /// approval card can still name it.
    pub enum QuestionKind {
        Choice = "choice",
        Destructive = "destructive",
        External = "external",
    }
    _ => Other(String)
}

wire_state! {
    pub enum QuestionStatus {
        Pending = "pending",
        Answered = "answered",
        Skipped = "skipped",
        Cancelled = "cancelled",
    }
    _ => Unknown
}
