use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The 1 px rule between the panel's header, tabs, sections and rows. It
/// matches the conversation header's rule so both sides read as one line.
pub(super) fn hairline(cx: &App) -> Hsla {
    cx.theme().foreground.opacity(0.06)
}

/// A titled group of rows in the Details panel: no box, just a label, 16 px
/// of padding and one full-width rule under the group. Rows inside never
/// draw their own rules.
pub(super) fn section_frame(cx: &App) -> Div {
    v_flex()
        .w_full()
        .px_4()
        .py_4()
        .border_b_1()
        .border_color(hairline(cx))
}

/// The gap from a section's label to its first row, and between rows.
pub(super) const ROW_GAP: Pixels = px(12.);

pub(super) fn section(title: &'static str, cx: &App) -> Div {
    section_frame(cx).child(crate::appearance::section_label(title, cx).pb(ROW_GAP))
}

/// A section's rows, 12 px apart.
pub(super) fn list_rows(rows: Vec<AnyElement>) -> Div {
    v_flex().w_full().gap(ROW_GAP).children(rows)
}

/// Secondary text in a row: a schedule, a model, a trigger's details. It
/// wraps instead of truncating.
pub(super) fn meta(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_size(px(12.))
        .line_height(px(18.))
        .text_color(cx.theme().muted_foreground)
        .whitespace_normal()
        .child(text.into())
}

/// A row's one meta line: whole tokens joined by "·". The separator stays
/// with the token before it, so a narrow panel wraps between tokens and
/// never starts a line with a dot.
pub(super) fn meta_line(tokens: Vec<AnyElement>, cx: &App) -> Div {
    let last = tokens.len().saturating_sub(1);
    let separator = cx.theme().muted_foreground.opacity(0.6);
    h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .gap_x_1()
        .children(tokens.into_iter().enumerate().map(move |(index, token)| {
            h_flex()
                .min_w_0()
                .gap_x_1()
                .items_center()
                .child(token)
                .when(index < last, |token| {
                    token.child(
                        div()
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(separator)
                            .child("·"),
                    )
                })
        }))
}

/// An item's name in a Details row, over its one meta line.
pub(super) fn item_name(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(14.))
        .line_height(px(20.))
        .font_weight(FontWeight::MEDIUM)
        .whitespace_normal()
        .child(text.into())
}

/// A problem in a row, with a small warning icon.
pub(super) fn alert(text: impl Into<SharedString>, color: Hsla, _cx: &App) -> Div {
    h_flex()
        .pt_0p5()
        .gap_1p5()
        .items_start()
        .text_size(px(12.5))
        .line_height(px(18.))
        .text_color(color)
        .child(
            Icon::new(IconName::TriangleAlert)
                .size_3()
                .mt(px(3.))
                .flex_none(),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .whitespace_normal()
                .child(text.into()),
        )
}

pub(super) fn muted(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

pub(super) fn note(text: impl Into<SharedString>, error: bool, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .text_xs()
        .text_color(if error { theme.danger } else { theme.success })
        .child(text.into())
}
