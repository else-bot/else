use super::*;
use crate::api::{PauseReason, ServerPause};
use std::time::Instant;

#[derive(Default)]
pub(super) enum PauseStrip {
    #[default]
    Hidden,
    Backup {
        /// When the server expects the pause to end, on this machine's
        /// monotonic clock, so a skewed desktop clock cannot shift it.
        ends: Option<Instant>,
        _refresh: Option<Task<()>>,
    },
    /// An update, or a pause reason this desktop does not know yet.
    Notice(&'static str),
}

impl PauseStrip {
    pub fn text(&self, now: Instant) -> Option<String> {
        match self {
            Self::Hidden => None,
            Self::Backup { ends, .. } => Some(backup_text(
                ends.map(|end| end.saturating_duration_since(now)),
            )),
            Self::Notice(text) => Some((*text).into()),
        }
    }
}

fn backup_text(remaining: Option<Duration>) -> String {
    match remaining
        .map(|left| left.as_secs())
        .filter(|secs| *secs > 0)
    {
        Some(seconds) => format!(
            "Backing up · about {} min left · messages send when it's done",
            seconds.div_ceil(60)
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
            Some(pause) if pause.reason == PauseReason::Update => {
                PauseStrip::Notice("Updating else.bot · messages send when it's done")
            }
            Some(pause) if pause.reason == PauseReason::Other => {
                PauseStrip::Notice("Work is paused · messages send when it's done")
            }
            Some(pause) => {
                let left = pause
                    .expected_secs
                    .map(|expected| expected.saturating_sub(pause.elapsed_secs))
                    .filter(|left| *left > 0)
                    .map(Duration::from_secs);
                let ends = left.map(|left| Instant::now() + left);
                // Redraw the countdown until the estimate runs out.
                let refresh = ends.map(|ends| {
                    cx.spawn_in(window, async move |this, cx| {
                        while Instant::now() < ends {
                            cx.background_executor()
                                .timer(Duration::from_secs(15))
                                .await;
                            if this.update(cx, |_, cx| cx.notify()).is_err() {
                                break;
                            }
                        }
                    })
                });
                PauseStrip::Backup {
                    ends,
                    _refresh: refresh,
                }
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::backup_text;
    use std::time::Duration;

    #[test]
    fn backup_pause_text_counts_down_and_omits_unknown_or_elapsed_estimates() {
        assert_eq!(
            backup_text(Some(Duration::from_secs(120))),
            "Backing up · about 2 min left · messages send when it's done"
        );
        assert_eq!(
            backup_text(Some(Duration::from_secs(1))),
            "Backing up · about 1 min left · messages send when it's done"
        );
        for remaining in [None, Some(Duration::ZERO), Some(Duration::from_millis(400))] {
            assert_eq!(
                backup_text(remaining),
                "Backing up · messages send when it's done"
            );
        }
    }
}
