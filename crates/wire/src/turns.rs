use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::computers::ComputerHandoff;
use crate::delegations::Delegation;
use crate::email::TurnMail;
use crate::items::Reaction;
use crate::notices::OwnerNotice;
use crate::outgoing::{DraftStatus, OutgoingDraft};
use crate::questions::Question;
use crate::routines::RoutineRun;
use crate::secrets::SecretRequest;
use crate::skills::{SkillInstall, SkillInstallStatus};
use crate::socials::SocialSetup;
use crate::waits::{ComputerJobWait, McpWait};

wire_state! {
    pub enum TurnStatus {
        Queued = "queued",
        Preparing = "preparing",
        Sending = "sending",
        Running = "running",
        Cancelling = "cancelling",
        Waiting = "waiting",
        Completed = "completed",
        Interrupted = "interrupted",
        Failed = "failed",
        Uncertain = "uncertain",
    }
    _ => Unknown
}

impl TurnStatus {
    /// Not yet ended: queued, parked, or holding the bot's active slot.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Self::Queued
                | Self::Preparing
                | Self::Sending
                | Self::Running
                | Self::Cancelling
                | Self::Waiting
        )
    }

    /// A provider is working on it now.
    pub fn is_working(self) -> bool {
        matches!(self, Self::Preparing | Self::Sending | Self::Running)
    }

    /// Ended without completing.
    pub fn is_unsuccessful(self) -> bool {
        matches!(self, Self::Interrupted | Self::Failed | Self::Uncertain)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnError {
    pub code: String,
    pub message: Option<String>,
}

wire_state! {
    /// How far a joined message got into the reply it joined.
    pub enum SteerStatus {
        Pending = "pending",
        Sending = "sending",
        /// The reply took it and answers it.
        Delivered = "delivered",
    }
    _ => Unknown
}

/// How often the attempt limit cut a turn that continued, of how many
/// continuations a request may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LimitContinued {
    pub count: i64,
    pub max: i64,
    pub minutes: i64,
}

/// One owner request and the bot's work on it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Turn {
    pub id: Uuid,
    /// The desktop does not read it.
    #[serde(default)]
    pub bot_id: Option<Uuid>,
    pub request_id: Uuid,
    pub turn_no: i64,
    /// The turn's latest provider attempt. Absent from older servers.
    #[serde(default)]
    pub attempt: Option<i64>,
    /// Set once the attempt limit cut the turn and it continued. Absent
    /// from older servers.
    #[serde(default)]
    pub limit_continued: Option<LimitContinued>,
    pub provider_id: String,
    /// The desktop does not read it.
    #[serde(default)]
    pub profile_revision: Option<i64>,
    pub status: TurnStatus,
    pub error: Option<TurnError>,
    pub effect_uncertain: bool,
    /// The desktop does not read it.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "crate::items::known_reaction")]
    pub reaction: Option<Reaction>,
    /// The running reply this message joined. Absent from older servers.
    #[serde(default)]
    pub steered_into: Option<Uuid>,
    /// How far this message got into that reply.
    #[serde(default)]
    pub steer_status: Option<SteerStatus>,
    #[serde(default)]
    pub secret_requests: Vec<SecretRequest>,
    #[serde(default)]
    pub social_setups: Vec<SocialSetup>,
    #[serde(default)]
    pub skill_installs: Vec<SkillInstall>,
    /// Absent from servers without owner questions.
    #[serde(default)]
    pub questions: Vec<Question>,
    #[serde(default)]
    pub routine: Option<RoutineRun>,
    /// The emails whose arrival started this turn. Absent from older servers.
    #[serde(default)]
    pub mail: Vec<TurnMail>,
    #[serde(default)]
    pub outgoing_drafts: Vec<OutgoingDraft>,
    /// Tasks this turn handed to other bots. Absent from older servers.
    #[serde(default)]
    pub delegations: Vec<Delegation>,
    /// MCP jobs this turn waits on. Absent from older servers.
    #[serde(default)]
    pub mcp_waits: Vec<McpWait>,
    /// Background computer jobs this turn waits on. Absent from older servers.
    #[serde(default)]
    pub computer_job_waits: Vec<ComputerJobWait>,
    /// Requests that the owner take over the computer. Absent from older servers.
    #[serde(default)]
    pub computer_handoffs: Vec<ComputerHandoff>,
    #[serde(default)]
    pub owner_notices: Vec<OwnerNotice>,
    /// The owner may run this failed message again. Absent from older
    /// servers.
    #[serde(default)]
    pub retryable: bool,
}

impl Turn {
    /// The reply this message joined took it, so that reply answers it.
    pub fn taken_by_reply(&self) -> bool {
        self.steer_status == Some(SteerStatus::Delivered)
    }

    /// Whether the turn waits on work of its own rather than on the owner.
    pub fn waits_on_job(&self) -> bool {
        !self.mcp_waits.is_empty() || !self.computer_job_waits.is_empty()
    }

    /// The tasks it handed to other bots that have not ended.
    pub fn open_delegations(&self) -> Vec<&Delegation> {
        self.delegations
            .iter()
            .filter(|d| !d.status.settled())
            .collect()
    }

    /// Whether the owner holds it: a pending skill install, secret, draft,
    /// question, or account setup.
    pub fn waits_on_owner(&self) -> bool {
        self.skill_installs
            .iter()
            .any(|r| r.status == SkillInstallStatus::Pending)
            || self.secret_requests.iter().any(|r| r.is_pending())
            || self
                .outgoing_drafts
                .iter()
                .any(|d| d.status == DraftStatus::Pending)
            || self.questions.iter().any(|q| q.is_pending())
            || self.social_setups.iter().any(|setup| setup.is_pending())
    }

    /// Parked only on tasks it handed to other bots.
    pub fn waits_on_bots(&self) -> bool {
        self.status == TurnStatus::Waiting
            && !self.waits_on_job()
            && !self.waits_on_owner()
            && !self.open_delegations().is_empty()
    }
}
