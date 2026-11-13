use super::*;
use crate::api::{LocalBackupSettings, LocalBackupStatus, PauseReason};
use chrono::{DateTime, FixedOffset, Offset as _, Utc};
use gpui_kit::component::{Theme, h_flex};
use view::{message, metric, metrics};

fn server_timestamp(value: &str, timezone: &str, offset: i32) -> String {
    match DateTime::parse_from_rfc3339(value) {
        Ok(at) => server_time(at.to_utc(), timezone, offset, "%b %-d, %H:%M"),
        Err(_) => value.into(),
    }
}

/// Format an instant on the server's wall clock: its IANA zone when this
/// desktop knows it, otherwise the offset the server reported.
fn server_time(at: DateTime<Utc>, timezone: &str, offset: i32, format: &str) -> String {
    if let Ok(zone) = timezone.parse::<chrono_tz::Tz>() {
        at.with_timezone(&zone).format(format).to_string()
    } else {
        at.with_timezone(&FixedOffset::east_opt(offset).unwrap_or_else(|| Utc.fix()))
            .format(format)
            .to_string()
    }
}

fn duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3600, seconds % 3600 / 60)
    }
}

/// Time until the next backup, to the minute: the panel redraws every few
/// seconds, so a seconds countdown would jitter.
fn countdown(seconds: i64) -> String {
    let minutes = (seconds.max(0) as u64).div_ceil(60);
    match minutes {
        0 => "Due now".into(),
        1..60 => format!("In {minutes} min"),
        _ => format!("In {}h {}m", minutes / 60, minutes % 60),
    }
}

fn result(status: LocalBackupStatus, theme: &Theme) -> (&'static str, Hsla) {
    match status {
        LocalBackupStatus::Running => ("Running", theme.warning),
        LocalBackupStatus::Completed => ("Complete", theme.success),
        LocalBackupStatus::Partial => ("Partial", theme.warning),
        LocalBackupStatus::Failed => ("Failed", theme.danger),
        LocalBackupStatus::Unknown => ("Unknown", theme.muted_foreground),
    }
}

impl Shell {
    pub(super) fn render_local_backups(
        &self,
        view: &View,
        snapshot: &BackupSnapshot,
        local: &LocalBackupSettings,
        stacked: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = cx.theme();
        let stamp = |value: &str| server_timestamp(value, &local.timezone, local.utc_offset_secs);
        let running = snapshot
            .pause
            .as_ref()
            .is_some_and(|pause| pause.reason == PauseReason::Backup);
        let next = local
            .next_at
            .as_deref()
            .map(stamp)
            .unwrap_or_else(|| "Not scheduled".into());
        let next_detail = local
            .next_at
            .as_deref()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| countdown(at.signed_duration_since(Utc::now()).num_seconds()))
            .filter(|_| !running)
            .unwrap_or_else(|| {
                if running {
                    "Running now".into()
                } else if local.enabled {
                    local.timezone.clone()
                } else {
                    "Automatic backups off".into()
                }
            });
        let latest = local.recent.iter().find(|run| {
            matches!(
                run.status,
                LocalBackupStatus::Completed | LocalBackupStatus::Partial
            )
        });
        let last_detail = latest
            .map(|run| {
                let size = run
                    .bytes
                    .map(byte_size)
                    .unwrap_or_else(|| "Size unknown".into());
                run.took_secs
                    .map(|seconds| format!("{size} · took {}", duration(seconds)))
                    .unwrap_or(size)
            })
            .unwrap_or_else(|| "After your first local backup".into());
        let disk = match local.free_bytes {
            Some(free) => format!(
                "{} on disk · {} free",
                byte_size(local.bytes),
                byte_size(free)
            ),
            None => format!("{} on disk · free space unknown", byte_size(local.bytes)),
        };
        let mut history = v_flex().gap_3().child(
            div()
                .text_base()
                .font_weight(FontWeight::SEMIBOLD)
                .child("Recent local backups"),
        );
        if local.recent.is_empty() {
            history = history.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("No local backups yet."),
            );
        } else {
            let cell = |text: String| div().flex_1().min_w_0().text_xs().child(text);
            history = history.child(
                h_flex()
                    .gap_3()
                    .text_color(theme.muted_foreground)
                    .child(cell("Started".into()))
                    .child(cell("Size".into()))
                    .child(cell("Took".into()))
                    .child(cell("Paused work".into()))
                    .child(cell("Result".into())),
            );
            for run in &local.recent {
                let (label, tone) = result(run.status, theme);
                history = history.child(
                    v_flex()
                        .gap_1()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            h_flex()
                                .gap_3()
                                .items_center()
                                .child(cell(stamp(&run.started_at)))
                                .child(cell(run.bytes.map(byte_size).unwrap_or_else(|| "—".into())))
                                .child(cell(
                                    run.took_secs.map(duration).unwrap_or_else(|| "—".into()),
                                ))
                                .child(cell(
                                    run.paused_secs.map(duration).unwrap_or_else(|| "—".into()),
                                ))
                                .child(
                                    div().flex_1().min_w_0().child(
                                        div()
                                            .text_xs()
                                            .px_2()
                                            .py_1()
                                            .rounded_full()
                                            .bg(tone.opacity(0.1))
                                            .text_color(tone)
                                            .child(label),
                                    ),
                                ),
                        )
                        .when_some(run.error.clone(), |row, error| {
                            row.child(message(error, theme.warning, theme))
                        }),
                );
            }
        }
        // Three groups, 32 px apart: the summary, the schedule, and what is
        // kept. The summary's figures already say whether a backup is
        // running or scheduled, so no status line repeats them.
        let summary = v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_3()
                    .items_baseline()
                    .justify_between()
                    .when(stacked, |row| row.flex_col().items_start())
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("On this server"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "{} · server time {}",
                                local.timezone,
                                server_time(
                                    Utc::now(),
                                    &local.timezone,
                                    local.utc_offset_secs,
                                    "%H:%M"
                                )
                            )),
                    ),
            )
            .child(metrics(
                [
                    metric(
                        "Next backup",
                        if running { "In progress".into() } else { next },
                        next_detail,
                        theme,
                    ),
                    metric(
                        "Last backup",
                        latest
                            .map(|run| stamp(run.finished_at.as_deref().unwrap_or(&run.started_at)))
                            .unwrap_or_else(|| "None yet".into()),
                        last_detail,
                        theme,
                    ),
                    metric(
                        "Local copies",
                        format!("{} of {}", local.copies, local.keep),
                        disk,
                        theme,
                    ),
                ],
                stacked,
                theme,
            ));
        v_flex()
            .gap_6()
            .child(summary)
            .when_some(view.local_form.as_ref(), |section, form| {
                section.child(form.render(
                    local,
                    view.operation == Operation::SavingLocal,
                    view.busy() || view.editor.is_some() || snapshot.pause.is_some(),
                    stacked,
                    cx,
                ))
            })
            .child(history)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn backup_next_local_countdown_rounds_up_to_the_minute() {
        assert_eq!(super::countdown(-5), "Due now");
        assert_eq!(super::countdown(0), "Due now");
        assert_eq!(super::countdown(1), "In 1 min");
        assert_eq!(super::countdown(3540), "In 59 min");
        assert_eq!(super::countdown(3599), "In 1h 0m");
        assert_eq!(super::countdown(3601), "In 1h 1m");
    }

    use super::{duration, server_timestamp};

    #[test]
    fn backup_timestamps_use_the_server_timezone_and_offset_fallback() {
        let at = "2026-11-12T23:00:00Z";
        assert_eq!(server_timestamp(at, "Asia/Dubai", 14400), "Nov 13, 03:00");
        assert_eq!(server_timestamp(at, "UTC+04:00", 14400), "Nov 13, 03:00");
        assert_eq!(duration(327), "5m 27s");
    }
}
