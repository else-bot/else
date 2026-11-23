//! Pieces the screens share: the theme's colors, avatars and the icons.

use else_wire::avatar::INK;
use else_wire::bots::{Avatar, AvatarColor};
use nsh_core::element::{ImageAsset, ImageSrc, asset};
use nsh_core::prelude::*;

pub const ARROW_UP: ImageAsset = asset!("arrow-up.svg");
pub const STOP: ImageAsset = asset!("stop.svg");
pub const PLUS: ImageAsset = asset!("plus.svg");
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

/// A bot's avatar: its shape in its color, with two eyes, drawn as the
/// desktop draws it at rest.
pub fn avatar(avatar: impl IntoReactive<Avatar>, size: f32) -> Svg {
    let avatar = avatar.into_reactive();
    svg(move || ImageSrc::svg_markup(&markup(avatar.get())))
        .size(px(size))
        .flex_shrink(0.0)
}

/// The avatar as an SVG in a unit box centered on the origin.
fn markup(avatar: Avatar) -> String {
    use std::fmt::Write as _;
    let points = avatar.shape.outline();
    let n = points.len();
    let at = |i: usize| points[i % n];
    // A smooth closed curve through every point (Catmull-Rom as cubic
    // Béziers), the path the desktop fills.
    let mut path = format!("M{:.4} {:.4}", points[0].0, points[0].1);
    for i in 0..n {
        let (p0, p1, p2, p3) = (at(i + n - 1), at(i), at(i + 1), at(i + 2));
        let c1 = (p1.0 + (p2.0 - p0.0) / 6.0, p1.1 + (p2.1 - p0.1) / 6.0);
        let c2 = (p2.0 - (p3.0 - p1.0) / 6.0, p2.1 - (p3.1 - p1.1) / 6.0);
        let _ = write!(
            path,
            "C{:.4} {:.4} {:.4} {:.4} {:.4} {:.4}",
            c1.0, c1.1, c2.0, c2.1, p2.0, p2.1
        );
    }
    path.push('Z');
    let edge = if avatar.color == AvatarColor::White {
        r##" stroke="#000" stroke-opacity="0.18" stroke-width="0.02""##
    } else {
        ""
    };
    let (w, h) = (0.075_f32, 0.165_f32);
    let spread = 0.08_f32.max(w * 1.15);
    let (ex, ey) = avatar.shape.eyes();
    let eye = |x: f32| {
        format!(
            r##"<rect x="{:.4}" y="{:.4}" width="{w}" height="{h}" rx="{:.4}" fill="#{INK:06x}"/>"##,
            x - w / 2.0,
            ey - h / 2.0,
            w / 2.0
        )
    };
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="-0.5 -0.5 1 1"><path d="{path}" fill="#{:06x}"{edge}/>{}{}</svg>"##,
        avatar.color.rgb(),
        eye(ex - spread),
        eye(ex + spread),
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
