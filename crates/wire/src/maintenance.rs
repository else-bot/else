use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize, Serializer};
use uuid::Uuid;

wire_state! {
    pub enum PauseReason {
        Backup = "backup",
        Update = "update",
    }
    /// A reason this desktop does not know yet.
    _ => Other
}

wire_state! {
    pub enum PausePhase {
        WaitingForTasks = "waiting_for_tasks",
        Computers = "computers",
        Files = "files",
        Projects = "projects",
        Database = "database",
    }
    _ => Unknown
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerPause {
    pub reason: PauseReason,
    #[serde(serialize_with = "millis")]
    pub started_at: Option<DateTime<Utc>>,
    pub expected_secs: Option<u64>,
    /// How long the pause had run when the server sent this.
    #[serde(default)]
    pub elapsed_secs: u64,
    pub phase: Option<PausePhase>,
    pub done: Option<usize>,
    pub total: Option<usize>,
}

fn millis<S: Serializer>(at: &Option<DateTime<Utc>>, serializer: S) -> Result<S::Ok, S::Error> {
    at.map(|at| at.to_rfc3339_opts(SecondsFormat::Millis, true))
        .serialize(serializer)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub version: Option<String>,
    pub active_turns: u64,
    pub running_commands: u64,
    pub queued_turns: Option<u64>,
    pub background_jobs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    pub unavailable: Option<String>,
    pub operation: Option<RemoteOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteOperation {
    pub request_id: Uuid,
    pub version: String,
    pub state: RemoteState,
    pub output: String,
    pub error: Option<String>,
}

wire_state! {
    pub enum RemoteState {
        Running = "running",
        Succeeded = "succeeded",
        Failed = "failed",
    }
    _ => Unknown
}
