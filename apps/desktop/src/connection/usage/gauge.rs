use crate::api::UsageWindow;
use crate::usage_ui::{bar, percent, resets};
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::*;

pub(super) fn gauge(window: &UsageWindow, cx: &App) -> Div {
    let theme = cx.theme();
    let used = window.attribution.used().expect("reported limit");
    let fill = if used >= 95. {
        theme.danger
    } else if used >= 80. {
        theme.warning
    } else {
        theme.muted_foreground
    };
    let mut card = v_flex()
        .flex_1()
        .min_w_0()
        .p_4()
        .gap_3()
        .rounded_lg()
        .bg(theme.background)
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(window.title()),
        )
        .child(
            div()
                .text_size(px(30.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!("{} used", percent(used))),
        )
        .child(bar([(used, fill)], cx))
        .children(window.attribution.resets_at().map(|time| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(resets(time))
        }));
    if let Some(outside) = window.attribution.outside() {
        card = card.child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!(
                    "Your bots ≈ {} · other use ≈ {}",
                    percent((used - outside).max(0.)),
                    percent(outside)
                )),
        );
    }
    card
}
