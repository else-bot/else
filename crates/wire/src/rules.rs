wire_state! {
    /// Who wrote a rules version.
    pub enum RuleAuthor {
        Bot = "bot",
        Owner = "owner",
    }
    _ => Unknown
}
