//! Pieces the screens share: the theme's colors, avatars and the icons.

use nsh_core::element::{ImageAsset, asset};
use nsh_core::prelude::*;

pub const ARROW_UP: ImageAsset = asset!("arrow-up.svg");
pub const STOP: ImageAsset = asset!("stop.svg");
pub const SERVER: ImageAsset = asset!("server.svg");

/// The theme's colors (`theme.json`), light and dark.
pub mod colors {
    use nsh_base::theme;
    use nsh_core::element::AdaptiveColor;

    fn token(name: &str) -> AdaptiveColor {
        theme().color(name).unwrap_or(theme().foreground)
    }

    pub fn background() -> AdaptiveColor {
        theme().background
    }
    pub fn card() -> AdaptiveColor {
        theme().card
    }
    pub fn label() -> AdaptiveColor {
        theme().foreground
    }
    pub fn secondary() -> AdaptiveColor {
        theme().muted_foreground
    }
    pub fn separator() -> AdaptiveColor {
        theme().border
    }
    pub fn tint() -> AdaptiveColor {
        theme().primary
    }
    pub fn on_tint() -> AdaptiveColor {
        theme().primary_foreground
    }
    pub fn destructive() -> AdaptiveColor {
        theme().destructive
    }
    pub fn fill() -> AdaptiveColor {
        theme().muted
    }
    pub fn chat() -> AdaptiveColor {
        token("chat")
    }
    pub fn bubble_owner() -> AdaptiveColor {
        token("bubble-owner")
    }
    pub fn bubble_bot() -> AdaptiveColor {
        token("bubble-bot")
    }
    pub fn warning() -> AdaptiveColor {
        token("warning")
    }
}

/// A bot's round avatar: its initial on its color.
pub fn avatar(
    initial: impl IntoReactive<SharedString>,
    color: impl IntoReactive<u32>,
    size: f32,
) -> View {
    let color = color.into_reactive();
    view()
        .size(px(size))
        .rounded(px(size / 2.0))
        .items_center()
        .justify_center()
        .bg(move || rgb(color.get()))
        .child(
            text(initial)
                .text_size(px(size * 0.42))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(0xffffff)),
        )
}

/// An inset-grouped section's card, as iOS Settings draws them.
pub fn group() -> View {
    view()
        .mx_4()
        .bg(colors::card())
        .rounded(px(10.0))
        .border_curve(BorderCurve::Continuous)
        .overflow_hidden()
}

/// A section's footnote under its card.
pub fn footnote(content: impl IntoReactive<SharedString>) -> Text {
    text(content)
        .mx_4()
        .px_4()
        .pt(px(6.0))
        .text_size(px(13.0))
        .line_height(px(18.0))
        .text_color(colors::secondary())
}
