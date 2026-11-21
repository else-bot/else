use super::BotSettings;
use crate::app::panel::{alert, item_name, list_rows, meta, meta_line, section};
use crate::app::{Connection, ElseApp, computer::DetailsTab};
use crate::controls::{Control as _, Scale, Tier};
use crate::model::{Conversation, Watcher, WatcherStatus};
use chrono::{DateTime, Utc};
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::*;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WatcherStop {
    Stopping,
    Failed(String),
    Unconfirmed(String),
}

/// What a finished stop request leaves on the row. `None` keeps it
/// stopping until `bot.updated` drops the watcher: it stopped, or it had
/// already ended.
fn stop_result(result: Result<(), crate::api::ClientError>) -> Option<WatcherStop> {
    match result {
        Ok(()) => None,
        Err(err) if err.code() == Some("not_active") => None,
        Err(err) if err.outcome_unknown() => Some(WatcherStop::Unconfirmed(format!(
            "Couldn't confirm the stop. {err}"
        ))),
        Err(err) => Some(WatcherStop::Failed(format!("Couldn't stop. {err}"))),
    }
}

impl BotSettings {
    pub(super) fn sync_watchers(&mut self, watchers: &[Watcher]) {
        let active = |id| watchers.iter().any(|watcher| watcher.id == id);
        self.watcher_stops.retain(|id, _| active(*id));
        self.open_watcher_log = self.open_watcher_log.filter(|id| active(*id));
    }
}

impl ElseApp {
    fn watchers(&self, bot: Uuid) -> &[Watcher] {
        self.automations(bot).map_or(&[], Conversation::watchers)
    }

    pub(in crate::app) fn sync_watcher_clock(&mut self, cx: &mut Context<Self>) {
        let visible = self.settings_shown()
            && self.details_tab == DetailsTab::Details
            && self
                .selected
                .is_some_and(|bot| !self.watchers(bot).is_empty());
        if !visible {
            self.watcher_clock = None;
        } else if self.watcher_clock.is_none() {
            self.watcher_clock = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(30))
                        .await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        }
    }

    fn stop_watcher(
        &mut self,
        bot: Uuid,
        watcher: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connection != Connection::Connected
            || !self.watchers(bot).iter().any(|active| active.id == watcher)
        {
            return;
        }
        let Some(entry) = self.settings.get_mut(&bot) else {
            return;
        };
        if matches!(
            entry.watcher_stops.get(&watcher),
            Some(WatcherStop::Stopping)
        ) {
            return;
        }
        entry.watcher_stops.insert(watcher, WatcherStop::Stopping);
        cx.notify();
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Some(state) = stop_result(client.stop_watcher(bot, watcher).await) else {
                return;
            };
            let _ = this.update(cx, |app, cx| {
                if let Some(entry) = app.settings.get_mut(&bot)
                    && let Some(stop) = entry.watcher_stops.get_mut(&watcher)
                {
                    *stop = state;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn render_watchers(
        &self,
        bot: Uuid,
        entry: &BotSettings,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let watchers = self.watchers(bot);
        if watchers.is_empty() {
            return None;
        }
        let now = Utc::now();
        let connected = self.connection == Connection::Connected;
        let rows = watchers
            .iter()
            .map(|watcher| {
                let id = watcher.id;
                let stop = entry.watcher_stops.get(&id);
                let stopping = matches!(stop, Some(WatcherStop::Stopping));
                let error = match stop {
                    Some(WatcherStop::Failed(message) | WatcherStop::Unconfirmed(message)) => {
                        Some(alert(message.clone(), cx.theme().danger, cx))
                    }
                    _ => None,
                };
                let stop =
                    Button::new(SharedString::from(format!("watcher-stop-{id}")))
                        .control(Tier::Ghost, Scale::Compact, stopping || !connected, cx)
                        .label(if stopping { "Stopping…" } else { "Stop" })
                        .loading(stopping)
                        .accessibility_label(format!(
                            "{} watching {}",
                            if stopping { "Stopping" } else { "Stop" },
                            watcher.purpose
                        ))
                        .on_click(cx.listener(move |app, _, window, cx| {
                            app.stop_watcher(bot, id, window, cx)
                        }));
                let open = entry.open_watcher_log == Some(id);
                let log = watcher.last_output.as_ref().filter(|text| !text.is_empty());
                let log_button = log.map(|_| {
                    Button::new(SharedString::from(format!("watcher-log-{id}")))
                        .control(Tier::Ghost, Scale::Compact, false, cx)
                        .flush_start(Scale::Compact)
                        .label(if open { "Hide log" } else { "View log" })
                        .accessibility_label(format!(
                            "{} log for {}",
                            if open { "Hide" } else { "View" },
                            watcher.purpose
                        ))
                        .on_click(cx.listener(move |app, _, _, cx| {
                            if let Some(entry) = app.settings.get_mut(&bot) {
                                entry.open_watcher_log =
                                    (entry.open_watcher_log != Some(id)).then_some(id);
                                cx.notify();
                            }
                        }))
                });
                let local = chrono::Local::now();
                let until = format!(
                    "until {}",
                    watcher_deadline(
                        watcher.until.with_timezone(&local.timezone()),
                        local.date_naive()
                    )
                );
                let timing = watcher_next(watcher, now);
                let mut labels = vec![meta(until, cx).into_any_element()];
                labels.extend(timing.map(|text| meta(text, cx).into_any_element()));
                v_flex()
                    .min_w_0()
                    .child(
                        h_flex()
                            .items_start()
                            .gap_2()
                            .child(item_name(watcher.purpose.clone()).flex_1().min_w_0())
                            .child(stop.flex_none()),
                    )
                    .child(meta(watcher_last_check(watcher, now), cx))
                    .child(meta_line(labels, cx))
                    .children(error)
                    .children(log_button.map(|button| h_flex().child(button)))
                    .children(log.filter(|_| open).map(|log| {
                        meta(log.clone(), cx)
                            .font_family(cx.theme().mono_font_family.clone())
                            .bg(cx.theme().secondary)
                            .rounded(cx.theme().radius)
                            .p_2()
                    }))
                    .into_any_element()
            })
            .collect();
        Some(section("Watching", cx).child(list_rows(rows)))
    }
}

/// A watcher's deadline: its time when it falls on `today`, else its date.
fn watcher_deadline<Tz: chrono::TimeZone>(until: DateTime<Tz>, today: chrono::NaiveDate) -> String
where
    Tz::Offset: std::fmt::Display,
{
    if until.date_naive() == today {
        until.format("%H:%M").to_string()
    } else {
        until.format("%b %-d").to_string()
    }
}

fn watcher_duration(minutes: i64) -> String {
    match minutes {
        ..=59 => format!("{minutes} min"),
        60..1440 => format!("{} hr", minutes / 60),
        1440..2880 => "1 day".to_owned(),
        _ => format!("{} days", minutes / 1440),
    }
}

fn watcher_last_check(watcher: &Watcher, now: DateTime<Utc>) -> String {
    let Some(at) = watcher.last_checked_at else {
        return match &watcher.last_status {
            Some(status) => format!("not checked yet: {status}"),
            None => "not checked yet".to_owned(),
        };
    };
    let minutes = (now - at).num_minutes();
    let when = if minutes <= 0 {
        "just now".to_owned()
    } else {
        format!("{} ago", watcher_duration(minutes))
    };
    match &watcher.last_status {
        Some(status) => format!("checked {when}: {status}"),
        None => format!("checked {when}"),
    }
}

fn watcher_next(watcher: &Watcher, now: DateTime<Utc>) -> Option<String> {
    let at = match watcher.status {
        WatcherStatus::Checking => return Some("checking now".to_owned()),
        WatcherStatus::Waiting { next_check_at } => next_check_at?,
    };
    Some(match (at - now).num_minutes() {
        _ if at <= now => "next check due now".to_owned(),
        0 => "next check in <1 min".to_owned(),
        minutes => format!("next check in {}", watcher_duration(minutes)),
    })
}

#[cfg(test)]
mod tests {
    use super::{WatcherStop, stop_result, watcher_deadline, watcher_last_check, watcher_next};
    use crate::api::ClientError;
    use crate::model::Watcher;
    use serde_json::json;

    #[test]
    fn watcher_labels_show_the_deadline_and_check_times() {
        let now = "2026-11-20T08:00:00Z".parse().unwrap();
        let watcher: Watcher = serde_json::from_value(json!({
            "id": "c7947c9e-2f91-4fb5-8fcc-ec4b20ca9307", "purpose": "Domain availability",
            "until": "2026-12-04T10:00:00Z", "status": "waiting",
            "last_status": "unavailable", "last_checked_at": "2026-11-20T07:58:00Z",
            "next_check_at": "2026-11-20T08:13:00Z", "last_output": null,
        }))
        .unwrap();
        let today = "2026-11-20".parse().unwrap();
        assert_eq!(watcher_deadline(watcher.until, today), "Dec 4");
        let tonight: chrono::DateTime<chrono::Utc> = "2026-11-20T18:30:00Z".parse().unwrap();
        assert_eq!(watcher_deadline(tonight, today), "18:30");
        assert_eq!(
            watcher_last_check(&watcher, now),
            "checked 2 min ago: unavailable"
        );
        assert_eq!(
            watcher_next(&watcher, now).as_deref(),
            Some("next check in 13 min")
        );
        assert_eq!(
            watcher_next(&watcher, "2026-11-20T08:13:01Z".parse().unwrap()).as_deref(),
            Some("next check due now")
        );
    }

    #[test]
    fn watcher_labels_show_a_first_check_in_progress() {
        let now = "2026-11-20T08:00:00Z".parse().unwrap();
        let watcher: Watcher = serde_json::from_value(json!({
            "id": "c7947c9e-2f91-4fb5-8fcc-ec4b20ca9307", "purpose": "Domain availability",
            "until": "2026-12-04T10:00:00Z", "status": "checking",
            "last_status": null, "last_checked_at": null, "next_check_at": null, "last_output": null,
        })).unwrap();
        assert_eq!(watcher_last_check(&watcher, now), "not checked yet");
        assert_eq!(watcher_next(&watcher, now).as_deref(), Some("checking now"));
    }

    #[test]
    fn a_watcher_that_never_ran_says_why() {
        let now = "2026-11-20T08:00:00Z".parse().unwrap();
        let watcher: Watcher = serde_json::from_value(json!({
            "id": "c7947c9e-2f91-4fb5-8fcc-ec4b20ca9307", "purpose": "Domain availability",
            "until": "2026-12-04T10:00:00Z", "status": "waiting",
            "last_status": "check did not run: the computer was not available",
            "last_checked_at": null, "next_check_at": "2026-11-20T08:01:00Z", "last_output": null,
        }))
        .unwrap();
        assert_eq!(
            watcher_last_check(&watcher, now),
            "not checked yet: check did not run: the computer was not available"
        );
    }

    #[test]
    fn a_stop_settles_on_its_result() {
        let api = |status: u16, code: &str| {
            Err(ClientError::Api {
                status,
                code: code.into(),
                message: "Nope.".into(),
                retryable: false,
            })
        };
        assert_eq!(stop_result(Ok(())), None);
        assert_eq!(
            stop_result(api(409, "not_active")),
            None,
            "it already ended"
        );
        assert_eq!(
            stop_result(api(404, "not_found")),
            Some(WatcherStop::Failed("Couldn't stop. Nope.".into()))
        );
        assert_eq!(
            stop_result(api(503, "unavailable")),
            Some(WatcherStop::Unconfirmed(
                "Couldn't confirm the stop. Nope.".into()
            ))
        );
    }
}
