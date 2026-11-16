//! The "By bot" ledger: each bot's percent per window, its turns and
//! tokens, and its sources when expanded.

use super::Palette;
use crate::api::{BotUsage, ProviderUsage, WindowName};
use crate::avatar::bot_avatar;
use crate::connection::Shell;
use crate::usage_ui::{dot, percent, tokens, turns};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::*;
use uuid::Uuid;

/// Width of the bot column.
const NAME: f32 = 210.;
/// Width of each window's percent column.
const WINDOW: f32 = 64.;
/// The columns after the window percents, in order.
const COLUMNS: [(&str, f32); 4] = [
    ("Turns", 48.),
    ("Tokens in / out", 112.),
    ("Cache read / write", 122.),
    ("Mostly", 110.),
];

impl Shell {
    pub(super) fn usage_table(
        &self,
        provider: &ProviderUsage,
        palette: &Palette,
        expanded: Option<Uuid>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let filter = palette.filter;
        let theme = cx.theme().clone();
        let mut bots: Vec<&BotUsage> = Vec::new();
        for bot in provider.windows.iter().flat_map(|window| &window.bots) {
            if !bots.iter().any(|entry| entry.bot_id == bot.bot_id)
                && filter.is_none_or(|id| id == bot.bot_id)
            {
                bots.push(bot);
            }
        }
        let headers = h_flex()
            .pb_2()
            .border_b_1()
            .border_color(theme.border)
            .text_color(theme.muted_foreground)
            .text_xs()
            .child(cell("Bot", NAME))
            .children(
                provider
                    .windows
                    .iter()
                    .map(|window| cell(window.name(WindowName::Column), WINDOW)),
            )
            .children(COLUMNS.iter().map(|&(title, width)| cell(title, width)));
        let min_width = NAME
            + provider.windows.len() as f32 * WINDOW
            + COLUMNS.iter().map(|&(_, width)| width).sum::<f32>();
        let mut table = v_flex().min_w(px(min_width)).child(headers);
        if bots.is_empty() {
            table = table.child(div().py_4().text_color(theme.muted_foreground).child(
                if filter.is_some() {
                    "This bot has no turns in these windows."
                } else {
                    "No bot turns in these windows."
                },
            ));
        }
        let ledger = provider.windows.last();
        for listed in bots {
            let id = listed.bot_id;
            let bot = ledger.and_then(|window| window.bot(id)).unwrap_or(listed);
            let row = h_flex()
                .id(SharedString::from(format!("usage-bot-{id}")))
                .w_full()
                .py_3()
                .items_center()
                .cursor_pointer()
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
                    h_flex()
                        .w(px(NAME))
                        .flex_none()
                        .gap_2()
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
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().truncate().child(bot.name.clone()))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .truncate()
                                        .child(bot.models.join(", ")),
                                ),
                        ),
                )
                .children(provider.windows.iter().map(|window| {
                    cell(
                        window
                            .bot(id)
                            .map(|bot| percent(bot.percent))
                            .unwrap_or_else(|| "—".into()),
                        WINDOW,
                    )
                }))
                .child(cell(bot.turns.to_string(), COLUMNS[0].1))
                .child(cell(
                    format!(
                        "{} / {}",
                        tokens(bot.input_tokens),
                        tokens(bot.output_tokens)
                    ),
                    COLUMNS[1].1,
                ))
                .child(cell(
                    format!(
                        "{} / {}",
                        tokens(bot.cached_input_tokens),
                        tokens(bot.cache_write_tokens)
                    ),
                    COLUMNS[2].1,
                ))
                .child(cell(
                    bot.sources
                        .first()
                        .map(|source| source.source.label())
                        .unwrap_or("—"),
                    COLUMNS[3].1,
                ));
            table = table.child(row);
            if expanded == Some(id)
                && let Some(window) = ledger.filter(|window| window.bot(id).is_some())
            {
                for (index, source) in bot.sources.iter().enumerate() {
                    table = table.child(
                        h_flex()
                            .id(SharedString::from(format!(
                                "usage-source-{}-{}-{index}",
                                window.minutes,
                                source.routine_id.unwrap_or(id)
                            )))
                            .w_full()
                            .gap_3()
                            .py_2()
                            .pl_8()
                            .pr_3()
                            .bg(theme.muted_foreground.opacity(0.05))
                            .text_xs()
                            .child(
                                div()
                                    .w(px(100.))
                                    .text_color(theme.muted_foreground)
                                    .child(source.source.label()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(source.label.clone().unwrap_or_default()),
                            )
                            .child(cell(turns(source.turns), 65.))
                            .child(cell(percent(source.percent), 55.)),
                    );
                }
            }
        }
        if filter.is_none()
            && provider
                .windows
                .iter()
                .any(|window| window.attribution.outside().is_some())
        {
            table = table.child(
                h_flex()
                    .py_3()
                    .text_color(theme.muted_foreground)
                    .child(cell("Outside else.bot", NAME))
                    .children(provider.windows.iter().map(|window| {
                        cell(
                            window
                                .attribution
                                .outside()
                                .map(percent)
                                .unwrap_or_else(|| "—".into()),
                            WINDOW,
                        )
                    })),
            );
        }
        let priced = provider
            .windows
            .iter()
            .filter(|window| window.attribution.is_priced())
            .count();
        let unit = if priced == provider.windows.len() {
            "Share of your cap"
        } else if priced > 0 {
            "Share of your cap where attributed, else share of else.bot usage"
        } else {
            "Share of else.bot usage"
        };
        let scope = ledger
            .map(|window| window.name(WindowName::Title).to_lowercase())
            .unwrap_or_default();
        let caption = format!(
            "{unit} · expand a bot for its sources. Turns, tokens, and sources cover the {scope}."
        );
        v_flex()
            .gap_2()
            .child(crate::appearance::section_label("By bot", cx))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(caption),
            )
            .child(
                div()
                    .id("usage-ledger-scroll")
                    .w_full()
                    .overflow_x_scroll()
                    .child(table),
            )
            .into_any_element()
    }
}

fn cell(text: impl Into<SharedString>, width: f32) -> Div {
    div()
        .flex_none()
        .w(px(width))
        .pr_2()
        .truncate()
        .child(text.into())
}
