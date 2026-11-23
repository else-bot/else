use super::BotPanel;
use crate::app::computer::DetailsTab;
use crate::app::panel::{alert, item_name, list_rows, meta, meta_line, section};
use crate::controls::{Control as _, Scale, Tier};
use chrono::Utc;
use else_wire::routines::Watcher;
use else_wire::schedule::{watcher_deadline, watcher_last_check, watcher_next};
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

impl BotPanel {
    pub(super) fn sync_watchers(&mut self, watchers: &[Watcher]) {
        let active = |id| watchers.iter().any(|watcher| watcher.id == id);
        self.watcher_stops.retain(|id, _| active(*id));
        self.open_watcher_log = self.open_watcher_log.filter(|id| active(*id));
    }

    fn watchers(&self) -> &[Watcher] {
        self.automations()
            .map_or(&[], |automations| &automations.watchers)
    }

    /// Ticks while the Details tab shows watchers, so their check times
    /// stay current.
    pub(super) fn sync_watcher_clock(&mut self, cx: &mut Context<Self>) {
        let visible = self.inputs.shown == Some(DetailsTab::Details) && !self.watchers().is_empty();
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

    fn stop_watcher(&mut self, watcher: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if !self.connected() || !self.watchers().iter().any(|active| active.id == watcher) {
            return;
        }
        if matches!(
            self.watcher_stops.get(&watcher),
            Some(WatcherStop::Stopping)
        ) {
            return;
        }
        self.watcher_stops.insert(watcher, WatcherStop::Stopping);
        cx.notify();
        let (client, bot) = (self.client.clone(), self.bot);
        cx.spawn_in(window, async move |this, cx| {
            let Some(state) = stop_result(client.stop_watcher(bot, watcher).await) else {
                return;
            };
            let _ = this.update(cx, |panel, cx| {
                if let Some(stop) = panel.watcher_stops.get_mut(&watcher) {
                    *stop = state;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn render_watchers(&self, cx: &mut Context<Self>) -> Option<Div> {
        let watchers = self.watchers();
        if watchers.is_empty() {
            return None;
        }
        let now = Utc::now();
        let connected = self.connected();
        let rows = watchers
            .iter()
            .map(|watcher| {
                let id = watcher.id;
                let stop = self.watcher_stops.get(&id);
                let stopping = matches!(stop, Some(WatcherStop::Stopping));
                let error = match stop {
                    Some(WatcherStop::Failed(message) | WatcherStop::Unconfirmed(message)) => {
                        Some(alert(message.clone(), cx.theme().danger, cx))
                    }
                    _ => None,
                };
                let stop = Button::new(SharedString::from(format!("watcher-stop-{id}")))
                    .control(Tier::Ghost, Scale::Compact, stopping || !connected, cx)
                    .label(if stopping { "Stopping…" } else { "Stop" })
                    .loading(stopping)
                    .accessibility_label(format!(
                        "{} watching {}",
                        if stopping { "Stopping" } else { "Stop" },
                        watcher.purpose
                    ))
                    .on_click(
                        cx.listener(move |panel, _, window, cx| panel.stop_watcher(id, window, cx)),
                    );
                let open = self.open_watcher_log == Some(id);
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
                        .on_click(cx.listener(move |panel, _, _, cx| {
                            panel.open_watcher_log =
                                (panel.open_watcher_log != Some(id)).then_some(id);
                            cx.notify();
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
#[cfg(test)]
mod tests {
    use super::{WatcherStop, stop_result};
    use crate::api::ClientError;

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
