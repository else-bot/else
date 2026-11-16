//! The Usage section of a bot's details: the bot's share of each provider
//! window, fetched while the details panel shows the bot.

use super::ElseApp;
use super::settings::{item_name, meta, muted, note, section, section_frame};
use crate::api::{Attribution, Bot, ProviderUsage, UsageReport};
use crate::controls::{Control as _, Scale, Tier};
use crate::usage_ui::{Loaded, bar, color, percent, resets};
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
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
        let theme = cx.theme().clone();
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
            let mut group = v_flex()
                .gap_2()
                .child(item_name(provider.label().to_owned()));
            for window in &provider.windows {
                let entry = window.bot(bot.id);
                let amount = entry.map(|bot| bot.percent).unwrap_or(0.);
                let attribution = window.attribution;
                let capped = attribution.is_priced();
                let label = match window.minutes {
                    300 => "This 5-hour window".into(),
                    10080 => "This week".into(),
                    _ => window.label(),
                };
                group = group
                    .child(
                        h_flex()
                            .justify_between()
                            .gap_2()
                            .text_xs()
                            .child(label)
                            .child(if capped {
                                format!("{} of cap", percent(amount))
                            } else {
                                percent(amount)
                            }),
                    )
                    .child(bar(
                        [
                            (amount, color(bot.id, Some(bot.avatar))),
                            (
                                (match attribution {
                                    Attribution::Priced { used, .. } => used,
                                    _ => 100.,
                                } - amount)
                                    .max(0.),
                                theme.muted_foreground.opacity(0.25),
                            ),
                        ],
                        capped,
                        cx,
                    ));
                let caption = match attribution {
                    Attribution::Priced {
                        used, resets_at, ..
                    } => format!(
                        "{} of the {} used so far · {}",
                        percent(if used > 0. { amount / used * 100. } else { 0. }),
                        percent(used),
                        resets(Some(resets_at))
                    ),
                    Attribution::Unattributed { used, .. } => format!(
                        "share of else.bot's {} use · {} of the cap used, not attributed yet",
                        provider.label(),
                        percent(used)
                    ),
                    Attribution::Unreported => {
                        format!("share of else.bot's {} use", provider.label())
                    }
                };
                group = group.child(meta(caption, cx));
            }
            if let Some(entry) = provider
                .windows
                .iter()
                .find_map(|window| window.bot(bot.id))
            {
                let mut sources: Vec<(crate::api::UsageSourceKind, f64)> = Vec::new();
                for source in &entry.sources {
                    if let Some((_, share)) =
                        sources.iter_mut().find(|(kind, _)| *kind == source.source)
                    {
                        *share += source.share;
                    } else {
                        sources.push((source.source, source.share));
                    }
                }
                sources.sort_by(|a, b| b.1.total_cmp(&a.1));
                group = group.child(meta(
                    sources
                        .iter()
                        .map(|(kind, share)| format!("{} {}", kind.label(), percent(share * 100.)))
                        .collect::<Vec<_>>()
                        .join(" · "),
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
                window.buckets.clear();
            }
            provider
                .windows
                .iter()
                .any(|window| !window.bots.is_empty())
                .then_some(provider)
        })
        .collect()
}
