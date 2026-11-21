use super::{Palette, View};
use crate::api::{BotUsage, ProviderUsage, UsageWindow};
use crate::avatar::bot_avatar;
use crate::controls::{hugging_segment, segment_track};
use crate::usage_ui::{initial, percent, share, share_bar, source_summary};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::*;

const AVATAR: f32 = 24.;
const NAME: f32 = 180.;
const SHARE: f32 = 64.;

impl View {
    pub(super) fn usage_table(
        &self,
        provider: &ProviderUsage,
        window: &UsageWindow,
        name: &str,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let mut bots: Vec<&BotUsage> = window.bots.iter().collect();
        bots.sort_by(|a, b| b.share.total_cmp(&a.share));
        let (small_count, small_share) = folded(&bots);
        let periods: Vec<&UsageWindow> = provider
            .windows
            .iter()
            .filter(|w| w.attribution.used().is_some())
            .collect();
        let mut heading = h_flex()
            .w_full()
            .justify_between()
            .items_center()
            .gap_3()
            .child(crate::appearance::section_label(
                format!("Share of your bots' {name} use {}", window.period()),
                cx,
            ));
        if periods.len() > 1 {
            heading = heading.child(segment_track(cx).children(periods.iter().map(|period| {
                let minutes = period.minutes;
                hugging_segment(
                    SharedString::from(format!("usage-period-{minutes}")),
                    period.title(),
                    minutes == window.minutes,
                    false,
                    cx,
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.window_minutes = Some(minutes);
                    view.expanded = None;
                    view.show_more = false;
                    cx.notify();
                }))
            })));
        }
        let column = |text: &'static str, width: Option<f32>, right: bool| {
            let cell = div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text);
            let cell = match width {
                Some(width) => cell.w(px(width)).flex_none(),
                None => cell.flex_1().min_w_0(),
            };
            if right { cell.text_right() } else { cell }
        };
        let mut table = v_flex()
            .gap_1()
            .child(heading)
            .child(v_flex().mb_2().children(match (window.attribution.used(), window.attribution.outside()) {
                (Some(used), Some(outside)) => vec![format!(
                    "Estimated. Together your bots used about {} of this limit.",
                    percent((used - outside).max(0.).round()),
                )],
                (Some(used), None) => vec![
                    format!("Estimated. They compare your bots with each other and are not shares of the {} used.", percent(used)),
                    "Each bot's part of the limit isn't known yet.".into(),
                ],
                (None, _) => vec!["Estimated. They compare your bots with each other.".to_owned()],
            }.into_iter().map(|line| div().text_xs().text_color(theme.muted_foreground).child(line))))
            .child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .pb_1()
                    .border_b_1()
                    .border_color(theme.border.opacity(0.5))
                    .child(div().w(px(AVATAR)).flex_none())
                    .child(column("Bot", Some(NAME), false))
                    .child(column("", None, false))
                    .child(column("Share of bots' use", Some(SHARE + 60.), true)),
            );
        if bots.is_empty() {
            table = table.child(
                div()
                    .py_4()
                    .text_color(theme.muted_foreground)
                    .child("No bot use in this period."),
            );
        }
        for bot in bots
            .into_iter()
            .filter(|bot| self.show_more || bot.share >= 0.01)
        {
            let id = bot.bot_id;
            let row = h_flex()
                .id(SharedString::from(format!("usage-bot-{id}")))
                .w_full()
                .py_1p5()
                .gap_3()
                .items_center()
                .cursor_pointer()
                .role(Role::Button)
                .tab_index(0)
                .aria_label(format!("Show sources for {}", bot.name))
                .aria_expanded(self.expanded == Some(id))
                .border_b_1()
                .border_color(theme.border.opacity(0.5))
                .hover(|row| row.bg(theme.muted_foreground.opacity(0.06)))
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.expanded = (view.expanded != Some(id)).then_some(id);
                    cx.notify();
                }))
                .child(
                    h_flex().w(px(AVATAR)).flex_none().justify_center().child(
                        palette
                            .avatar(id)
                            .map(|avatar| {
                                bot_avatar(
                                    SharedString::from(format!("usage-avatar-{id}")),
                                    avatar,
                                    px(AVATAR),
                                )
                                .still()
                                .into_any_element()
                            })
                            .unwrap_or_else(|| {
                                initial(&bot.name, palette.color(id), px(AVATAR)).into_any_element()
                            }),
                    ),
                )
                .child(
                    div()
                        .w(px(NAME))
                        .flex_none()
                        .truncate()
                        .child(bot.name.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(share_bar(bot.share, palette.color(id))),
                )
                .child(
                    div()
                        .w(px(SHARE + 60.))
                        .flex_none()
                        .text_right()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(share(bot.share)),
                );
            table = table.child(row);
            if self.expanded == Some(id) {
                table = table.child(
                    div()
                        .pl(px(AVATAR + 12.))
                        .pb_3()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{}: {}", bot.name, source_summary(bot))),
                );
            }
        }
        if small_count > 0 {
            table = table.child(
                div()
                    .id("usage-more-bots")
                    .py_3()
                    .cursor_pointer()
                    .text_xs()
                    .text_color(theme.primary)
                    .role(Role::Button)
                    .tab_index(0)
                    .aria_expanded(self.show_more)
                    .aria_label(if self.show_more {
                        "Show fewer bots"
                    } else {
                        "Show more bots"
                    })
                    .child(if self.show_more {
                        "Show fewer bots".into()
                    } else {
                        format!(
                            "Show {small_count} more bots (about {} together)",
                            share(small_share)
                        )
                    })
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.show_more = !view.show_more;
                        cx.notify();
                    })),
            );
        }
        table.into_any_element()
    }
}

fn folded(bots: &[&BotUsage]) -> (usize, f64) {
    bots.iter()
        .filter(|bot| bot.share < 0.01)
        .fold((0, 0.), |(count, total), bot| {
            (count + 1, total + bot.share)
        })
}

#[cfg(test)]
mod tests {
    use super::folded;
    use crate::api::BotUsage;

    #[test]
    fn usage_folds_only_bots_under_one_percent() {
        let bot = |share| BotUsage {
            bot_id: uuid::Uuid::new_v4(),
            name: "Bot".into(),
            share,
            sources: vec![],
        };
        let bots = [bot(0.98), bot(0.01), bot(0.006), bot(0.004)];
        assert_eq!(folded(&bots.iter().collect::<Vec<_>>()), (2, 0.01));
    }
}
