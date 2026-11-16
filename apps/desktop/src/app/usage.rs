//! The Usage section of a bot's details: the bot's share of each provider
//! window, fetched while the details panel shows the bot.

use super::ElseApp;
use super::settings::{item_name, meta, muted, note, section, section_frame};
use crate::api::{Attribution, Bot, ProviderUsage, UsageReport};
use crate::controls::{Control as _, Scale, Tier};
use crate::usage_ui::{Loaded, percent, sentence, share};
use gpui_kit::component::button::Button;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::*;
use uuid::Uuid;

/// The providers this bot ran on, each window keeping only the bot's row.
pub(super) type UsageState = Loaded<Vec<ProviderUsage>>;

impl ElseApp {
    /// Refreshes the bot's usage, keeping what is shown until it arrives.
    /// Called when the details panel opens or switches to `bot`.
    pub(super) fn load_bot_usage(
        &mut self,
        bot: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.settings.get_mut(&bot) else {
            return;
        };
        let attempt = entry.usage.begin();
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client
                .usage()
                .await
                .map(|report| own_usage(report, bot))
                .map_err(|error| error.to_string());
            let _ = this.update(cx, |app, cx| {
                if app
                    .settings
                    .get_mut(&bot)
                    .is_some_and(|entry| entry.usage.settle(attempt, result))
                {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn render_bot_usage(
        &self,
        bot: &Bot,
        state: &UsageState,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let providers = match state {
            Loaded::Loading(_) => {
                return Some(
                    section("Usage", cx)
                        .child(muted("Loading usage…", cx))
                        .into_any_element(),
                );
            }
            Loaded::Failed(error) => {
                return Some(
                    section("Usage", cx)
                        .child(note(format!("Couldn't load usage. {error}"), true, cx))
                        .child(
                            Button::new("retry-bot-usage")
                                .control(Tier::Secondary, Scale::Compact, false, cx)
                                .label("Retry")
                                .on_click(cx.listener({
                                    let id = bot.id;
                                    move |app, _, window, cx| app.load_bot_usage(id, window, cx)
                                })),
                        )
                        .into_any_element(),
                );
            }
            Loaded::Ready { value, .. } if value.is_empty() => return None,
            Loaded::Ready { value, .. } => value,
        };
        let mut body = section_frame(cx).gap_3().child(
            h_flex()
                .justify_between()
                .gap_2()
                .child(crate::appearance::section_label("Usage", cx))
                .child(
                    Button::new("open-bot-usage")
                        .control(Tier::Ghost, Scale::Compact, false, cx)
                        .label("Open in Usage →")
                        .on_click(cx.listener(|app, _, window, cx| {
                            app.sidebar_focus.focus(window, cx);
                            window.dispatch_action(
                                Box::new(crate::connection::OpenUsageSettings),
                                cx,
                            );
                        })),
                ),
        );
        let unmeasured = providers.iter().any(|provider| {
            provider
                .windows
                .iter()
                .any(|window| matches!(window.attribution, Attribution::Unattributed { .. }))
        });
        body = body.child(meta(
            if unmeasured {
                "Estimates. This bot's part of your subscription limits isn't known yet."
            } else {
                "Estimates."
            },
            cx,
        ));
        for provider in providers {
            let name = self.provider_label(&provider.provider_id);
            let mut windows: Vec<_> = provider.windows.iter().collect();
            windows.sort_by_key(|window| std::cmp::Reverse(window.minutes));
            let mut group = v_flex().gap_1().child(item_name(name.clone()));
            for window in windows {
                let Some(entry) = window.bot(bot.id) else {
                    continue;
                };
                let mut amount = share(entry.share);
                if let Attribution::Priced { used, outside, .. } = window.attribution {
                    amount = format!(
                        "{amount} · about {} of the limit",
                        percent((entry.share * (used - outside).max(0.)).round())
                    );
                }
                group = group.child(
                    v_flex()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!("{amount} of your bots' {name} use")),
                        )
                        .child(meta(sentence(&window.period()), cx)),
                );
            }
            body = body.child(group);
        }
        Some(body.into_any_element())
    }
}

fn own_usage(report: UsageReport, bot: Uuid) -> Vec<ProviderUsage> {
    report
        .providers
        .into_iter()
        .filter_map(|mut provider| {
            for window in &mut provider.windows {
                window.bots.retain(|entry| entry.bot_id == bot);
            }
            provider
                .windows
                .iter()
                .any(|window| !window.bots.is_empty())
                .then_some(provider)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::own_usage;
    use crate::api::{FIVE_HOURS, UsageReport, UsageSourceKind, WEEK};
    use crate::usage_ui::sources;

    #[test]
    fn bot_usage_keeps_shares_and_sources_in_their_own_period() {
        let report: UsageReport =
            serde_json::from_str(include_str!("../api/usage_fixture.json")).unwrap();
        let bot = report.providers[0].windows[0].bots[0].bot_id;
        let own = own_usage(report, bot);
        assert_eq!(own.len(), 1);
        let provider = &own[0];
        let weekly = provider.selected_window(None).unwrap();
        assert_eq!(weekly.minutes, WEEK);
        assert_eq!(weekly.bot(bot).unwrap().share, 1.0);
        assert_eq!(
            sources(weekly.bot(bot).unwrap()),
            [(UsageSourceKind::Chat, 1.0)]
        );
        let five = provider.selected_window(Some(FIVE_HOURS)).unwrap();
        assert_eq!(five.bot(bot).unwrap().share, 0.51);
        assert_eq!(
            sources(five.bot(bot).unwrap())[0],
            (UsageSourceKind::Routine, 0.88)
        );
        assert_eq!(five.attribution.used(), Some(62.0));
        assert!(provider.windows.iter().all(|window| window.bots.len() == 1));
    }
}
