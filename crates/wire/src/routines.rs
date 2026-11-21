use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::settings::ProfileChoice;
use crate::turns::{TurnError, TurnStatus};

wire_state! {
    /// Where a routine's last due run is: awaiting its check, skipped, ended
    /// with no news, or its turn's status with live turns as `queued` or
    /// `running`. A newer server's status keeps its text so it can be shown.
    pub enum RunStatus {
        Checking = "checking",
        Queued = "queued",
        Running = "running",
        Skipped = "skipped",
        NoNews = "no_news",
        Completed = "completed",
        Interrupted = "interrupted",
        Failed = "failed",
        Uncertain = "uncertain",
    }
    _ => Other(String)
}

wire_state! {
    /// Why a routine's due run did not start a turn.
    pub enum SkipReason {
        Overlap = "overlap",
        QueueFull = "queue_full",
        ProviderUnavailable = "provider_unavailable",
        NothingNew = "nothing_new",
        CheckCancelled = "check_cancelled",
        CheckUnavailable = "check_unavailable",
    }
    _ => Unknown
}

wire_state! {
    /// Why else.bot switched a routine off.
    pub enum PausedReason {
        /// Its last failure needs the owner; retrying won't fix it.
        NeedsAction = "needs_action",
        /// It failed several runs in a row.
        Repeated = "repeated",
    }
    _ => Unknown
}

/// The occurrence a turn runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineRun {
    pub routine_id: Uuid,
    pub name: String,
    pub due_at: DateTime<Utc>,
    /// The routine is a reminder, due once. Absent from older servers.
    #[serde(default)]
    pub reminder: bool,
    /// The routine lets a run end with no news.
    #[serde(default)]
    pub quiet: bool,
    /// The run completed with nothing to report, so it alerts no one.
    #[serde(default)]
    pub no_news: bool,
}

impl RoutineRun {
    /// What started the run, as the conversation labels it.
    pub fn label(&self) -> &'static str {
        if self.reminder {
            "Reminder"
        } else {
            "Scheduled routine"
        }
    }
}

/// The last due occurrence of a routine and the result of its check or turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastRun {
    pub due_at: DateTime<Utc>,
    pub status: RunStatus,
    pub skip_reason: Option<SkipReason>,
    pub turn_id: Option<Uuid>,
    pub error: Option<TurnError>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// The days (`mon` to `sun`) and the span, end excluded, when a routine's
/// runs may start, in its time zone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveHours {
    pub days: Vec<String>,
    pub start: String,
    pub end: String,
}

/// A bot's saved task, run on a cron schedule in its conversation, or
/// once at `run_at` for a reminder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routine {
    pub id: Uuid,
    #[serde(default)]
    pub bot_id: Uuid,
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub prompt_revision: i64,
    pub check: Option<String>,
    pub cron: String,
    /// When a reminder runs once; `None` for a routine on a cron schedule.
    #[serde(default)]
    pub run_at: Option<DateTime<Utc>>,
    pub timezone: String,
    /// When runs may start; `None` is any time.
    #[serde(default)]
    pub active_hours: Option<ActiveHours>,
    pub enabled: bool,
    #[serde(default)]
    pub model: Option<ProfileChoice>,
    #[serde(default)]
    pub requested_model: Option<String>,
    pub next_run_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_run: Option<LastRun>,
    /// Consecutive failed runs the owner has not dismissed.
    #[serde(default)]
    pub failed_runs: i64,
    /// Why else.bot switched the routine off.
    #[serde(default)]
    pub paused_reason: Option<PausedReason>,
    /// The last run is the owner's retry of the paused routine, not yet
    /// settled.
    #[serde(default)]
    pub retrying: bool,
    /// A run with nothing to report may end with no news, alerting no one.
    #[serde(default)]
    pub quiet: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineEnvelope {
    pub routine: Routine,
    /// Set on the retry route. False when a repeated request returns its run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routines {
    pub routines: Vec<Routine>,
}

/// A bot's webhook. An authenticated call wakes the bot with `prompt` at
/// most as often as its pace allows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Webhook {
    pub id: Uuid,
    #[serde(default)]
    pub bot_id: Uuid,
    pub name: String,
    pub prompt: String,
    pub min_interval_minutes: i64,
    #[serde(default)]
    pub daily_limit: i64,
    pub enabled: bool,
    pub requested_at: Option<DateTime<Utc>>,
    pub wake_not_before: Option<DateTime<Utc>>,
    #[serde(default)]
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub updated_at: DateTime<Utc>,
    pub last_wake: Option<WebhookWake>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookEnvelope {
    pub webhook: Webhook,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookWake {
    pub woke_at: DateTime<Utc>,
    pub status: Option<TurnStatus>,
    pub turn_id: Option<Uuid>,
}

wire_state! {
    pub enum WatcherStatus {
        Waiting = "waiting",
        Checking = "checking",
        Waking = "waking",
        Ended = "ended",
    }
    _ => Unknown
}

wire_state! {
    pub enum WatcherOutcome {
        Met = "met",
        Failed = "failed",
        Expired = "expired",
        Stopped = "stopped",
    }
    _ => Unknown
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watcher {
    pub id: Uuid,
    pub purpose: String,
    #[serde(default)]
    pub check: String,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub every_minutes: i64,
    pub until: DateTime<Utc>,
    pub status: WatcherStatus,
    pub outcome: Option<WatcherOutcome>,
    pub last_status: Option<String>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub next_check_at: Option<DateTime<Utc>>,
    pub last_output: Option<String>,
    #[serde(default)]
    pub checks: i64,
    #[serde(default)]
    pub failures: i64,
    #[serde(default)]
    pub created_at: DateTime<Utc>,
    pub created_turn_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatcherEnvelope {
    pub watcher: Watcher,
}
