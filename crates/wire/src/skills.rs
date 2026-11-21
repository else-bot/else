use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

wire_state! {
    #[derive(Default)]
    pub enum SkillScope {
        #[default]
        All = "all",
        Bot = "bot",
    }
    _ => Unknown
}

wire_state! {
    pub enum SkillInstallStatus {
        Pending = "pending",
        Installed = "installed",
        Declined = "declined",
        Cancelled = "cancelled",
    }
    _ => Unknown
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSource {
    pub url: String,
    pub repo: String,
    pub path: String,
    pub commit: String,
}

impl SkillSource {
    pub fn label(&self) -> String {
        format!(
            "{} / {} @ {}",
            self.repo,
            self.path,
            self.commit.chars().take(7).collect::<String>()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillFile {
    pub path: String,
    pub size: u64,
    pub script: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInstall {
    pub id: Uuid,
    #[serde(default)]
    pub bot_id: Option<Uuid>,
    #[serde(default)]
    pub turn_id: Option<Uuid>,
    pub skill_id: String,
    pub name: String,
    pub description: String,
    pub reason: String,
    pub source: SkillSource,
    pub files: Vec<SkillFile>,
    pub env: Vec<String>,
    pub secret: String,
    pub status: SkillInstallStatus,
    pub scope: Option<SkillScope>,
    pub key_stored: bool,
    pub created_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    pub scope: SkillScope,
    pub bot_id: Option<Uuid>,
    pub bot_name: Option<String>,
    pub installed_by: Uuid,
    pub installed_by_name: String,
    pub installed_at: DateTime<Utc>,
    pub files: Vec<SkillFile>,
    pub env: Vec<String>,
    pub secret: String,
    pub key_stored: bool,
}

impl InstalledSkill {
    pub fn access(&self) -> String {
        match self.scope {
            SkillScope::All => "All bots".into(),
            SkillScope::Unknown => "Unknown scope".into(),
            SkillScope::Bot => self.bot_name.clone().unwrap_or_else(|| {
                self.bot_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "Installing bot".into())
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    pub skills: Vec<InstalledSkill>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillContents {
    pub skill: InstalledSkill,
    pub contents: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallReview {
    pub install: SkillInstall,
    pub contents: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub install: SkillInstall,
    pub requeued: bool,
}
