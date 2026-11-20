//! Watcher rules: a bot's check that else.bot runs in its computer every
//! few minutes until it reports what the bot waits for, keeps failing, is
//! stopped, or its deadline passes. Each check is a short background job,
//! so a watcher holds no process between checks and survives restarts of
//! the computer and of else.bot. When a watcher ends, its bot is woken
//! once in its owner conversation.
//!
//! A check follows the routine check convention: exit 0 printing nothing
//! means not yet, exit 0 printing something means found. Lines starting
//! with `STATUS:` are the watcher's status for the owner and never count
//! as output.

use std::time::Duration;

use super::routines::CheckEnd;

/// Active watchers one bot may have at once.
pub const MAX_PER_BOT: i64 = 4;
pub const MIN_EVERY_MINUTES: i64 = 5;
pub const MAX_EVERY_MINUTES: i64 = 24 * 60;
pub const DEFAULT_EVERY_MINUTES: i64 = 15;
/// The furthest deadline: 30 days.
pub const MAX_HOURS: i64 = 30 * 24;
/// How long one check may run before it is stopped.
pub const CHECK_DURATION: Duration = Duration::from_secs(10 * 60);
/// Failed checks in a row that end a watcher.
pub const MAX_FAILURES: i64 = 3;
const STATUS_PREFIX: &str = "STATUS:";
const MAX_STATUS_CHARS: usize = 200;

/// Where a watcher is in its life, as stored in `watchers.status`.
/// `waking` has ended and still has to queue its bot's wake; `ended` has
/// queued it or needs none.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    sqlx::Type,
    strum::IntoStaticStr,
    strum::Display,
)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Status {
    Waiting,
    Checking,
    Waking,
    Ended,
}

impl Status {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Waiting | Self::Checking)
    }
}

/// How a watcher ended, as stored in `watchers.outcome`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    sqlx::Type,
    strum::IntoStaticStr,
    strum::Display,
)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Outcome {
    /// A check reported what the bot waits for.
    Met,
    /// [`MAX_FAILURES`] checks in a row failed.
    Failed,
    /// Its deadline passed first.
    Expired,
    Stopped,
}

/// What one ended check does to its watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    /// The owner-facing status: the check's last `STATUS:` line, or a note.
    pub status: String,
    /// The check's output with its `STATUS:` lines removed.
    pub output: String,
    /// Failed checks in a row, this one included.
    pub failures: i64,
    /// The check ran, whatever it reported.
    pub ran: bool,
    pub ends: Option<Outcome>,
}

/// The last `STATUS:` line of `output`, and the rest of it.
fn split_status(output: &str) -> (Option<String>, String) {
    let mut status = None;
    let mut rest = Vec::new();
    for line in output.lines() {
        match line.trim_start().strip_prefix(STATUS_PREFIX) {
            Some(text) => status = Some(text.trim().chars().take(MAX_STATUS_CHARS).collect()),
            None => rest.push(line),
        }
    }
    (status.filter(|s: &String| !s.is_empty()), rest.join("\n"))
}

/// Decide what a check that ended as `job` reports, given the failed
/// checks in a row before it. A check that did not run, or whose outcome is
/// unknown, neither fails nor clears the streak.
pub fn checked(
    job: crate::computers::jobs::JobStatus,
    exit_code: Option<i32>,
    output: Option<String>,
    leased: bool,
    failures: i64,
) -> Option<Checked> {
    let (status, output) = split_status(output.as_deref().unwrap_or_default());
    let end = CheckEnd::of(job, exit_code, Some(output.clone()), leased)?;
    let failed = |note: String| {
        let failures = failures + 1;
        Checked {
            status: note,
            output: output.clone(),
            failures,
            ran: true,
            ends: (failures >= MAX_FAILURES).then_some(Outcome::Failed),
        }
    };
    Some(match end {
        CheckEnd::Clear => Checked {
            status: status.unwrap_or_else(|| "not yet".into()),
            output: output.clone(),
            failures: 0,
            ran: true,
            ends: None,
        },
        CheckEnd::Found(_) => Checked {
            status: status.unwrap_or_else(|| "found".into()),
            output: output.clone(),
            failures: 0,
            ran: true,
            ends: Some(Outcome::Met),
        },
        CheckEnd::Failed { exit_code, .. } => {
            failed(status.unwrap_or_else(|| format!("check failed with exit code {exit_code}")))
        }
        CheckEnd::Stopped(_) => {
            failed(status.unwrap_or_else(|| "check was stopped before it finished".into()))
        }
        CheckEnd::Unknown(_) => Checked {
            status: "check outcome unknown: it ended without an exit code".into(),
            output: output.clone(),
            failures,
            ran: true,
            ends: None,
        },
        CheckEnd::NotStarted => Checked {
            status: "check did not run: the computer was not available".into(),
            output: output.clone(),
            failures,
            ran: false,
            ends: None,
        },
    })
}

/// A check that could not run because the Vault entry `name` it needs was
/// deleted. It counts as failed, like a check that exits non-zero.
pub fn missing_secret(name: &str, failures: i64) -> Checked {
    let failures = failures + 1;
    Checked {
        status: format!(
            "check failed: the Vault entry {name} was deleted; save it again or stop this watcher"
        ),
        output: String::new(),
        failures,
        ran: true,
        ends: (failures >= MAX_FAILURES).then_some(Outcome::Failed),
    }
}

/// The hidden task text of a watcher's wake: what the bot planned, then
/// what the watcher last saw.
pub fn wake_text(prompt: &str, last_status: Option<&str>, last_output: Option<&str>) -> String {
    let status = last_status.unwrap_or("not checked yet");
    let output = last_output.map(str::trim).filter(|o| !o.is_empty());
    format!(
        "{prompt}\n\n---\n\nLast status: {status}\nLast check output:\n\n{}",
        output.unwrap_or("(none)")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computers::jobs::JobStatus;

    fn check(job: JobStatus, exit: Option<i32>, output: &str, failures: i64) -> Checked {
        checked(job, exit, Some(output.to_owned()), true, failures).unwrap()
    }

    #[test]
    fn status_lines_report_without_counting_as_found() {
        let c = check(
            JobStatus::Completed,
            Some(0),
            "STATUS: first\n  STATUS: example.com is taken\n",
            1,
        );
        assert_eq!(c.status, "example.com is taken");
        assert_eq!((c.ends, c.failures), (None, 0));
        let found = check(
            JobStatus::Completed,
            Some(0),
            "STATUS: available\nexample.com is free",
            0,
        );
        assert_eq!(found.ends, Some(Outcome::Met));
        assert_eq!(found.output, "example.com is free");
    }

    #[test]
    fn the_third_failure_in_a_row_ends_the_watcher() {
        let second = check(JobStatus::Failed, Some(6), "curl: could not resolve", 1);
        assert_eq!((second.failures, second.ends), (2, None));
        assert_eq!(second.status, "check failed with exit code 6");
        let third = check(JobStatus::Cancelled, Some(130), "", 2);
        assert_eq!((third.failures, third.ends), (3, Some(Outcome::Failed)));
    }

    #[test]
    fn a_check_that_never_ran_or_is_unknown_keeps_the_streak() {
        let unrun = checked(JobStatus::Failed, None, None, false, 2).unwrap();
        assert_eq!((unrun.failures, unrun.ran, unrun.ends), (2, false, None));
        let lost = check(JobStatus::Lost, None, "", 2);
        assert_eq!((lost.failures, lost.ran, lost.ends), (2, true, None));
        assert!(checked(JobStatus::Running, None, None, true, 0).is_none());
    }
}
