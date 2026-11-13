use super::{Avatar, AvatarColor, AvatarShape, bot_avatar};
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::*;
use std::rc::Rc;

type Change = Rc<dyn Fn(Avatar, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct AvatarPicker {
    current: Avatar,
    compact: bool,
    disabled: bool,
    on_change: Change,
}

impl AvatarPicker {
    pub fn new(
        current: Avatar,
        on_change: impl Fn(Avatar, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            current,
            compact: false,
            disabled: false,
            on_change: Rc::new(on_change),
        }
    }

    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl RenderOnce for AvatarPicker {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let size = px(if self.compact { 34. } else { 44. });
        let art_size = px(if self.compact { 26. } else { 32. });
        let radius = px(if self.compact { 10. } else { 12. });
        let shapes = AvatarShape::ALL.map(|shape| {
            let avatar = Avatar {
                shape,
                ..self.current
            };
            let pick = self.on_change.clone();
            Button::new(ElementId::Name(format!("avatar-shape-{shape:?}").into()))
                .focus_ring(false)
                .cursor_pointer()
                .ghost()
                .w(size)
                .h(size)
                .p_0()
                .rounded(radius)
                .toggled(self.current.shape == shape)
                .disabled(self.disabled)
                .accessibility_label(format!("{} shape", shape.label()))
                .tooltip(shape.label())
                .child(ring(
                    size,
                    radius,
                    self.current.shape == shape,
                    cx,
                    bot_avatar(
                        ElementId::Name(format!("avatar-shape-art-{shape:?}").into()),
                        avatar,
                        art_size,
                    )
                    .still(),
                ))
                .on_click(move |_, window, cx| pick(avatar, window, cx))
        });
        let colors = AvatarColor::ALL.map(|color| {
            let avatar = Avatar {
                color,
                ..self.current
            };
            let pick = self.on_change.clone();
            let swatch = div()
                .size(px(20.))
                .rounded_full()
                .p(px(1.))
                .bg(theme.border)
                .child(div().size_full().rounded_full().bg(color.fill()));
            Button::new(ElementId::Name(format!("avatar-color-{color:?}").into()))
                .focus_ring(false)
                .cursor_pointer()
                .ghost()
                .w(px(30.))
                .h(px(30.))
                .p_0()
                .rounded_full()
                .toggled(self.current.color == color)
                .disabled(self.disabled)
                .accessibility_label(format!("{} color", color.label()))
                .tooltip(color.label())
                .child(ring(
                    px(30.),
                    px(15.),
                    self.current.color == color,
                    cx,
                    swatch,
                ))
                .on_click(move |_, window, cx| pick(avatar, window, cx))
        });
        let reset = self.on_change;
        v_flex()
            .w(px(if self.compact { 224. } else { 288. }))
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .child(crate::appearance::section_label(
                        if self.compact { "Shape" } else { "Avatar" },
                        cx,
                    ))
                    .child(
                        Button::new("reset-avatar")
                            .focus_ring(false)
                            .cursor_pointer()
                            .label("Reset")
                            .xsmall()
                            .ghost()
                            .disabled(self.disabled || self.current == Avatar::default())
                            .on_click(move |_, window, cx| reset(Avatar::default(), window, cx)),
                    ),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .justify_center()
                    .children(shapes),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .justify_center()
                    .children(colors),
            )
    }
}

fn ring(size: Pixels, radius: Pixels, picked: bool, cx: &App, content: impl IntoElement) -> Div {
    let theme = cx.theme();
    let (ring, gap) = if picked {
        (theme.ring, theme.popover)
    } else {
        (transparent_black(), transparent_black())
    };
    let layer = |size: Pixels, radius: Pixels, fill: Hsla| {
        div()
            .size(size)
            .rounded(radius)
            .bg(fill)
            .flex()
            .items_center()
            .justify_center()
    };
    layer(size, radius, ring).child(layer(size - px(4.), radius - px(2.), gap).child(content))
}
