use super::{Avatar, AvatarColor, AvatarShape, bot_avatar};
use crate::controls::{Control as _, Scale, Tier};
use gpui_kit::assets::IconName;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, h_flex, v_flex};
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
        // Shapes and colors share one grid: the same cells, columns and
        // ring, so both rows of each start under their label.
        let size = px(if self.compact { 36. } else { 40. });
        let art_size = px(if self.compact { 28. } else { 32. });
        let swatch_size = px(if self.compact { 22. } else { 24. });
        let radius = px(if self.compact { 10. } else { 12. });
        let gap = px(8.);
        let grid_width = size * COLUMNS as f32 + gap * (COLUMNS - 1) as f32;
        let theme = cx.theme();
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
            // A swatch close to the dialog's own tone (white on a light
            // theme) needs a firmer edge than the hairline to stay visible.
            let fill = color.fill();
            let edge = if (fill.l - theme.popover.l).abs() < 0.12 {
                theme.foreground.opacity(0.25)
            } else {
                theme.border
            };
            let swatch = div()
                .size(swatch_size)
                .rounded_full()
                .p(px(1.))
                .bg(edge)
                .child(div().size_full().rounded_full().bg(fill));
            // A round swatch takes a round ring: 2 px, then a 2 px gap.
            let ring_size = swatch_size + px(8.);
            Button::new(ElementId::Name(format!("avatar-color-{color:?}").into()))
                .focus_ring(false)
                .cursor_pointer()
                .ghost()
                .w(size)
                .h(size)
                .p_0()
                .rounded(size / 2.)
                .toggled(self.current.color == color)
                .disabled(self.disabled)
                .accessibility_label(format!("{} color", color.label()))
                .tooltip(color.label())
                .child(ring(
                    ring_size,
                    ring_size / 2.,
                    self.current.color == color,
                    cx,
                    swatch,
                ))
                .on_click(move |_, window, cx| pick(avatar, window, cx))
        });
        let reset = self.on_change;
        let grid =
            |cells: [Button; 12]| h_flex().w(grid_width).flex_wrap().gap(gap).children(cells);
        v_flex()
            .w(grid_width)
            .gap_2()
            .child(
                h_flex()
                    .h(px(28.))
                    .items_center()
                    .justify_between()
                    .child(crate::appearance::section_label("Shape", cx))
                    .child(
                        Button::new("reset-avatar")
                            .control(
                                Tier::Ghost,
                                Scale::Compact,
                                self.disabled || self.current == Avatar::default(),
                                cx,
                            )
                            .icon(IconName::Undo2)
                            // The kit's labels are 14 px; this one matches
                            // the 12 px "Shape" label beside it.
                            .child(div().text_xs().child("Reset"))
                            .accessibility_label("Reset avatar")
                            .mr(-Scale::Compact.padding())
                            .on_click(move |_, window, cx| reset(Avatar::default(), window, cx)),
                    ),
            )
            .child(grid(shapes))
            .child(
                h_flex()
                    .h(px(28.))
                    .mt_2()
                    .items_center()
                    .child(crate::appearance::section_label("Color", cx)),
            )
            .child(grid(colors))
    }
}

/// Cells per row in both grids.
const COLUMNS: usize = 6;

fn ring(size: Pixels, radius: Pixels, picked: bool, cx: &App, content: impl IntoElement) -> Div {
    let theme = cx.theme();
    // A picked item: a 2 px accent ring, then a 2 px gap.
    let (ring, gap) = if picked {
        (theme.primary, theme.popover)
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
