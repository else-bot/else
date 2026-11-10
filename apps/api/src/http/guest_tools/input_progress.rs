use serde::Deserialize;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum InputProgress {
    NotStarted,
    Started { next_index: usize },
    Completed,
    Refused { error: String },
    Failed { done: usize, error: String },
    ProtocolViolation { done: usize, reason: &'static str },
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum Event {
    Started { started: bool },
    Done { done: usize },
    Ok { ok: bool, actions: usize },
    Error { error: String },
}

impl InputProgress {
    pub(super) fn parse(stdout: &str, actions: usize) -> Self {
        let mut progress = Self::NotStarted;
        for line in stdout.lines() {
            let Ok(event) = serde_json::from_str(line) else {
                return Self::ProtocolViolation {
                    done: progress.done(actions),
                    reason: "INPUT_PROTOCOL: malformed input helper event",
                };
            };
            progress = progress.advance(event, actions);
            if matches!(progress, Self::ProtocolViolation { .. }) {
                break;
            }
        }
        progress
    }

    fn advance(self, event: Event, actions: usize) -> Self {
        let done = self.done(actions);
        match (self, event) {
            (Self::NotStarted, Event::Started { started: true }) => Self::Started { next_index: 0 },
            (Self::Started { next_index }, Event::Done { done })
                if done == next_index && done < actions =>
            {
                Self::Started {
                    next_index: next_index + 1,
                }
            }
            (
                Self::Started { next_index },
                Event::Ok {
                    ok: true,
                    actions: reported,
                },
            ) if next_index == actions && reported == actions => Self::Completed,
            (Self::NotStarted, Event::Error { error }) => Self::Refused { error },
            (Self::Started { .. }, Event::Error { error }) => Self::Failed { done, error },
            _ => Self::ProtocolViolation {
                done,
                reason: "INPUT_PROTOCOL: out-of-order or inconsistent input helper event",
            },
        }
    }

    pub(super) fn done(&self, actions: usize) -> usize {
        match self {
            Self::NotStarted | Self::Refused { .. } => 0,
            Self::Started { next_index } => *next_index,
            Self::Completed => actions,
            Self::Failed { done, .. } | Self::ProtocolViolation { done, .. } => *done,
        }
    }

    pub(super) fn error(&self) -> Option<&str> {
        match self {
            Self::Refused { error } | Self::Failed { error, .. } => Some(error),
            Self::ProtocolViolation { reason, .. } => Some(reason),
            Self::NotStarted | Self::Started { .. } | Self::Completed => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STARTED: &str = r#"{"started":true}"#;
    const DONE_0: &str = r#"{"done":0}"#;
    const DONE_1: &str = r#"{"done":1}"#;
    const OK: &str = r#"{"ok":true,"actions":2}"#;
    const ERROR: &str = r#"{"error":"STOPPED"}"#;

    fn parse(events: &[&str]) -> InputProgress {
        InputProgress::parse(&events.join("\n"), 2)
    }

    #[test]
    fn helper_transcripts_preserve_success_refusal_failure_and_cutoffs() {
        assert_eq!(parse(&[]), InputProgress::NotStarted);
        assert_eq!(parse(&[STARTED]), InputProgress::Started { next_index: 0 });
        assert_eq!(
            parse(&[STARTED, DONE_0]),
            InputProgress::Started { next_index: 1 }
        );
        assert_eq!(
            parse(&[STARTED, DONE_0, DONE_1]),
            InputProgress::Started { next_index: 2 }
        );
        assert_eq!(
            parse(&[STARTED, DONE_0, DONE_1, OK]),
            InputProgress::Completed
        );
        assert_eq!(
            parse(&[ERROR]),
            InputProgress::Refused {
                error: "STOPPED".into()
            }
        );
        for (events, done) in [
            (vec![STARTED, ERROR], 0),
            (vec![STARTED, DONE_0, ERROR], 1),
            (vec![STARTED, DONE_0, DONE_1, ERROR], 2),
        ] {
            assert_eq!(
                parse(&events),
                InputProgress::Failed {
                    done,
                    error: "STOPPED".into()
                }
            );
        }
    }

    #[test]
    fn invalid_events_never_complete_or_erase_confirmed_progress() {
        let cases: &[(&[&str], usize)] = &[
            (&[DONE_0, STARTED, DONE_1, OK], 0),
            (&[OK], 0),
            (&[STARTED, STARTED], 0),
            (&[STARTED, DONE_1], 0),
            (&[STARTED, r#"{"done":2}"#], 0),
            (&[STARTED, OK], 0),
            (&[STARTED, DONE_0, DONE_0, OK], 1),
            (&[STARTED, DONE_0, r#"{"done":99}"#, OK], 1),
            (&[STARTED, DONE_0, OK], 1),
            (&[STARTED, DONE_0, r#"{"done":1,"ok":true}"#], 1),
            (&[STARTED, DONE_0, DONE_1, r#"{"done":2}"#], 2),
            (&[STARTED, DONE_0, DONE_1, OK, DONE_1], 2),
            (&[STARTED, DONE_0, DONE_1, OK, OK], 2),
            (&[STARTED, DONE_0, DONE_1, OK, ERROR], 2),
            (&[ERROR, STARTED], 0),
            (&[ERROR, OK], 0),
            (&[STARTED, DONE_0, ERROR, OK], 1),
            (&[STARTED, DONE_0, ERROR, DONE_1], 1),
        ];
        for &(events, done) in cases {
            let progress = parse(events);
            assert!(
                matches!(progress, InputProgress::ProtocolViolation { .. }),
                "{events:?}: {progress:?}"
            );
            assert_eq!(progress.done(2), done, "{events:?}");
            assert!(progress.error().unwrap().starts_with("INPUT_PROTOCOL"));
        }
    }

    #[test]
    fn malformed_events_cannot_be_skipped() {
        for event in [
            "not JSON",
            "{}",
            "null",
            r#"{"done":null}"#,
            r#"{"done":-1}"#,
            r#"{"done":1.0}"#,
            r#"{"done":"1"}"#,
            r#"{"started":false}"#,
            r#"{"ok":false,"actions":2}"#,
            r#"{"ok":true}"#,
            r#"{"ok":true,"actions":1}"#,
            r#"{"error":null}"#,
            r#"{"ok":true,"actions":2,"error":"STOPPED"}"#,
        ] {
            let progress = parse(&[STARTED, DONE_0, DONE_1, event, OK]);
            assert!(
                matches!(progress, InputProgress::ProtocolViolation { .. }),
                "{event}: {progress:?}"
            );
            assert_eq!(progress.done(2), 2, "{event}");
        }
    }
}
