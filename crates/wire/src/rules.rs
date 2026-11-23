use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

wire_state! {
    /// Who wrote a rules version.
    pub enum RuleAuthor {
        Bot = "bot",
        Owner = "owner",
    }
    _ => Unknown
}

wire_state! {
    pub enum RuleChange {
        Added = "added",
        Changed = "changed",
        Removed = "removed",
        Restored = "restored",
    }
    _ => Unknown
}

wire_state! {
    pub enum DiffOp {
        Same = "same",
        Add = "add",
        Remove = "remove",
    }
    _ => Unknown
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub op: DiffOp,
    pub text: String,
}

/// The owner's card for one saved version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesUpdate {
    pub version: i64,
    pub section: String,
    pub change: RuleChange,
    pub owner_words: String,
    pub diff: Vec<DiffLine>,
    /// When the owner undid this change by restoring the version before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undone_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSection {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesVersion {
    pub version: i64,
    pub section: String,
    pub change: RuleChange,
    pub owner_words: String,
    pub author: RuleAuthor,
    pub restored_from: Option<i64>,
    pub diff: Vec<DiffLine>,
    pub created_at: DateTime<Utc>,
    /// When the owner undid this version by restoring the one before it.
    #[serde(default)]
    pub undone_at: Option<DateTime<Utc>>,
}

/// A bot's rules file with every saved version, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesFile {
    pub version: i64,
    pub markdown: String,
    pub sections: Vec<RuleSection>,
    pub history: Vec<RulesVersion>,
}

impl RulesVersion {
    /// What changed, as the history lists it: "Added Weekly report",
    /// "Restored v3", "Cleared all rules".
    pub fn title(&self) -> String {
        match (self.change, self.restored_from) {
            (RuleChange::Restored, Some(0)) => "Cleared all rules".to_owned(),
            (RuleChange::Restored, Some(from)) => format!("Restored v{from}"),
            (change, _) => format!(
                "{} {}",
                match change {
                    RuleChange::Added => "Added",
                    RuleChange::Removed => "Removed",
                    _ => "Changed",
                },
                self.section
            ),
        }
    }
}

impl RulesFile {
    /// Titles of the rules changed after version `seen`, lowercased.
    pub fn changed_since(&self, seen: i64) -> std::collections::HashSet<String> {
        self.history
            .iter()
            .filter(|v| {
                v.version > seen && v.change != RuleChange::Removed && !v.section.is_empty()
            })
            .map(|v| v.section.to_lowercase())
            .collect()
    }
}
