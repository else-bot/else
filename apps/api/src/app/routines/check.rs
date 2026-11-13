//! A routine's check: the shell command its bot's computer runs before
//! each run, and how the check job's end decides whether the run wakes the
//! model.

use super::{CHECK_DURATION, InvalidRoutine, MAX_CHECK_BYTES};

/// A shell command the bot's computer runs before each run, trimmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineCheck(String);

impl RoutineCheck {
    pub fn parse(raw: &str) -> Result<Self, InvalidRoutine> {
        if raw.contains('\0') {
            return Err(InvalidRoutine::Nul);
        }
        let check = raw.trim();
        if check.is_empty() {
            return Err(InvalidRoutine::Check);
        }
        if check.len() > MAX_CHECK_BYTES {
            return Err(InvalidRoutine::CheckTooLarge);
        }
        Ok(Self(check.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How a routine's check job ended, read from its `guest_jobs` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckEnd {
    /// Exit 0 with only whitespace, as `echo "$found"` prints when it found
    /// nothing: nothing new.
    Clear,
    /// Exit 0 with output for the run.
    Found(String),
    Failed {
        exit_code: i32,
        output: String,
    },
    /// Stopped before it exited: at its deadline, or by a cancel other than
    /// the owner's Stop (which settles the run itself).
    Stopped(String),
    /// It ended without a readable exit status, or its computer stopped
    /// first: whether it finished is unknown.
    Unknown(String),
    /// It never held the computer, so it never ran.
    NotStarted,
}

/// What a run does once its check ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterCheck {
    Skip(CheckSkip),
    /// Queue the turn with this note after the routine's prompt.
    Wake(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckSkip {
    NothingNew,
    Cancelled,
    Unavailable,
}

impl CheckEnd {
    /// `leased` is whether the job ever held the computer.
    pub fn of(
        status: crate::computers::jobs::JobStatus,
        exit_code: Option<i32>,
        output: Option<String>,
        leased: bool,
    ) -> Option<Self> {
        use crate::computers::jobs::JobStatus;
        let output = output.unwrap_or_default();
        Some(match (status, exit_code) {
            (JobStatus::Completed, _) if output.trim().is_empty() => Self::Clear,
            (JobStatus::Completed, _) => Self::Found(output),
            (JobStatus::Failed, Some(exit_code)) => Self::Failed { exit_code, output },
            (JobStatus::Failed, None) if !leased => Self::NotStarted,
            (JobStatus::Failed, None) | (JobStatus::Lost, _) => Self::Unknown(output),
            (JobStatus::Cancelled, _) => Self::Stopped(output),
            (JobStatus::Starting | JobStatus::Running | JobStatus::Cancelling, _) => return None,
        })
    }

    pub fn after(&self) -> AfterCheck {
        let minutes = CHECK_DURATION.as_secs() / 60;
        let (head, output) = match self {
            Self::Clear => return AfterCheck::Skip(CheckSkip::NothingNew),
            Self::NotStarted => return AfterCheck::Skip(CheckSkip::Unavailable),
            Self::Found(output) => ("This routine's check found this:".to_owned(), output),
            Self::Failed { exit_code, output } => (
                format!(
                    "This routine's check failed with exit code {exit_code}. Run the task without \
                     it if you can, and tell the owner if it keeps failing. Its output:"
                ),
                output,
            ),
            Self::Stopped(output) => (
                format!(
                    "This routine's check was stopped before it finished, at its {minutes}-minute \
                     limit or by a cancel. Its output so far:"
                ),
                output,
            ),
            Self::Unknown(output) => (
                "This routine's check has an unknown outcome: it ended without reporting an exit \
                 code. Its output so far:"
                    .to_owned(),
                output,
            ),
        };
        let output = output.trim();
        let output = if output.is_empty() { "(none)" } else { output };
        AfterCheck::Wake(format!("{head}\n\n{output}"))
    }
}

/// The text a run's turn starts with: the routine's prompt, then what its
/// check reported, if it has one.
pub fn run_prompt(prompt: &str, note: Option<&str>) -> String {
    match note {
        Some(note) => format!("{prompt}\n\n---\n\n{note}"),
        None => prompt.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_check_skips_only_when_it_exits_zero_with_nothing_or_never_ran() {
        use crate::computers::jobs::JobStatus;
        let end = |status, exit, output: &str, leased| {
            CheckEnd::of(status, exit, Some(output.to_owned()), leased)
                .unwrap()
                .after()
        };
        assert_eq!(
            end(JobStatus::Completed, Some(0), "", true),
            AfterCheck::Skip(CheckSkip::NothingNew)
        );
        assert_eq!(
            end(JobStatus::Failed, None, "", false),
            AfterCheck::Skip(CheckSkip::Unavailable)
        );
        let wake = |after| match after {
            AfterCheck::Wake(note) => note,
            AfterCheck::Skip(skip) => panic!("skipped: {skip:?}"),
        };
        assert_eq!(
            end(JobStatus::Completed, Some(0), " \n", true),
            AfterCheck::Skip(CheckSkip::NothingNew),
            "a bare newline is nothing new"
        );
        let note = wake(end(JobStatus::Completed, Some(0), "3 leads\n", true));
        assert!(note.ends_with("\n\n3 leads"), "{note}");
        let note = wake(end(JobStatus::Failed, Some(2), "", true));
        assert!(
            note.contains("exit code 2") && note.ends_with("(none)"),
            "{note}"
        );
        let note = wake(end(JobStatus::Cancelled, Some(130), "half", true));
        assert!(note.contains("stopped before it finished"), "{note}");
        let note = wake(end(JobStatus::Failed, None, "new lead", true));
        assert!(
            note.contains("unknown outcome") && note.ends_with("new lead"),
            "an unreadable exit status after launch is unknown, not unstarted: {note}"
        );
        let note = wake(end(JobStatus::Lost, None, "", true));
        assert!(note.contains("unknown outcome"), "{note}");
        assert_eq!(
            CheckEnd::of(JobStatus::Running, None, None, true),
            None,
            "a running check has no end yet"
        );
        assert_eq!(run_prompt("Do it.", None), "Do it.");
        assert_eq!(
            run_prompt("Do it.", Some("Found x.")),
            "Do it.\n\n---\n\nFound x."
        );
    }

    #[test]
    fn checks_are_trimmed_and_bounded() {
        assert_eq!(
            RoutineCheck::parse("  ./a.sh \n").unwrap().as_str(),
            "./a.sh"
        );
        assert_eq!(RoutineCheck::parse(" \n"), Err(InvalidRoutine::Check));
        assert_eq!(
            RoutineCheck::parse(&"x".repeat(MAX_CHECK_BYTES + 1)),
            Err(InvalidRoutine::CheckTooLarge)
        );
    }
}
