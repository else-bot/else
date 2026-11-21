//! Where an attempt leaves its turn, decided from facts alone.

use super::Unwind;
use crate::app::providers::{FailureCode, TurnOutcome};
use crate::db::limit_cuts::Cut;
use crate::db::turns::TurnStatus;

/// What decides how an attempt leaves its turn.
#[derive(Debug, Clone)]
pub struct Facts<'a> {
    pub outcome: &'a TurnOutcome,
    /// How the turn unwinds, when it is `cancelling`.
    pub cancel: Option<Unwind>,
    pub holds: Holds,
    /// An owner message joined the reply and was not taken yet.
    pub joined: bool,
    /// What the attempt limit did, when it cut this attempt.
    pub cut: Option<Cut>,
}

/// Counts over the turn's `turn_holds` and its delegations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Holds {
    pub pending: i64,
    /// Pending holds on the owner.
    pub blocking: i64,
    pub tasks_unseen: i64,
    pub waits_unseen: i64,
    pub owner_unseen: i64,
    /// Settled task batches ([`crate::db::delegations::READY`]).
    pub ready: i64,
}

/// Where an attempt leaves its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// It continues in a new attempt.
    Queued,
    /// It parks until its holds settle.
    Waiting,
    Ended(End),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct End {
    pub status: TurnStatus,
    pub reason: Reason,
}

/// The `error_code` and `error_message` an ended turn records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The cancel's own reason stays.
    Kept,
    Set {
        code: Option<String>,
        message: Option<String>,
    },
}

/// Whether the attempt limit decides this attempt's end: it cut the
/// attempt and the owner did not stop the turn first.
pub fn limit_cut(outcome: &TurnOutcome, cancel: Option<Unwind>) -> bool {
    is_deadline(outcome) && cancel.is_none()
}

fn is_deadline(outcome: &TurnOutcome) -> bool {
    matches!(
        outcome,
        TurnOutcome::Failed {
            code: FailureCode::TurnDeadline,
            ..
        }
    )
}

/// Decide where an attempt with `facts` leaves its turn. A waiting turn
/// continues by the rule of the `turns_continuable` view.
pub fn settle(facts: &Facts) -> Disposition {
    let Holds {
        pending,
        blocking,
        tasks_unseen,
        waits_unseen,
        owner_unseen,
        ready,
    } = facts.holds;
    let (status, code, message) = match facts.outcome {
        TurnOutcome::Completed => (TurnStatus::Completed, None, None),
        TurnOutcome::Interrupted => (TurnStatus::Interrupted, None, None),
        TurnOutcome::Failed { code, message } => (
            TurnStatus::Failed,
            Some(code.as_str()),
            Some(message.clone()),
        ),
        TurnOutcome::Uncertain { message } => (
            TurnStatus::Uncertain,
            Some("delivery_uncertain"),
            Some(message.clone()),
        ),
    };
    let calm = matches!(status, TurnStatus::Completed | TurnStatus::Interrupted);
    // The owner stopped only this attempt: the turn parks on its jobs and
    // tasks, or continues with their outcomes. What the attempt itself left
    // unseen (a withdrawn question) restarts nothing. With nothing to park
    // on, the attempt's own outcome stands.
    let kept = facts.cancel == Some(Unwind::KeepWaits)
        && (calm || pending > 0 || waits_unseen + tasks_unseen > 0);
    let owner_unseen = if kept { 0 } else { owner_unseen };
    let unseen = tasks_unseen + waits_unseen + owner_unseen;
    // An attempt the limit cut continues in a new one, unless a cap is
    // reached. The owner's stop wins: the cut is the interruption it asked
    // for.
    let stopped = is_deadline(facts.outcome) && facts.cancel.is_some();
    let continued = facts.cut == Some(Cut::Continued);
    let (status, code, message) = if kept || continued || stopped {
        (TurnStatus::Interrupted, None, None)
    } else {
        let capped = match facts.cut {
            Some(Cut::Capped { continued }) => {
                Some(crate::providers::cli::attempt::limit_message(continued))
            }
            _ => None,
        };
        (status, code, capped.or(message))
    };
    let parks = (kept || facts.cancel.is_none())
        && matches!(status, TurnStatus::Completed | TurnStatus::Interrupted);
    // Pending tasks and waits hold the turn without blocking a ready batch
    // or any other unseen outcome, as `turns_continuable` decides for a
    // waiting turn. A joined owner message continues the reply now,
    // whatever it waits on.
    let own_outcome = ready > 0 || waits_unseen + owner_unseen > 0;
    if parks && (facts.joined || (blocking == 0 && own_outcome)) {
        Disposition::Queued
    } else if parks && pending > 0 {
        Disposition::Waiting
    } else if parks && (unseen > 0 || continued) {
        Disposition::Queued
    } else if facts.cancel.is_some() && status == TurnStatus::Interrupted {
        Disposition::Ended(End {
            status,
            reason: Reason::Kept,
        })
    } else {
        Disposition::Ended(End {
            status,
            reason: Reason::Set {
                code: code.map(str::to_owned),
                message,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Disposition, End, Facts, Holds, Reason};
    use crate::app::providers::{FailureCode, TurnOutcome};
    use crate::db::limit_cuts::Cut;
    use crate::db::turns::TurnStatus;
    use crate::db::turns::lifecycle::Unwind;

    const LIMIT_AFTER_TWO: &str = "Stopped at the 30-minute attempt limit after continuing 2 times. Background jobs it started may still be running.";

    fn ended(status: TurnStatus, code: Option<&str>, message: Option<&str>) -> Disposition {
        Disposition::Ended(End {
            status,
            reason: Reason::Set {
                code: code.map(str::to_owned),
                message: message.map(str::to_owned),
            },
        })
    }

    fn kept(status: TurnStatus) -> Disposition {
        Disposition::Ended(End {
            status,
            reason: Reason::Kept,
        })
    }

    #[test]
    fn settle_decides_each_attempt_end() {
        let completed = TurnOutcome::Completed;
        let interrupted = TurnOutcome::Interrupted;
        let failed = TurnOutcome::failed(FailureCode::ProviderFailed, "boom");
        let deadline = TurnOutcome::failed(FailureCode::TurnDeadline, "cut");
        let uncertain = TurnOutcome::Uncertain {
            message: "lost".into(),
        };
        let task = Holds {
            pending: 1,
            ..Holds::default()
        };
        let cases: Vec<(&str, Facts, Disposition)> = vec![
            (
                "a finished attempt with nothing held ends completed",
                Facts {
                    outcome: &completed,
                    cancel: None,
                    holds: Holds::default(),
                    joined: false,
                    cut: None,
                },
                ended(TurnStatus::Completed, None, None),
            ),
            (
                "a pending task parks the turn",
                Facts {
                    outcome: &completed,
                    cancel: None,
                    holds: task,
                    joined: false,
                    cut: None,
                },
                Disposition::Waiting,
            ),
            (
                "a settled wait continues it while a task runs",
                Facts {
                    outcome: &completed,
                    cancel: None,
                    holds: Holds {
                        waits_unseen: 1,
                        ..task
                    },
                    joined: false,
                    cut: None,
                },
                Disposition::Queued,
            ),
            (
                "a pending owner card blocks that continuation",
                Facts {
                    outcome: &completed,
                    cancel: None,
                    holds: Holds {
                        blocking: 1,
                        waits_unseen: 1,
                        ..task
                    },
                    joined: false,
                    cut: None,
                },
                Disposition::Waiting,
            ),
            (
                "a joined owner message continues past a pending card",
                Facts {
                    outcome: &completed,
                    cancel: None,
                    holds: Holds {
                        blocking: 1,
                        ..task
                    },
                    joined: true,
                    cut: None,
                },
                Disposition::Queued,
            ),
            (
                "an unseen task outcome continues the turn",
                Facts {
                    outcome: &completed,
                    cancel: None,
                    holds: Holds {
                        tasks_unseen: 1,
                        ..Holds::default()
                    },
                    joined: false,
                    cut: None,
                },
                Disposition::Queued,
            ),
            (
                "a failure ends the turn with its own reason",
                Facts {
                    outcome: &failed,
                    cancel: None,
                    holds: task,
                    joined: false,
                    cut: None,
                },
                ended(TurnStatus::Failed, Some("provider_failed"), Some("boom")),
            ),
            (
                "an uncertain delivery ends uncertain",
                Facts {
                    outcome: &uncertain,
                    cancel: None,
                    holds: Holds::default(),
                    joined: false,
                    cut: None,
                },
                ended(
                    TurnStatus::Uncertain,
                    Some("delivery_uncertain"),
                    Some("lost"),
                ),
            ),
            (
                "a limit cut under the cap continues",
                Facts {
                    outcome: &deadline,
                    cancel: None,
                    holds: Holds::default(),
                    joined: false,
                    cut: Some(Cut::Continued),
                },
                Disposition::Queued,
            ),
            (
                "a limit cut at the cap fails with the limit message",
                Facts {
                    outcome: &deadline,
                    cancel: None,
                    holds: Holds::default(),
                    joined: false,
                    cut: Some(Cut::Capped { continued: 2 }),
                },
                ended(
                    TurnStatus::Failed,
                    Some("turn_deadline"),
                    Some(LIMIT_AFTER_TWO),
                ),
            ),
            (
                "a limit cut during a cancel keeps the cancel's reason",
                Facts {
                    outcome: &deadline,
                    cancel: Some(Unwind::EndAll),
                    holds: Holds::default(),
                    joined: false,
                    cut: None,
                },
                kept(TurnStatus::Interrupted),
            ),
            (
                "the owner's Stop parks the turn on its task",
                Facts {
                    outcome: &interrupted,
                    cancel: Some(Unwind::KeepWaits),
                    holds: task,
                    joined: false,
                    cut: None,
                },
                Disposition::Waiting,
            ),
            (
                "the owner's Stop with a withdrawn question only ends interrupted",
                Facts {
                    outcome: &completed,
                    cancel: Some(Unwind::KeepWaits),
                    holds: Holds {
                        owner_unseen: 1,
                        ..Holds::default()
                    },
                    joined: false,
                    cut: None,
                },
                kept(TurnStatus::Interrupted),
            ),
            (
                "a failure during the owner's Stop with nothing held stands",
                Facts {
                    outcome: &failed,
                    cancel: Some(Unwind::KeepWaits),
                    holds: Holds::default(),
                    joined: false,
                    cut: None,
                },
                ended(TurnStatus::Failed, Some("provider_failed"), Some("boom")),
            ),
            (
                "a full cancel ends the turn whatever it waits on",
                Facts {
                    outcome: &interrupted,
                    cancel: Some(Unwind::EndAll),
                    holds: Holds {
                        tasks_unseen: 1,
                        ..task
                    },
                    joined: true,
                    cut: None,
                },
                kept(TurnStatus::Interrupted),
            ),
            (
                "a completion during a full cancel stands",
                Facts {
                    outcome: &completed,
                    cancel: Some(Unwind::EndAll),
                    holds: Holds::default(),
                    joined: false,
                    cut: None,
                },
                ended(TurnStatus::Completed, None, None),
            ),
        ];
        for (name, facts, expected) in cases {
            assert_eq!(super::settle(&facts), expected, "{name}");
        }
    }
}
