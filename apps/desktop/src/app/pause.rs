use super::*;
use crate::api::{PauseReason, ServerPause};
use chrono::{DateTime, Utc};

#[derive(Default)]
pub(super) enum PauseStrip {
    #[default]
    Hidden,
    Backup {
        started_at: DateTime<Utc>,
        expected_secs: Option<u64>,
        _refresh: Task<()>,
    },
    Update,
}

impl PauseStrip {
    pub fn text(&self, now: DateTime<Utc>) -> Option<String> {
        match self {
            Self::Hidden => None,
            Self::Backup {
                started_at,
                expected_secs,
                ..
            } => Some(backup_text(*started_at, *expected_secs, now)),
            Self::Update => Some("Updating else.bot · messages send when it's done".into()),
        }
    }
}

fn backup_text(
    started_at: DateTime<Utc>,
    expected_secs: Option<u64>,
    now: DateTime<Utc>,
) -> String {
    let remaining = expected_secs
        .and_then(|seconds| i64::try_from(seconds).ok())
        .and_then(chrono::Duration::try_seconds)
        .and_then(|duration| started_at.checked_add_signed(duration))
        .map(|end| end.signed_duration_since(now).num_seconds())
        .filter(|seconds| *seconds > 0);
    match remaining {
        Some(seconds) => format!(
            "Backing up · about {} min left · messages send when it's done",
            seconds / 60 + i64::from(seconds % 60 != 0)
        ),
        None => "Backing up · messages send when it's done".into(),
    }
}

impl ElseApp {
    pub(super) fn set_server_pause(
        &mut self,
        pause: Option<ServerPause>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pause = match pause {
            None => PauseStrip::Hidden,
            Some(pause) if pause.reason == PauseReason::Update => PauseStrip::Update,
            Some(pause) => PauseStrip::Backup {
                started_at: pause.started_at,
                expected_secs: pause.expected_secs,
                _refresh: cx.spawn_in(window, async move |this, cx| {
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_secs(15))
                            .await;
                        if this.update(cx, |_, cx| cx.notify()).is_err() {
                            break;
                        }
                    }
                }),
            },
        };
    }
}

#[cfg(test)]
mod tests {
    use super::{PauseStrip, backup_text};
    use chrono::{DateTime, Utc};

    #[test]
    fn backup_pause_text_counts_down_and_omits_unknown_or_elapsed_estimates() {
        let started = DateTime::parse_from_rfc3339("2026-11-12T06:45:26Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            backup_text(started, Some(327), started + chrono::Duration::seconds(207)),
            "Backing up · about 2 min left · messages send when it's done"
        );
        assert_eq!(
            backup_text(started, Some(327), started + chrono::Duration::seconds(326)),
            "Backing up · about 1 min left · messages send when it's done"
        );
        for expected in [None, Some(0), Some(327), Some(u64::MAX)] {
            assert_eq!(
                backup_text(started, expected, started + chrono::Duration::seconds(400)),
                "Backing up · messages send when it's done"
            );
        }
        assert_eq!(
            backup_text(started, Some(327), started + chrono::Duration::seconds(327)),
            "Backing up · messages send when it's done"
        );
    }

    #[test]
    fn backup_pause_strip_hides_when_idle_and_names_updates() {
        assert_eq!(PauseStrip::Hidden.text(Utc::now()), None);
        assert_eq!(
            PauseStrip::Update.text(Utc::now()).as_deref(),
            Some("Updating else.bot · messages send when it's done")
        );
    }
}
