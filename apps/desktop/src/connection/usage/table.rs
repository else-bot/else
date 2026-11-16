use super::{Palette, View};
use crate::api::{BotUsage, UsageWindow};
use crate::avatar::bot_avatar;
use crate::connection::Shell;
use crate::usage_ui::{bar, dot, share, source_summary, sources};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::*;

impl Shell {
    pub(super) fn usage_table(
        &self,
        window: &UsageWindow,
        provider: &str,
        palette: &Palette,
        view: &View,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let mut bots: Vec<&BotUsage> = window.bots.iter().collect();
        bots.sort_by(|a, b| b.share.total_cmp(&a.share));
        let (small_count, small_share) = folded(&bots);
        let mut table = v_flex().gap_1()
            .child(crate::appearance::section_label(format!("Bots {}", window.period()), cx))
            .child(
                div().text_xs().mb_2().text_color(theme.muted_foreground)
                    .child(format!("Estimated share of your bots' {provider} use. Based on tokens at each model's price; your own use isn't counted.")),
            );
        if bots.is_empty() {
            table = table.child(
                div()
                    .py_4()
                    .text_color(theme.muted_foreground)
                    .child("No bot use in this window."),
            );
        }
        for bot in bots
            .into_iter()
            .filter(|bot| view.show_more || bot.share >= 0.01)
        {
            let id = bot.bot_id;
            let row =
                h_flex()
                    .id(SharedString::from(format!("usage-bot-{id}")))
                    .w_full()
                    .py_3()
                    .gap_3()
                    .items_center()
                    .cursor_pointer()
                    .role(Role::Button)
                    .tab_index(0)
                    .aria_label(format!("Show sources for {}", bot.name))
                    .aria_expanded(view.expanded == Some(id))
                    .border_b_1()
                    .border_color(theme.border.opacity(0.5))
                    .hover(|row| row.bg(theme.muted_foreground.opacity(0.06)))
                    .on_click(cx.listener(move |shell, _, _, cx| {
                        if let Some(view) = &mut shell.usage {
                            view.expanded = (view.expanded != Some(id)).then_some(id);
                            cx.notify();
                        }
                    }))
                    .child(
                        palette
                            .avatar(id)
                            .map(|avatar| {
                                bot_avatar(
                                    SharedString::from(format!("usage-avatar-{id}")),
                                    avatar,
                                    px(24.),
                                )
                                .still()
                                .into_any_element()
                            })
                            .unwrap_or_else(|| dot(palette.color(id)).into_any_element()),
                    )
                    .child(
                        div()
                            .w(px(180.))
                            .flex_none()
                            .truncate()
                            .child(bot.name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(bar([(bot.share * 100., palette.color(id))], cx)),
                    )
                    .child(
                        div()
                            .w(px(44.))
                            .flex_none()
                            .text_right()
                            .child(share(bot.share)),
                    )
                    .child(
                        div()
                            .w(px(105.))
                            .flex_none()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .children(sources(bot).first().map(|(kind, _)| {
                                format!("mostly {}", kind.label().to_lowercase())
                            })),
                    );
            table = table.child(row);
            if view.expanded == Some(id) {
                table = table.child(
                    div()
                        .pl_9()
                        .pb_3()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(source_summary(bot, false)),
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
                    .text_color(theme.muted_foreground)
                    .role(Role::Button)
                    .tab_index(0)
                    .aria_expanded(view.show_more)
                    .aria_label(if view.show_more {
                        "Show fewer bots"
                    } else {
                        "Show more bots"
                    })
                    .child(if view.show_more {
                        "Show fewer bots".into()
                    } else {
                        format!("{small_count} more bots · {}", share(small_share))
                    })
                    .on_click(cx.listener(|shell, _, _, cx| {
                        if let Some(view) = &mut shell.usage {
                            view.show_more = !view.show_more;
                            cx.notify();
                        }
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
