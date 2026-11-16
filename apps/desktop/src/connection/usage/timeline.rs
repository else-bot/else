use super::Palette;
use crate::api::UsageWindow;
use crate::usage_ui::{local_time, percent};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::*;

/// The window as columns of equal length, each stacking the bots' use and
/// the use outside else.bot.
pub(super) fn timeline(window: &UsageWindow, palette: &Palette, cx: &App) -> Div {
    let theme = cx.theme();
    let step = step_label(window);
    let peak = window
        .buckets
        .iter()
        .map(|bucket| bucket.bots.iter().map(|bot| bot.percent).sum::<f64>() + bucket.outside)
        .fold(0f64, f64::max)
        .max(f64::EPSILON);
    v_flex()
        .p_4()
        .gap_3()
        .rounded_lg()
        .bg(theme.background)
        .child(crate::appearance::section_label(
            format!("This window, every {step}"),
            cx,
        ))
        .child(h_flex().w_full().h(px(105.)).items_end().gap_1().children(
            window.buckets.iter().map(|bucket| {
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .justify_end()
                    .h_full()
                    .children((bucket.outside > 0.).then(|| {
                        div()
                            .w_full()
                            .h(px((bucket.outside / peak * 105.) as f32))
                            .bg(theme.muted_foreground.opacity(0.4))
                    }))
                    .children(bucket.bots.iter().rev().map(|bot| {
                        div()
                            .w_full()
                            .h(px((bot.percent / peak * 105.) as f32))
                            .bg(palette.fill(bot.bot_id))
                    }))
            }),
        ))
        .child(
            h_flex()
                .justify_between()
                .text_xs()
                .text_color(theme.muted_foreground)
                .children(
                    window
                        .buckets
                        .first()
                        .map(|bucket| local_time(bucket.starts_at)),
                )
                .child(format!("Peak {} / {step}", percent(peak)))
                .children(
                    window
                        .buckets
                        .last()
                        .map(|bucket| local_time(bucket.starts_at)),
                ),
        )
}

/// How long each column covers, such as "15 minutes".
fn step_label(window: &UsageWindow) -> String {
    let minutes = window.minutes / window.buckets.len().max(1) as u64;
    if minutes.is_multiple_of(60) && minutes >= 60 {
        let hours = minutes / 60;
        format!("{hours} hour{}", if hours == 1 { "" } else { "s" })
    } else {
        format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
    }
}
