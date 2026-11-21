use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

wire_state! {
    /// What the runtime last observed of a bot's computer.
    pub enum RuntimeState {
        Absent = "absent",
        Creating = "creating",
        Starting = "starting",
        Running = "running",
        Pausing = "pausing",
        Paused = "paused",
        Resuming = "resuming",
        Stopping = "stopping",
        Stopped = "stopped",
        Missing = "missing",
        Error = "error",
    }
    _ => Unknown
}

wire_state! {
    /// Whether the owner let the computer start on demand or stopped it.
    pub enum DesiredState {
        Auto = "auto",
        Stopped = "stopped",
    }
    _ => Unknown
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Computers {
    pub latest_image: Option<String>,
    pub rollback_bytes: Option<u64>,
    pub computers: Vec<Computer>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Computer {
    pub bot_id: Uuid,
    pub state: RuntimeState,
    pub desired_state: DesiredState,
    pub image: String,
    pub latest_image: Option<String>,
    pub upgrade: Option<ComputerUpgrade>,
    pub disk_used_bytes: Option<u64>,
    pub disk_size_bytes: Option<u64>,
    pub cpus: u32,
    pub cpu_percent: Option<f64>,
    pub memory_used_bytes: Option<u64>,
    pub memory_limit_bytes: u64,
    pub last_used_at: Option<DateTime<Utc>>,
}

impl Computer {
    pub fn outdated(&self) -> bool {
        self.latest_image
            .as_ref()
            .is_some_and(|latest| *latest != self.image)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedComputers {
    pub queued: Vec<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelledComputers {
    pub cancelled: Vec<Uuid>,
}

/// The status fields sent even before a computer is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputerStateHeader {
    pub bot_id: Option<Uuid>,
    pub state: RuntimeState,
    pub desired_state: DesiredState,
    #[serde(default)]
    pub desktop_available: bool,
    #[serde(default)]
    pub default_memory_mib: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputerState {
    #[serde(flatten)]
    pub status: ComputerStateHeader,
    pub runtime_name: Option<String>,
    pub cpus: Option<i32>,
    pub active_leases: Option<i64>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub latest_image: Option<String>,
    #[serde(default)]
    pub upgrade: Option<ComputerUpgrade>,
    #[serde(default)]
    pub memory_mib: Option<u32>,
}

impl ComputerState {
    /// The memory the owner approved above the configured size, and that size.
    pub fn raised_memory(&self) -> Option<(u32, u32)> {
        match (self.memory_mib, self.status.default_memory_mib) {
            (Some(memory), Some(default)) if memory > default => Some((memory, default)),
            _ => None,
        }
    }

    pub fn outdated(&self) -> bool {
        matches!((&self.image, &self.latest_image), (Some(image), Some(latest)) if image != latest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ComputerUpgrade {
    Queued {
        position: usize,
    },
    Waiting {
        reason: String,
    },
    Running,
    Failed {
        error: String,
    },
    Completed {
        unrestored_packages: Vec<String>,
    },
    #[serde(other)]
    Unknown,
}

/// The viewer and server negotiate either read-only or control access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopMode {
    Watch,
    Control,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    Size {
        width: u16,
        height: u16,
    },
    Mode {
        mode: DesktopMode,
    },
    Cursor {
        style: String,
    },
    Error {
        #[serde(default)]
        code: String,
        message: String,
    },
    /// A status message a newer server added.
    #[serde(other)]
    Unknown,
}

/// A bot's request that the owner take over its computer. Its turn waits
/// until the owner hands the computer back or declines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputerHandoff {
    pub id: Uuid,
    pub bot_id: Option<Uuid>,
    pub turn_id: Option<Uuid>,
    pub reason: String,
    pub status: HandoffStatus,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub taken_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub settled_at: Option<DateTime<Utc>>,
}

wire_state! {
    pub enum HandoffStatus {
        Pending = "pending",
        /// The owner controls the computer and has not handed it back.
        Taken = "taken",
        HandedBack = "handed_back",
        Dismissed = "dismissed",
        Skipped = "skipped",
        Cancelled = "cancelled",
    }
    _ => Unknown
}

impl HandoffStatus {
    pub fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::Taken)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettledHandoff {
    pub handoff: ComputerHandoff,
}
