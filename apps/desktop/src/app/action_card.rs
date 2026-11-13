use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Theme, h_flex, v_flex};
use gpui_kit::*;

pub(super) fn card(
    theme: &Theme,
    icon: IconName,
    title: impl Into<SharedString>,
    source: impl Into<SharedString>,
    status: impl Into<SharedString>,
) -> Div {
    v_flex()
        .w_full()
        .min_w_0()
        .gap_4()
        .p_4()
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .items_start()
                .gap_3()
                .child(
                    h_flex()
                        .size_8()
                        .flex_none()
                        .justify_center()
                        .rounded(theme.radius)
                        .bg(theme.muted)
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(icon).size_4()),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .whitespace_normal()
                                .child(title.into()),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(theme.muted_foreground)
                                .whitespace_normal()
                                .child(source.into()),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .px_2()
                        .py_1()
                        .rounded(theme.radius)
                        .bg(theme.warning.opacity(0.08))
                        .text_xs()
                        .text_color(theme.warning)
                        .child(status.into()),
                ),
        )
}

pub(super) fn footer() -> Div {
    h_flex()
        .w_full()
        .justify_end()
        .items_center()
        .flex_wrap()
        .gap_2()
}

pub(super) fn event_row(theme: &Theme) -> Div {
    h_flex()
        .w_full()
        .min_w_0()
        .min_h(px(36.))
        .items_center()
        .gap_2()
        .px_2()
        .rounded(theme.radius)
        .text_size(px(13.))
        .text_color(theme.muted_foreground)
        .hover(|style| style.bg(theme.muted.opacity(0.5)))
}

pub(super) fn failure(theme: &Theme, text: String) -> Div {
    h_flex()
        .w_full()
        .min_w_0()
        .items_start()
        .gap_2()
        .child(
            Icon::new(IconName::TriangleAlert)
                .size_4()
                .text_color(theme.danger),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.danger)
                        .child("Failed"),
                )
                .child(
                    div()
                        .text_sm()
                        .whitespace_normal()
                        .text_color(theme.foreground)
                        .child(text),
                ),
        )
}
