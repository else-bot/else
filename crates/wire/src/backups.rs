use crate::maintenance::ServerPause;
use serde::{Deserialize, Serialize};

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

wire_state! {
    pub enum BackupService {
        R2 = "r2",
        S3 = "s3",
        Compatible = "compatible",
    }
    _ => Other(String)
}

impl BackupService {
    pub fn label(&self) -> &str {
        match self {
            Self::R2 => "Cloudflare R2",
            Self::S3 => "Amazon S3",
            Self::Compatible => "S3 compatible",
            Self::Other(service) => service,
        }
    }
}

wire_state! {
    pub enum LocalBackupStatus {
        Running = "running",
        Completed = "completed",
        /// Completed without every computer.
        Partial = "partial",
        Failed = "failed",
    }
    _ => Unknown
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupDestination {
    pub service: BackupService,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    pub path_style: bool,
    pub has_secret: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupEstimate {
    pub bytes: u64,
    pub basis: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupRun {
    pub set_id: String,
    pub finished_at: Option<String>,
    /// The encrypted size of the whole set.
    pub bytes: Option<u64>,
    /// What its remote run wrote to the bucket: only the chunks the bucket
    /// lacked. Absent for sets replicated before incremental uploads.
    pub uploaded_bytes: Option<u64>,
    pub local_status: LocalBackupStatus,
    pub remote_status: Option<RemoteBackupStatus>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupSnapshot {
    pub revision: i64,
    pub enabled: bool,
    pub interval_secs: u64,
    pub keep: usize,
    pub destination: Option<BackupDestination>,
    pub running: bool,
    pub next_backup_at: Option<String>,
    pub estimate: Option<BackupEstimate>,
    pub recent: Vec<BackupRun>,
    pub remote_count: usize,
    /// Bytes stored under the destination's prefix after the last
    /// retention, or the sum of the kept sets' sizes before one was measured.
    pub remote_bytes: u64,
    pub unavailable_reason: Option<String>,
    pub last_error: Option<String>,
    /// Absent from servers that predate the daily local schedule.
    #[serde(default)]
    pub local: Option<LocalBackupSettings>,
    #[serde(default)]
    pub pause: Option<ServerPause>,
}

/// The daily local backup on this server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalBackupSettings {
    pub enabled: bool,
    /// `HH:MM` on the server's clock.
    pub time: String,
    pub timezone: String,
    pub utc_offset_secs: i32,
    pub keep: usize,
    /// An environment variable sets the schedule; the owner cannot change it.
    pub managed_by_env: bool,
    pub next_at: Option<String>,
    pub copies: usize,
    pub bytes: u64,
    pub free_bytes: Option<u64>,
    pub recent: Vec<LocalBackupRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalBackupRun {
    pub set_id: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: LocalBackupStatus,
    pub bytes: Option<u64>,
    pub took_secs: Option<u64>,
    pub paused_secs: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tested {
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupAccepted {
    pub accepted: bool,
}
