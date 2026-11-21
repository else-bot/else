use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, de};
use uuid::Uuid;

wire_state! {
    pub enum JobStatus {
        Starting = "starting",
        Running = "running",
        Cancelling = "cancelling",
        Completed = "completed",
        Failed = "failed",
        Cancelled = "cancelled",
        Lost = "lost",
    }
    _ => Other(String)
}

/// A job in an installed MCP that a waiting turn rechecks until it ends.
/// A pending wait is the only hold of its attempt. The details are absent
/// from older servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpWait {
    pub id: Uuid,
    pub mcp_id: String,
    #[serde(default)]
    pub tool: String,
    /// What the bot waits for, in its own words.
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub interval_seconds: i64,
    #[serde(default, deserialize_with = "timestamp")]
    pub deadline_at: Option<String>,
    #[serde(default)]
    pub checks: i64,
    /// The job's status in the last check, when it was readable.
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub last_result: Option<String>,
    #[serde(default, deserialize_with = "timestamp")]
    pub created_at: Option<String>,
}

/// A background job in the bot's computer that a waiting turn continues
/// after. A pending wait is the only hold of its attempt. The details are
/// absent from older servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputerJobWait {
    pub id: Uuid,
    pub job_id: Uuid,
    #[serde(default)]
    pub command: String,
    /// What the bot said the job is for. Absent for jobs started without
    /// one and from older servers.
    #[serde(default)]
    pub purpose: Option<String>,
    #[serde(default)]
    pub job_status: Option<JobStatus>,
    #[serde(default, deserialize_with = "timestamp")]
    pub created_at: Option<String>,
}

fn timestamp<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(text) = &value {
        text.parse::<DateTime<Utc>>().map_err(de::Error::custom)?;
    }
    Ok(value)
}
