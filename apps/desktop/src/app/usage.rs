//! The Usage section of a bot's details: the bot's share of each provider
//! window, fetched while the details panel shows the bot.

use super::ElseApp;
use super::settings::{item_name, meta, muted, note, section, section_frame};
use crate::api::{Bot, FIVE_HOURS, ProviderUsage, UsageReport};
use crate::controls::{Control as _, Scale, Tier};
use crate::usage_ui::{Loaded, bar, color, percent, reset_in, share, source_summary};
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
        for provider in providers {
            let name = self.provider_label(&provider.provider_id);
            let mut group = v_flex().gap_2().child(item_name(name.clone()));
            let Some(window) = provider.primary_window() else {
                continue;
            };
            let entry = window.bot(bot.id);
            let amount = entry.map(|entry| entry.share).unwrap_or(0.);
            group = group
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!(
                            "{} of your bots' {name} use {}",
                            share(amount),
                            window.period()
                        )),
                )
                .child(bar([(amount * 100., color(bot.id, Some(bot.avatar)))], cx));
            if let Some(entry) = entry {
                group = group.child(meta(source_summary(entry, true), cx));
            }
            if let (Some(used), Some(resets_at)) =
                (window.attribution.used(), window.attribution.resets_at())
            {
                group = group.child(meta(
                    format!(
                        "{name} {}: {} used · resets {}",
                        window.title().to_lowercase(),
                        percent(used),
                        reset_in(resets_at, chrono::Utc::now())
                    ),
                    cx,
                ));
            }
            if window.minutes != FIVE_HOURS
                && let Some(five) = provider
                    .windows
                    .iter()
                    .find(|window| window.minutes == FIVE_HOURS)
            {
                group = group.child(meta(
                    format!(
                        "Last 5 hours: {} of your bots' {name} use",
                        share(five.bot(bot.id).map(|entry| entry.share).unwrap_or(0.))
                    ),
                    cx,
                ));
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
        let weekly = provider.primary_window().unwrap();
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
