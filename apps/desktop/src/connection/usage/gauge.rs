use super::Palette;
use crate::api::{Attribution, UsageWindow, WindowName};
use crate::usage_ui::{bar, dot, local_time, percent, resets};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::*;

/// One window's card: its total, reset, and each bot's slice of it.
/// `reports_limits` tells whether the provider reports any window's total.
pub(super) fn gauge(
    provider: &str,
    reports_limits: bool,
    window: &UsageWindow,
    palette: &Palette,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    let mut card = v_flex()
        .flex_1()
        .min_w_0()
        .p_4()
        .gap_2()
        .rounded_lg()
        .bg(theme.background)
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(window.name(WindowName::Title)),
        );
    card = if let Some(used) = window.attribution.used() {
        card.child(
            div()
                .text_size(px(30.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(percent(used)),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!("used · {}", resets(window.attribution.resets_at()))),
        )
    } else {
        card.child(div().text_sm().child(if reports_limits {
            "No reading since this window reset".to_owned()
        } else {
            format!("{provider} doesn't report its limits")
        }))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Share of else.bot usage"),
        )
    };
    card = card.child(
        div()
            .text_xs()
            .text_color(theme.muted_foreground.opacity(0.75))
            .child(
                window
                    .attribution
                    .observed_at()
                    .map(|time| format!("As of {}", local_time(time)))
                    .unwrap_or_else(|| format!("Window starts {}", local_time(window.starts_at))),
            ),
    );
    let unattributed = match window.attribution {
        Attribution::Unattributed { used, .. } => Some(used),
        Attribution::Priced { .. } | Attribution::Unreported => None,
    };
    let segments = window
        .bots
        .iter()
        .filter(|_| unattributed.is_none())
        .map(|bot| (bot.percent, palette.fill(bot.bot_id)))
        .chain(
            window
                .attribution
                .outside()
                .or(unattributed)
                .map(|amount| (amount, theme.muted_foreground.opacity(0.4))),
        );
    let capped = window.attribution.used().is_some();
    card = card.child(bar(segments, capped, cx));
    if capped {
        card = card.child(
            div()
                .relative()
                .h(px(14.))
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .child(div().absolute().left_0().child("0%"))
                .child(
                    div()
                        .absolute()
                        .left(relative(0.8))
                        .ml(px(-10.))
                        .child("80%"),
                )
                .child(div().absolute().right_0().child("100%")),
        );
    }
    card.child(
        h_flex()
            .flex_wrap()
            .gap_x_3()
            .gap_y_1()
            .text_xs()
            .text_color(theme.muted_foreground)
            .children(window.bots.iter().map(|bot| {
                h_flex()
                    .gap_1()
                    .opacity(palette.emphasis(bot.bot_id))
                    .child(dot(palette.color(bot.bot_id)))
                    .child(if unattributed.is_some() {
                        format!("{} {} of else.bot use", bot.name, percent(bot.percent))
                    } else {
                        format!("{} {}", bot.name, percent(bot.percent))
                    })
            }))
            .children(window.attribution.outside().map(|amount| {
                h_flex()
                    .gap_1()
                    .child(dot(theme.muted_foreground.opacity(0.4)))
                    .child(format!("Outside else.bot {}", percent(amount)))
            }))
            .children(unattributed.map(|_| {
                h_flex()
                    .gap_1()
                    .child(dot(theme.muted_foreground.opacity(0.4)))
                    .child("Not attributed yet: it splits once a bot turn moves the total")
            })),
    )
}
