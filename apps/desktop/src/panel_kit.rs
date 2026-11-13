//! Small layout pieces shared by the Details panel, the Settings sheet and
//! its pages: soft scroll edges and one grouped-list style.

use gpui_kit::component::{ActiveTheme as _, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// How far each fade reaches into the scroll container.
pub(crate) const FADE: Pixels = px(24.);

/// Wrap a scroll container in a frame that fades its top and bottom edges
/// into `background`, so content dissolves under a header or footer instead
/// of being sliced at a hard line. The fades are static, so the container
/// must keep at least [`FADE`] of padding at both ends: at rest they cover
/// only that padding, and they soften only what scrolls beneath them.
pub(crate) fn faded(scroller: impl IntoElement, background: Hsla) -> Div {
    let edge = || div().absolute().left_0().right_0().h(FADE);
    div()
        .relative()
        .size_full()
        .min_h_0()
        .flex()
        .flex_col()
        .child(scroller)
        .child(edge().top_0().bg(linear_gradient(
            180.,
            linear_color_stop(background, 0.),
            linear_color_stop(background.opacity(0.), 1.),
        )))
        .child(edge().bottom_0().bg(linear_gradient(
            0.,
            linear_color_stop(background, 0.),
            linear_color_stop(background.opacity(0.), 1.),
        )))
}

/// One list style for every Settings list (servers, socials, email
/// accounts): a rounded container with hairlines between its rows.
pub(crate) fn grouped_list(rows: impl IntoIterator<Item = AnyElement>, cx: &App) -> Div {
    let line = cx.theme().border;
    v_flex()
        .w_full()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(line)
        .children(rows.into_iter().enumerate().map(move |(index, row)| {
            div()
                .w_full()
                .when(index > 0, |row| row.border_t_1().border_color(line))
                .child(row)
        }))
}

/// A row of a [`grouped_list`]: at least 56 px tall, inset 16 px, with its
/// leading icon, text and trailing actions centred on one axis.
pub(crate) fn list_row() -> Div {
    h_flex()
        .w_full()
        .min_h(px(56.))
        .px_4()
        .py_3()
        .gap_3()
        .items_center()
}

/// The 32 px tile that leads a [`list_row`].
pub(crate) fn row_icon(icon: Icon, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .size_8()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme.radius)
        .bg(theme.secondary)
        .child(icon.size_4().text_color(theme.muted_foreground))
}

/// A row's title and its one secondary line.
pub(crate) fn row_text(title: impl IntoElement, detail: impl IntoElement, cx: &App) -> Div {
    v_flex()
        .flex_1()
        .min_w_0()
        .gap_0p5()
        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(detail),
        )
}

/// A quiet state tag beside a row's title, such as "In use". Neutral in
/// both themes: the accent stays reserved for primary actions.
pub(crate) fn tag(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .flex_none()
        .px_1p5()
        .rounded(px(4.))
        .bg(theme.foreground.opacity(0.07))
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.muted_foreground)
        .child(text.into())
}
