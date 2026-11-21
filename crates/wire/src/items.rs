wire_state! {
    pub enum ItemKind {
        User = "user",
        Assistant = "assistant",
        Status = "status",
        Routine = "routine",
    }
    _ => Unknown
}
