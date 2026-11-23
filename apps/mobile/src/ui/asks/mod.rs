//! Cards for what a bot waits on the owner to settle: questions, drafts,
//! secret requests and notices. Each card reflects the server: once
//! settled, its turn's update replaces it with a receipt.
//! This module holds what every card shares; each card has its own module.

mod draft;
mod notice;
mod question;
mod secret;

use nsh_core::prelude::*;
use uuid::Uuid;

use super::parts::colors;
use crate::conversation::Ask;
use else_client::request::Request;

pub fn card(bot: Uuid, ask: Ask) -> AnyElement {
    match ask {
        Ask::Question(question) => question::card(bot, *question).into_element(),
        Ask::Draft(draft) => draft::card(bot, *draft).into_element(),
        Ask::Secret(request) => secret::card(bot, *request).into_element(),
        Ask::Notice(notice) => notice::card(*notice).into_element(),
    }
}

pub(super) fn frame() -> View {
    view()
        .mx_4()
        .my_2()
        .p_4()
        .gap_3()
        .rounded(px(16.0))
        .border_curve(BorderCurve::Continuous)
        .bg(colors::card())
        .border_1()
        .border_color(colors::separator())
}

pub(super) fn eyebrow(label: &'static str) -> impl IntoElement {
    text(label)
        .text_size(px(13.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(colors::warning())
}

/// Why the card's last request did not go through. A lost response is an
/// unknown outcome: trying again repeats it, a repeat of a settled ask is
/// refused, and the server's own update removes the card once settled.
pub(super) fn failure<Op: Clone + 'static>(request: Signal<Request<Op>>) -> impl IntoElement {
    dynamic(move || {
        let message = request.with(|r| match r {
            Request::Unconfirmed { error, .. } => Some(format!(
                "{error} It may have gone through; trying again is safe."
            )),
            r => r.error().map(str::to_owned),
        })?;
        Some(
            text(message)
                .text_size(px(13.0))
                .line_height(px(18.0))
                .text_color(colors::destructive()),
        )
    })
}

/// How a card's button looks: filled for the main action, plain for the
/// others, in red for one that cannot be undone.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Fill {
    Prominent,
    ProminentDestructive,
    Plain,
    PlainDestructive,
}

/// A full-width button. Disabled, it takes no presses and screen readers
/// say it is dimmed.
pub(super) fn button(
    label: impl IntoReactive<SharedString>,
    fill: Fill,
    enabled: impl Fn() -> bool + Copy + 'static,
    on_press: impl Fn() + 'static,
) -> View {
    let accent = match fill {
        Fill::Prominent | Fill::Plain => colors::tint(),
        Fill::ProminentDestructive | Fill::PlainDestructive => colors::destructive(),
    };
    let filled = matches!(fill, Fill::Prominent | Fill::ProminentDestructive);
    view()
        .flex_1()
        .min_h(px(44.0))
        .px_3()
        .rounded(px(12.0))
        .border_curve(BorderCurve::Continuous)
        .items_center()
        .justify_center()
        .bg(if filled { accent } else { colors::fill() })
        .disabled(move || !enabled())
        .disabled_style(|s| s.opacity(0.45))
        .active(|s| s.opacity(0.7))
        .role(AccessibilityRole::Button)
        .on_press(on_press)
        .child(
            text(label)
                .text_size(px(17.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_center()
                .text_color(if filled { colors::on_tint() } else { accent }),
        )
}
