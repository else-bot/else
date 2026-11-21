use chrono::{DateTime, Local, Utc};
use else_wire::bots::Avatar;
use else_wire::bots::AvatarColor;
use gpui_kit::component::{ActiveTheme as _, h_flex};
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
    let color = avatar.map(|a| a.color).unwrap_or_else(|| {
        let hash = id.as_bytes().iter().fold(0usize, |hash, byte| {
            hash.wrapping_mul(31).wrapping_add(*byte as usize)
        });
        palette[hash % palette.len()]
    });
    crate::avatar::color_fill(color)
}

pub(crate) fn percent(value: f64) -> String {
    let rounded = (value * 10.).round() / 10.;
    if rounded.fract() == 0. {
        format!("{rounded:.0}%")
    } else {
        format!("{rounded:.1}%")
    }
}

pub(crate) fn share(value: f64) -> String {
    if value > 0. && value < 0.005 {
        "<1%".into()
    } else {
        format!("{:.0}%", (value * 100.).round())
    }
}

pub(crate) fn sources(bot: &crate::api::BotUsage) -> Vec<(crate::api::UsageSourceKind, f64)> {
    let mut groups: Vec<(crate::api::UsageSourceKind, f64)> = Vec::new();
    for source in &bot.sources {
        if let Some((_, amount)) = groups.iter_mut().find(|(kind, _)| *kind == source.source) {
            *amount += source.share;
        } else {
            groups.push((source.source, source.share));
        }
    }
    groups.sort_by(|a, b| b.1.total_cmp(&a.1));
    groups
}

/// "Chat 44% · Routines 33% · Asked by other bots 22%", largest first.
pub(crate) fn source_summary(bot: &crate::api::BotUsage) -> String {
    sources(bot)
        .iter()
        .map(|(kind, amount)| format!("{} {}", kind.label(), share(*amount)))
        .collect::<Vec<_>>()
        .join(" · ")
}

pub(crate) fn local_time(value: DateTime<Utc>) -> String {
    value.with_timezone(&Local).format("%H:%M").to_string()
}

pub(crate) fn reset_in(value: DateTime<Utc>, now: DateTime<Utc>) -> String {
    if value <= now {
        return "awaiting next reading".into();
    }
    let minutes = ((value - now).num_seconds() + 59) / 60;
    let days = minutes / 1440;
    let hours = minutes % 1440 / 60;
    if days > 0 {
        if hours > 0 {
            format!(
                "in {days} {} {hours} h",
                if days == 1 { "day" } else { "days" }
            )
        } else {
            format!("in {days} {}", if days == 1 { "day" } else { "days" })
        }
    } else if hours > 0 {
        if minutes % 60 > 0 {
            format!("in {hours} h {} min", minutes % 60)
        } else {
            format!("in {hours} h")
        }
    } else {
        format!("in {minutes} min")
    }
}

pub(crate) fn resets(value: DateTime<Utc>) -> String {
    let now = Utc::now();
    if value <= now {
        return "Awaiting next reading".into();
    }
    let time = value
        .with_timezone(&Local)
        .format(if (value - now).num_hours() >= 24 {
            "%a %H:%M"
        } else {
            "%H:%M"
        })
        .to_string();
    format!("Resets {} · {time}", reset_in(value, now))
}

pub(crate) fn bar(segments: impl IntoIterator<Item = (f64, Hsla)>, cx: &App) -> Div {
    h_flex()
        .relative()
        .w_full()
        .h(px(5.))
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
}

/// A bot's share as a bare fill with no track, so it doesn't read as a
/// limit meter.
pub(crate) fn share_bar(share: f64, color: Hsla) -> Div {
    div()
        .h(px(6.))
        .rounded_sm()
        .bg(color)
        .w(relative(share.clamp(0., 1.) as f32))
}

/// A bot without an avatar: its initial on a tint of its colour.
pub(crate) fn initial(name: &str, color: Hsla, size: Pixels) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(size)
        .rounded_full()
        .bg(color.opacity(0.25))
        .text_color(color)
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .child(
            name.chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_default(),
        )
}

#[cfg(test)]
mod tests {
    #[test]
    fn usage_shares_round_without_hiding_small_nonzero_use() {
        assert_eq!(super::share(0.), "0%");
        assert_eq!(super::share(0.0001), "<1%");
        assert_eq!(super::share(0.0049), "<1%");
        assert_eq!(super::share(0.005), "1%");
        assert_eq!(super::share(0.416), "42%");
        assert_eq!(super::share(1.), "100%");
    }

    #[test]
    fn usage_reset_durations_use_days_hours_and_minutes() {
        use chrono::{Duration, TimeZone, Utc};
        let now = Utc.with_ymd_and_hms(2026, 11, 16, 14, 0, 0).unwrap();
        assert_eq!(
            super::reset_in(now + Duration::minutes(11), now),
            "in 11 min"
        );
        assert_eq!(
            super::reset_in(now + Duration::hours(110), now),
            "in 4 days 14 h"
        );
        assert_eq!(super::reset_in(now + Duration::days(1), now), "in 1 day");
        assert_eq!(
            super::reset_in(now + Duration::minutes(61), now),
            "in 1 h 1 min"
        );
        assert_eq!(
            super::reset_in(now + Duration::seconds(10), now),
            "in 1 min"
        );
        assert_eq!(super::reset_in(now, now), "awaiting next reading");
    }

    #[test]
    fn usage_sources_combine_repeated_kinds_from_older_servers() {
        use crate::api::{BotUsage, UsageSource, UsageSourceKind};
        let bot = BotUsage {
            bot_id: uuid::Uuid::nil(),
            name: "Tech Manager".into(),
            share: 0.416,
            sources: [
                (UsageSourceKind::Chat, 0.444),
                (UsageSourceKind::Routine, 0.313),
                (UsageSourceKind::Delegated, 0.113),
                (UsageSourceKind::Delegated, 0.063),
                (UsageSourceKind::Delegated, 0.048),
                (UsageSourceKind::Routine, 0.018),
            ]
            .into_iter()
            .map(|(source, share)| UsageSource { source, share })
            .collect(),
        };
        assert_eq!(
            super::source_summary(&bot),
            "Chat 44% · Routines 33% · Asked by other bots 22%"
        );
    }
}
