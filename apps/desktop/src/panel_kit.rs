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
/// `corner` rounds the bottom fade's trailing corner to match a rounded
/// surface it ends on, because clipping is rectangular and an opaque square
/// fade would poke past the surface's curve.
pub(crate) fn faded(scroller: impl IntoElement, background: Hsla, corner: Pixels) -> Div {
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
        .child(edge().bottom_0().rounded_br(corner).bg(linear_gradient(
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

/// The 32 px tile that leads a [`list_row`]: a 6% ink well, visible on
/// any sheet or card.
pub(crate) fn row_icon(icon: Icon, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .size_8()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme.radius)
        .bg(theme.foreground.opacity(0.06))
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

/// A page's title and one-line description, with its header actions on the
/// title's line. Every page uses compact secondary actions here, so "Check
/// all", "Refresh" and "Add server" read as one set.
pub(crate) fn page_heading(
    title: &'static str,
    description: &'static str,
    actions: Vec<AnyElement>,
    cx: &App,
) -> Div {
    h_flex()
        .w_full()
        .items_start()
        .gap_3()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .child(
                    div()
                        .text_xl()
                        .line_height(px(28.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .max_w(PARAGRAPH)
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(description),
                ),
        )
        .when(!actions.is_empty(), |row| {
            row.child(h_flex().flex_none().gap_2().children(actions))
        })
}

/// The widest a Settings paragraph runs, so lines stay readable.
pub(crate) const PARAGRAPH: Pixels = px(480.);

/// One bordered group on a Settings page: `radius_lg`, a 16 px inset and
/// 16 px between its parts, the same frame as [`grouped_list`].
pub(crate) fn group_card(cx: &App) -> Div {
    v_flex()
        .w_full()
        .p_4()
        .gap_4()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(cx.theme().border)
}

/// A group's 16 px semibold title.
pub(crate) fn group_title(text: impl Into<SharedString>) -> Div {
    div()
        .text_base()
        .line_height(px(22.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

/// Lets a segment hug its label with 16 px on each side instead of taking
/// an equal share of a stretched track; the track then hugs its segments.
pub(crate) fn hugging(
    segment: gpui_kit::component::button::Button,
) -> gpui_kit::component::button::Button {
    segment.flex_none().flex_basis(Length::Auto).px_4()
}
