//! What happens after a routine run ends.
//!
//! A run that fails for a reason that may pass (the provider was briefly
//! unreachable, else.bot restarted) is retried once at once. If the retry
//! fails too, the owner is told and the next scheduled run tries again. A
//! run that fails for a reason retrying cannot fix (a policy violation,
//! invalid model settings, a reply else.bot cannot read), or a routine whose scheduled
//! runs keep failing, is paused and the owner is told why. An uncertain run
//! may have done part of its work, so it is never retried automatically.
//! The owner can retry a paused routine: a success switches it back on, and
//! a failure keeps it paused with the new reason.

use chrono::{DateTime, Utc};

use crate::app::providers::FailureCode;
use crate::db::turns::TurnStatus;

const UNEXPECTED: &str = "an unexpected error happened";

/// Consecutive failed occurrences after which a routine is paused.
pub const MAX_FAILED_RUNS: i64 = 3;

/// How a routine run's turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended<'a> {
    Completed,
    /// Stopped by the owner or a deletion.
    Stopped,
    /// `code` is `None` when the turn recorded none.
    Failed {
        code: Option<&'a FailureCode>,
    },
    Uncertain,
}

impl<'a> Ended<'a> {
    /// `None` while the turn has not ended.
    pub fn of(status: TurnStatus, code: Option<&'a FailureCode>) -> Option<Self> {
        match status {
            TurnStatus::Completed => Some(Self::Completed),
            TurnStatus::Interrupted => Some(Self::Stopped),
            TurnStatus::Failed => Some(Self::Failed { code }),
            TurnStatus::Uncertain => Some(Self::Uncertain),
            TurnStatus::Queued
            | TurnStatus::Preparing
            | TurnStatus::Sending
            | TurnStatus::Running
            | TurnStatus::Cancelling
            | TurnStatus::Waiting => None,
        }
    }
}

/// Whether running again can help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    Transient,
    NeedsAction,
}

impl FailureCode {
    pub fn cause(&self) -> Cause {
        match self {
            Self::ProviderUnavailable
            | Self::ProviderFailed
            | Self::TurnDeadline
            | Self::StartFailed
            | Self::ApiRestarted
            | Self::Orphaned
            | Self::ProviderRefused => Cause::Transient,
            Self::ProviderProtocol
            | Self::PolicyViolation
            | Self::InvalidProfile
            | Self::ProfileMismatch
            | Self::ProviderMisconfigured
            | Self::AttachmentUnavailable => Cause::NeedsAction,
            // The streak limit still pauses a routine that keeps failing.
            Self::Other(_) => Cause::Transient,
        }
    }

    fn explanation(&self) -> &'static str {
        match self {
            Self::ProviderUnavailable => {
                "the AI provider could not be reached or stopped unexpectedly"
            }
            Self::ProviderFailed => "the AI provider reported an error",
            Self::TurnDeadline => "it ran past its time limit",
            Self::StartFailed => "else.bot could not start the run",
            Self::ApiRestarted => "else.bot restarted while it ran",
            Self::Orphaned => "else.bot lost track of the run",
            Self::ProviderRefused => "the AI provider refused the request",
            Self::ProviderProtocol => "the AI provider sent a reply else.bot could not understand",
            Self::PolicyViolation => "it tried to use a tool it is not allowed to use",
            Self::InvalidProfile => "its model settings are no longer valid",
            Self::ProfileMismatch => "the AI provider ran a different model than the one chosen",
            Self::ProviderMisconfigured => "its model settings are missing",
            Self::AttachmentUnavailable => "its files could not be prepared",
            Self::Other(_) => UNEXPECTED,
        }
    }
}

/// Why a run started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// A scheduled run, or one the bot started on demand.
    First,
    /// An automatic retry of a failed run.
    Retry,
    /// The owner's retry of a failed or paused routine; a success switches
    /// a paused routine back on.
    Resume,
}

/// The routine as the ended run found it.
#[derive(Debug, Clone, Copy)]
pub struct Run {
    pub attempt: Attempt,
    pub enabled: bool,
    /// else.bot switched the routine off and the owner has not switched it
    /// on, off, or dismissed its failures since.
    pub paused: bool,
    /// Consecutive failed occurrences before this one.
    pub failed_runs: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pause {
    NeedsAction,
    Repeated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaction {
    /// Forget earlier failures.
    Reset,
    Ignore,
    /// Run again now, without telling the owner yet.
    Retry,
    /// Count the failure and tell the owner; the next scheduled run tries
    /// again.
    Report,
    /// Count the failure, switch the routine off, and tell the owner.
    Pause(Pause),
    /// Forget earlier failures and switch the paused routine back on.
    Resume,
    /// The owner's retry failed: count it, keep the routine paused with
    /// this reason, and tell the owner.
    StayPaused(Pause),
}

pub fn react(ended: Ended<'_>, run: Run) -> Reaction {
    // A routine the owner switched on, off, or dismissed during the retry
    // keeps the owner's choice.
    let resumes = run.attempt == Attempt::Resume && run.paused;
    let (failed, needs_action) = match ended {
        Ended::Completed if resumes => return Reaction::Resume,
        Ended::Completed => return Reaction::Reset,
        Ended::Stopped => return Reaction::Ignore,
        Ended::Failed { code } => (
            true,
            code.is_some_and(|code| code.cause() == Cause::NeedsAction),
        ),
        Ended::Uncertain => (false, false),
    };
    if resumes {
        return Reaction::StayPaused(if needs_action {
            Pause::NeedsAction
        } else {
            Pause::Repeated
        });
    }
    if !run.enabled {
        return Reaction::Report;
    }
    if needs_action {
        return Reaction::Pause(Pause::NeedsAction);
    }
    if failed && run.attempt == Attempt::First {
        return Reaction::Retry;
    }
    if run.failed_runs + 1 >= MAX_FAILED_RUNS {
        return Reaction::Pause(Pause::Repeated);
    }
    Reaction::Report
}

/// What the owner is told about a failed run.
pub struct Notice<'a> {
    pub name: &'a str,
    pub ended: Ended<'a>,
    pub message: Option<&'a str>,
    /// The failed run was an automatic retry.
    pub retried: bool,
    pub enabled: bool,
    /// Consecutive failed occurrences, this one included.
    pub failed_runs: i64,
    /// The next scheduled run, in the routine's time zone.
    pub next: Option<String>,
    /// The routine is a reminder, due once.
    pub reminder: bool,
}

/// The bot's message about a failed run; `reaction` is `Report`, `Pause`, or
/// `StayPaused`.
pub fn notice(notice: &Notice<'_>, reaction: Reaction) -> String {
    let name = notice.name;
    let (what, error) = match notice.ended {
        Ended::Failed { code } => (
            code.map_or(UNEXPECTED, FailureCode::explanation),
            match (code, notice.message.filter(|m| !m.is_empty())) {
                (Some(code), Some(message)) => format!("`{code}`: {message}"),
                (Some(code), None) => format!("`{code}`"),
                (None, Some(message)) => message.to_owned(),
                (None, None) => "no error was recorded".to_owned(),
            },
        ),
        _ => (
            "it ended in an uncertain state, so part of its work may already be done",
            notice.message.unwrap_or("uncertain").to_owned(),
        ),
    };
    let (kind, fix) = if notice.reminder {
        (
            "reminder",
            "Ask me to look into it and run the reminder again.",
        )
    } else {
        (
            "routine",
            "Ask me to look into it. Once it is fixed, turn the routine back on with its switch \
             in this bot's settings.",
        )
    };
    let body = match reaction {
        Reaction::StayPaused(_) => format!(
            "Your retry of the {kind} **{name}** failed because {what}, so it stays paused. Ask \
             me to look into it, then retry it from this bot's settings."
        ),
        Reaction::Pause(Pause::NeedsAction) if notice.reminder => format!(
            "The reminder **{name}** failed because {what}. Retrying won't fix this: its \
             instructions, its model, or else.bot itself needs a change first. {fix}"
        ),
        Reaction::Pause(Pause::NeedsAction) => format!(
            "I paused the routine **{name}** because {what}. Retrying won't fix this: the \
             routine's instructions, its model, or else.bot itself needs a change first. {fix}"
        ),
        Reaction::Pause(Pause::Repeated) => format!(
            "I paused the routine **{name}** after {} failed runs in a row. The last one failed \
             because {what}. {fix}",
            notice.failed_runs
        ),
        _ => {
            let retry = match notice.ended {
                Ended::Uncertain => "I didn't retry it, to avoid doing that work twice.",
                _ if notice.retried => "I retried it right away and it failed again.",
                _ => "I couldn't retry it right away.",
            };
            let next = match (&notice.next, notice.enabled) {
                (Some(next), true) if notice.reminder => {
                    format!("It still runs at its set time, {next}.")
                }
                _ if notice.reminder => "It won't run again unless you ask.".to_owned(),
                (_, false) => {
                    "The routine is switched off, so it won't run again until you turn it back on."
                        .to_owned()
                }
                (Some(next), true) => format!("I'll try again at the next scheduled run, {next}."),
                (None, true) => "It has no further scheduled runs.".to_owned(),
            };
            format!("The {kind} **{name}** failed because {what}. {retry} {next}")
        }
    };
    format!("{body}\n\nError: {error}")
}

/// The bot's message about a run it gave up reviewing: the run's outcome
/// was neither retried nor counted.
pub fn review_failed_notice(name: &str, reminder: bool) -> String {
    let kind = if reminder { "reminder" } else { "routine" };
    format!(
        "I couldn't finish checking how the {kind} **{name}** went, so I didn't retry it or \
         count it as a failure. If this keeps happening, ask me to look into it."
    )
}

/// `at` in `timezone`, as the desktop shows times ("Sun 8 Nov, 04:00").
pub fn local_time(at: DateTime<Utc>, timezone: &str) -> String {
    match timezone.parse::<chrono_tz::Tz>() {
        Ok(tz) => at.with_timezone(&tz).format("%a %-d %b, %H:%M").to_string(),
        Err(_) => at.format("%a %-d %b, %H:%M UTC").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRESH: Run = Run {
        attempt: Attempt::First,
        enabled: true,
        paused: false,
        failed_runs: 0,
    };
    const CLOSED: Ended<'static> = Ended::Failed {
        code: Some(&FailureCode::ProviderUnavailable),
    };

    #[test]
    fn a_transient_failure_retries_once_then_waits_then_pauses() {
        assert_eq!(react(CLOSED, FRESH), Reaction::Retry);
        let retry = Run {
            attempt: Attempt::Retry,
            ..FRESH
        };
        assert_eq!(react(CLOSED, retry), Reaction::Report);
        assert_eq!(
            react(
                CLOSED,
                Run {
                    failed_runs: MAX_FAILED_RUNS - 2,
                    ..retry
                }
            ),
            Reaction::Report
        );
        assert_eq!(
            react(
                CLOSED,
                Run {
                    failed_runs: MAX_FAILED_RUNS - 1,
                    ..retry
                }
            ),
            Reaction::Pause(Pause::Repeated)
        );
    }

    #[test]
    fn an_error_retrying_cannot_fix_pauses_at_once() {
        for code in [
            FailureCode::ProviderProtocol,
            FailureCode::PolicyViolation,
            FailureCode::InvalidProfile,
        ] {
            assert_eq!(
                react(Ended::Failed { code: Some(&code) }, FRESH),
                Reaction::Pause(Pause::NeedsAction),
                "{code}"
            );
        }
        let unknown = FailureCode::decode("something_new");
        assert_eq!(
            react(
                Ended::Failed {
                    code: Some(&unknown)
                },
                FRESH
            ),
            Reaction::Retry,
            "a code an older build stored stays transient"
        );
    }

    #[test]
    fn uncertain_runs_are_never_retried_and_stopped_runs_change_nothing() {
        assert_eq!(react(Ended::Uncertain, FRESH), Reaction::Report);
        assert_eq!(react(Ended::Stopped, FRESH), Reaction::Ignore);
        assert_eq!(react(Ended::Completed, FRESH), Reaction::Reset);
        let off = Run {
            enabled: false,
            ..FRESH
        };
        assert_eq!(react(CLOSED, off), Reaction::Report);
        assert_eq!(
            react(
                Ended::Failed {
                    code: Some(&FailureCode::ProviderRefused)
                },
                off
            ),
            Reaction::Report
        );
    }

    #[test]
    fn an_owner_retry_resumes_on_success_and_otherwise_stays_paused() {
        let paused = Run {
            attempt: Attempt::Resume,
            enabled: false,
            paused: true,
            failed_runs: 1,
        };
        assert_eq!(react(Ended::Completed, paused), Reaction::Resume);
        assert_eq!(
            react(
                Ended::Failed {
                    code: Some(&FailureCode::PolicyViolation)
                },
                paused
            ),
            Reaction::StayPaused(Pause::NeedsAction)
        );
        assert_eq!(react(CLOSED, paused), Reaction::StayPaused(Pause::Repeated));
        assert_eq!(
            react(Ended::Uncertain, paused),
            Reaction::StayPaused(Pause::Repeated)
        );
        assert_eq!(react(Ended::Stopped, paused), Reaction::Ignore);
        let switched_on = Run {
            enabled: true,
            paused: false,
            ..paused
        };
        assert_eq!(react(Ended::Completed, switched_on), Reaction::Reset);
        assert_eq!(
            react(CLOSED, switched_on),
            Reaction::Report,
            "never retried automatically"
        );
        let switched_off = Run {
            paused: false,
            ..paused
        };
        assert_eq!(
            react(Ended::Completed, switched_off),
            Reaction::Reset,
            "the owner's switch stays off"
        );
        assert_eq!(react(CLOSED, switched_off), Reaction::Report);
    }

    #[test]
    fn a_failure_without_a_recorded_code_is_transient_and_reported_as_unexpected() {
        let ended = Ended::of(TurnStatus::Failed, None).unwrap();
        assert_eq!(react(ended, FRESH), Reaction::Retry);
        let base = Notice {
            name: "Update tickets",
            ended,
            message: None,
            retried: true,
            enabled: true,
            failed_runs: 1,
            next: None,
            reminder: false,
        };
        let text = notice(&base, Reaction::Report);
        assert!(
            text.contains("failed because an unexpected error happened"),
            "{text}"
        );
        assert!(text.ends_with("Error: no error was recorded"), "{text}");
        let explained = notice(
            &Notice {
                message: Some("the provider process exited"),
                ..base
            },
            Reaction::Report,
        );
        assert!(
            explained.ends_with("Error: the provider process exited"),
            "{explained}"
        );
    }

    #[test]
    fn notices_say_what_failed_and_what_happens_next() {
        let base = Notice {
            name: "Update tickets",
            ended: CLOSED,
            message: Some("the provider process exited"),
            retried: true,
            enabled: true,
            failed_runs: 1,
            next: Some("Sun 8 Nov, 04:00".into()),
            reminder: false,
        };
        let text = notice(&base, Reaction::Report);
        assert!(text.starts_with("The routine **Update tickets** failed because the AI provider"));
        assert!(text.contains("I retried it right away and it failed again."));
        assert!(text.contains("next scheduled run, Sun 8 Nov, 04:00."));
        assert!(text.ends_with("Error: `provider_unavailable`: the provider process exited"));
        let once = notice(
            &Notice {
                reminder: true,
                next: None,
                ..base
            },
            Reaction::Report,
        );
        assert!(once.starts_with("The reminder **Update tickets** failed because"));
        let early = notice(
            &Notice {
                reminder: true,
                next: base.next.clone(),
                ..base
            },
            Reaction::Report,
        );
        assert!(
            early.contains("It still runs at its set time, Sun 8 Nov, 04:00."),
            "{early}"
        );
        assert!(
            once.contains("It won't run again unless you ask."),
            "{once}"
        );
        let paused = notice(
            &Notice {
                failed_runs: 3,
                next: base.next.clone(),
                ..base
            },
            Reaction::Pause(Pause::Repeated),
        );
        assert!(paused.starts_with("I paused the routine **Update tickets** after 3 failed runs"));
        assert!(paused.contains("turn the routine back on"));
        let uncertain = notice(
            &Notice {
                ended: Ended::Uncertain,
                message: None,
                ..base
            },
            Reaction::Report,
        );
        assert!(uncertain.contains("I didn't retry it"), "{uncertain}");
    }

    #[test]
    fn times_are_local_to_the_routine() {
        let at = DateTime::parse_from_rfc3339("2026-11-08T03:00:00Z")
            .unwrap()
            .to_utc();
        assert_eq!(local_time(at, "Europe/Paris"), "Sun 8 Nov, 04:00");
        assert_eq!(local_time(at, "nowhere"), "Sun 8 Nov, 03:00 UTC");
    }
}
