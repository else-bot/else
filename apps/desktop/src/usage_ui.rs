use crate::avatar::{Avatar, AvatarColor};
use chrono::{DateTime, Local, Utc};
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

pub(crate) fn color(id: Uuid, avatar: Option<Avatar>) -> Hsla {
    let palette = [
        AvatarColor::Blue,
        AvatarColor::Purple,
        AvatarColor::Teal,
        AvatarColor::Orange,
        AvatarColor::Pink,
        AvatarColor::Green,
        AvatarColor::Amber,
    ];
    avatar
        .map(|a| a.color)
        .unwrap_or_else(|| {
            let hash = id.as_bytes().iter().fold(0usize, |hash, byte| {
                hash.wrapping_mul(31).wrapping_add(*byte as usize)
            });
            palette[hash % palette.len()]
        })
        .fill()
}

pub(crate) fn percent(value: f64) -> String {
    let rounded = (value * 10.).round() / 10.;
    if rounded.fract() == 0. {
        format!("{rounded:.0}%")
    } else {
        format!("{rounded:.1}%")
    }
}

pub(crate) fn turns(count: u64) -> String {
    if count == 1 {
        "1 turn".into()
    } else {
        format!("{count} turns")
    }
}

pub(crate) fn tokens(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!("{:.2}B", value as f64 / 1_000_000_000.)
    } else if value >= 999_500 {
        format!("{:.2}M", value as f64 / 1_000_000.)
    } else if value >= 1_000 {
        format!("{:.0}k", value as f64 / 1_000.)
    } else {
        value.to_string()
    }
}

pub(crate) fn local_time(value: DateTime<Utc>) -> String {
    value.with_timezone(&Local).format("%a %H:%M").to_string()
}

pub(crate) fn resets(value: Option<DateTime<Utc>>) -> String {
    value
        .map(|value| {
            let minutes = (value - Utc::now()).num_minutes().max(0);
            if value <= Utc::now() {
                "awaiting next reading".into()
            } else {
                format!(
                    "resets {} (in {}h {}m)",
                    local_time(value),
                    minutes / 60,
                    minutes % 60
                )
            }
        })
        .unwrap_or_else(|| "reset time unavailable".into())
}

pub(crate) fn bar(segments: impl IntoIterator<Item = (f64, Hsla)>, capped: bool, cx: &App) -> Div {
    h_flex()
        .relative()
        .w_full()
        .h(px(9.))
        .rounded_sm()
        .overflow_hidden()
        .bg(cx.theme().muted_foreground.opacity(0.10))
        .children(
            segments
                .into_iter()
                .filter(|(amount, _)| *amount > 0.)
                .map(|(amount, color)| {
                    div()
                        .flex_none()
                        .h_full()
                        .w(relative((amount / 100.).clamp(0., 1.) as f32))
                        .bg(color)
                }),
        )
        .when(capped, |bar| {
            bar.child(
                div()
                    .absolute()
                    .left(relative(0.8))
                    .top_0()
                    .bottom_0()
                    .w(px(1.))
                    .bg(cx.theme().foreground.opacity(0.5)),
            )
        })
}

pub(crate) fn dot(color: Hsla) -> Div {
    div().flex_none().size(px(7.)).rounded_sm().bg(color)
}
