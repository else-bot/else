//! What happens after a routine run ends.
//!
//! A run that fails for a reason that may pass (the provider was briefly
//! unreachable, else.bot restarted) is retried once at once. If the retry
//! fails too, the owner is told and the next scheduled run tries again. A
//! run that fails for a reason retrying cannot fix (a policy violation,
//! invalid model settings, a reply else.bot cannot read), or a routine whose scheduled
//! runs keep failing, is paused and the owner is told why. An uncertain run
//! may have done part of its work, so it is never retried automatically.

use chrono::{DateTime, Utc};

/// Consecutive failed occurrences after which a routine is paused.
pub const MAX_FAILED_RUNS: i64 = 3;

/// How a routine run's turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended<'a> {
    Completed,
    /// Stopped by the owner or a deletion.
    Stopped,
    Failed {
        code: &'a str,
    },
    Uncertain,
}

impl<'a> Ended<'a> {
    /// `None` while the turn has not ended.
    pub fn of(status: &str, code: Option<&'a str>) -> Option<Self> {
        match status {
            "completed" => Some(Self::Completed),
            "interrupted" => Some(Self::Stopped),
            "failed" => Some(Self::Failed {
                code: code.unwrap_or_default(),
            }),
            "uncertain" => Some(Self::Uncertain),
            _ => None,
        }
    }
}

/// Whether running again can help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    Transient,
    NeedsAction,
}

struct Code {
    code: &'static str,
    cause: Cause,
    explanation: &'static str,
}

const CODES: &[Code] = &[
    Code {
        code: "provider_unavailable",
        cause: Cause::Transient,
        explanation: "the AI provider could not be reached or stopped unexpectedly",
    },
    Code {
        code: "provider_failed",
        cause: Cause::Transient,
        explanation: "the AI provider reported an error",
    },
    Code {
        code: "turn_deadline",
        cause: Cause::Transient,
        explanation: "it ran past its time limit",
    },
    Code {
        code: "start_failed",
        cause: Cause::Transient,
        explanation: "else.bot could not start the run",
    },
    Code {
        code: "api_restarted",
        cause: Cause::Transient,
        explanation: "else.bot restarted while it ran",
    },
    Code {
        code: "orphaned",
        cause: Cause::Transient,
        explanation: "else.bot lost track of the run",
    },
    Code {
        code: "provider_refused",
        // Covers overload and usage-limit rejections as well as refusals.
        cause: Cause::Transient,
        explanation: "the AI provider refused the request",
    },
    Code {
        code: "provider_protocol",
        cause: Cause::NeedsAction,
        explanation: "the AI provider sent a reply else.bot could not understand",
    },
    Code {
        code: "policy_violation",
        cause: Cause::NeedsAction,
        explanation: "it tried to use a tool it is not allowed to use",
    },
    Code {
        code: "invalid_profile",
        cause: Cause::NeedsAction,
        explanation: "its model settings are no longer valid",
    },
    Code {
        code: "profile_mismatch",
        cause: Cause::NeedsAction,
        explanation: "the AI provider ran a different model than the one chosen",
    },
    Code {
        code: "provider_misconfigured",
        cause: Cause::NeedsAction,
        explanation: "its model settings are missing",
    },
    Code {
        code: "attachment_unavailable",
        cause: Cause::NeedsAction,
        explanation: "its files could not be prepared",
    },
];

fn known(code: &str) -> Option<&'static Code> {
    CODES.iter().find(|known| known.code == code)
}

/// Unknown codes count as transient: the streak limit still pauses a
/// routine that keeps failing.
pub fn cause(code: &str) -> Cause {
    known(code).map_or(Cause::Transient, |known| known.cause)
}

fn explanation(code: &str) -> &'static str {
    known(code).map_or("an unexpected error happened", |known| known.explanation)
}

/// The routine as the ended run found it.
#[derive(Debug, Clone, Copy)]
pub struct Run {
    /// The run was itself an automatic retry.
    pub retry: bool,
    pub enabled: bool,
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
}

pub fn react(ended: Ended<'_>, run: Run) -> Reaction {
    let code = match ended {
        Ended::Completed => return Reaction::Reset,
        Ended::Stopped => return Reaction::Ignore,
        Ended::Failed { code } => Some(code),
        Ended::Uncertain => None,
    };
    if !run.enabled {
        return Reaction::Report;
    }
    if code.is_some_and(|code| cause(code) == Cause::NeedsAction) {
        return Reaction::Pause(Pause::NeedsAction);
    }
    if code.is_some() && !run.retry {
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
}

/// The bot's message about a failed run; `reaction` is `Report` or `Pause`.
pub fn notice(notice: &Notice<'_>, reaction: Reaction) -> String {
    let name = notice.name;
    let (what, error) = match notice.ended {
        Ended::Failed { code } => (
            explanation(code),
            match notice.message {
                Some(message) if !message.is_empty() => format!("`{code}`: {message}"),
                _ => format!("`{code}`"),
            },
        ),
        _ => (
            "it ended in an uncertain state, so part of its work may already be done",
            notice.message.unwrap_or("uncertain").to_owned(),
        ),
    };
    let fix = "Ask me to look into it. Once it is fixed, turn the routine back on with its \
               switch in this bot's settings.";
    let body = match reaction {
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
                (_, false) => {
                    "The routine is switched off, so it won't run again until you turn it back on."
                        .to_owned()
                }
                (Some(next), true) => format!("I'll try again at the next scheduled run, {next}."),
                (None, true) => "It has no further scheduled runs.".to_owned(),
            };
            format!("The routine **{name}** failed because {what}. {retry} {next}")
        }
    };
    format!("{body}\n\nError: {error}")
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
        retry: false,
        enabled: true,
        failed_runs: 0,
    };
    const CLOSED: Ended<'static> = Ended::Failed {
        code: "provider_unavailable",
    };

    #[test]
    fn a_transient_failure_retries_once_then_waits_then_pauses() {
        assert_eq!(react(CLOSED, FRESH), Reaction::Retry);
        let retry = Run {
            retry: true,
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
        for code in ["provider_protocol", "policy_violation", "invalid_profile"] {
            assert_eq!(
                react(Ended::Failed { code }, FRESH),
                Reaction::Pause(Pause::NeedsAction),
                "{code}"
            );
        }
        assert_eq!(cause("something_new"), Cause::Transient);
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
                    code: "provider_refused"
                },
                off
            ),
            Reaction::Report
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
        };
        let text = notice(&base, Reaction::Report);
        assert!(text.starts_with("The routine **Update tickets** failed because the AI provider"));
        assert!(text.contains("I retried it right away and it failed again."));
        assert!(text.contains("next scheduled run, Sun 8 Nov, 04:00."));
        assert!(text.ends_with("Error: `provider_unavailable`: the provider process exited"));
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
