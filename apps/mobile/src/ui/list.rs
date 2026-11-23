//! Inset grouped lists, as iOS Settings draws them: a section's title, its
//! rows in a rounded card with separators between them, and a footnote.
//! nsh-base leaves the look of lists to the app; this is the app's one.
//! Rows use NSH's own disabled and selected states, so presses stop and
//! screen readers say so without the app tracking it.

use nsh_core::prelude::*;

use super::parts::{self, colors};

/// A section: an optional title, its rows, and an optional footnote.
pub fn section(
    title: Option<&'static str>,
    rows: impl IntoIterator<Item = AnyElement>,
    footer: Option<String>,
) -> View {
    view()
        .child(title.map(|title| {
            text(title.to_uppercase())
                .mx_4()
                .px_4()
                .pb(px(6.0))
                .text_size(px(13.0))
                .text_color(colors::secondary())
        }))
        .child(card(rows))
        .child(footer.map(parts::footnote))
}

/// Rows in a rounded card, a separator between each two, inset to the
/// text as iOS does.
pub fn card(rows: impl IntoIterator<Item = AnyElement>) -> View {
    let mut children = Vec::new();
    for (i, row) in rows.into_iter().enumerate() {
        if i > 0 {
            children.push(
                view()
                    .ml_4()
                    .h(px(0.5))
                    .bg(colors::separator())
                    .into_element(),
            );
        }
        children.push(row);
    }
    parts::group().children(children)
}

/// A row that opens another screen: its title, its value, and a chevron
/// when there is something to open.
pub fn link_row(
    title: impl Into<String>,
    value: String,
    open: Option<impl Fn() + 'static>,
) -> AnyElement {
    let title = title.into();
    let enabled = open.is_some();
    view()
        .flex_row()
        .items_center()
        .gap_3()
        .min_h(px(44.0))
        .px_4()
        .py(px(11.0))
        .active(|s| s.bg(colors::fill()))
        .disabled(!enabled)
        .role(AccessibilityRole::Button)
        .label(format!("{title}, {value}"))
        .on_press(move || {
            if let Some(open) = &open {
                open();
            }
        })
        .child(
            text(title)
                .number_of_lines(1)
                .text_size(px(17.0))
                .text_color(colors::label()),
        )
        .child(
            text(value)
                .flex_1()
                .number_of_lines(1)
                .text_right()
                .text_size(px(17.0))
                .text_color(colors::secondary()),
        )
        .child(enabled.then(super::parts::chevron))
        .into_element()
}

/// One option of a pick-one list: its label, a detail line when it has
/// one, and a checkmark when picked. A row that cannot be picked is dimmed.
pub fn option_row(
    label: String,
    detail: Option<String>,
    picked: impl Fn() -> bool + Clone + 'static,
    enabled: bool,
    on_press: impl Fn() + 'static,
) -> AnyElement {
    view()
        .flex_row()
        .items_center()
        .min_h(px(44.0))
        .px_4()
        .py(px(11.0))
        .active(|s| s.bg(colors::fill()))
        .disabled(!enabled)
        .role(AccessibilityRole::Button)
        .label(label.clone())
        .accessibility_state({
            let picked = picked.clone();
            move || AccessibilityState {
                selected: picked(),
                ..AccessibilityState::default()
            }
        })
        .on_press(move || {
            haptic(Haptic::Selection);
            on_press()
        })
        .child(
            view()
                .flex_1()
                .gap(px(2.0))
                .child(text(label).text_size(px(17.0)).text_color(if enabled {
                    colors::label()
                } else {
                    colors::secondary()
                }))
                .child(detail.map(|detail| {
                    text(detail)
                        .text_size(px(13.0))
                        .line_height(px(18.0))
                        .text_color(colors::secondary())
                })),
        )
        .child(dynamic(move || {
            picked().then(|| {
                image(super::parts::CHECKMARK)
                    .size(px(16.0))
                    .tint_color(colors::tint())
                    .label("Selected")
            })
        }))
        .into_element()
}

/// A setting turned on or off with the system's switch. The app owns the
/// value: a flip asks for one, and `checked` shows what the server kept.
pub fn switch_row(
    title: &'static str,
    checked: impl Fn() -> bool + 'static,
    enabled: impl Fn() -> bool + 'static,
    on_change: impl Fn(bool) + 'static,
) -> AnyElement {
    view()
        .flex_row()
        .items_center()
        .gap_3()
        .min_h(px(44.0))
        .px_4()
        .py(px(7.0))
        .child(
            text(title)
                .flex_1()
                .text_size(px(17.0))
                .text_color(colors::label()),
        )
        .child(
            nsh_core::mobile::switch()
                .checked(checked)
                .disabled(move || !enabled())
                .accessibility_label(title)
                .on_change(move |on: &bool| on_change(*on)),
        )
        .into_element()
}

/// A row that acts at once, in red when it cannot be undone.
pub fn action_row(
    title: impl Into<String>,
    destructive: bool,
    enabled: impl Fn() -> bool + 'static,
    on_press: impl Fn() + 'static,
) -> AnyElement {
    let title = title.into();
    view()
        .min_h(px(44.0))
        .px_4()
        .py(px(11.0))
        .justify_center()
        .active(|s| s.bg(colors::fill()))
        .disabled(move || !enabled())
        .disabled_style(|s| s.opacity(0.45))
        .role(AccessibilityRole::Button)
        .label(title.clone())
        .on_press(on_press)
        .child(text(title).text_size(px(17.0)).text_color(if destructive {
            colors::destructive()
        } else {
            colors::tint()
        }))
        .into_element()
}
