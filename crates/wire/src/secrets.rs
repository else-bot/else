use std::{collections::BTreeMap, fmt};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

wire_state! {
    pub enum SecretRequestStatus {
        Pending = "pending",
        Stored = "stored",
        Cancelled = "cancelled",
        Skipped = "skipped",
    }
    _ => Unknown
}

/// A bot's request for credentials. It holds metadata only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRequest {
    pub id: Uuid,
    #[serde(default)]
    pub bot_id: Option<Uuid>,
    #[serde(default)]
    pub turn_id: Option<Uuid>,
    pub name: String,
    pub reason: String,
    pub env: Vec<String>,
    pub description: Option<String>,
    pub website: Option<String>,
    pub status: SecretRequestStatus,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub settled_at: Option<DateTime<Utc>>,
}

impl SecretRequest {
    pub fn is_pending(&self) -> bool {
        self.status == SecretRequestStatus::Pending
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultEntry {
    pub name: String,
    pub description: String,
    pub website: Option<String>,
    pub env: Vec<String>,
    pub updated_at: DateTime<Utc>,
    pub last_used: Option<VaultUse>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultUse {
    pub bot_id: Uuid,
    pub bot_name: Option<String>,
    pub r#use: VaultUseKind,
    pub at: DateTime<Utc>,
}

wire_state! {
    pub enum VaultUseKind {
        Command = "command",
        Job = "job",
        Browser = "browser",
    }
    _ => Other(String)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct List {
    pub secrets: Vec<VaultEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSecret {
    pub name: String,
    pub env: Vec<String>,
    pub not_redacted: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevealedSecret<T = String> {
    pub env: BTreeMap<String, T>,
}

impl<T> fmt::Debug for RevealedSecret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RevealedSecret([redacted])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub request: SecretRequest,
    pub requeued: bool,
}
