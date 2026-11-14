//! The app's one control kit: buttons, segmented controls, toggles, text
//! fields and the composer's send and stop buttons.
//!
//! Call sites say what a control is, a [`Tier`] and a [`Scale`], and never
//! set its colors, height or padding. The component library fixes button
//! heights at 24 and 32 px, paints disabled controls in its own bordered
//! style and sizes switches at 28×16 or 36×20, so the kit overrides all
//! three here, once.

use std::rc::Rc;

use gpui_kit::base::{Switch as BaseSwitch, SwitchThumb, spring};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Selectable as _, Sizable as _, Size,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Div, ElementId, FontWeight, Hsla, IntoElement, ParentElement as _, Pixels, RenderOnce,
    SharedString, StatefulInteractiveElement as _, Styled, Window, div, px,
};

/// What a button does in its context: the one main action, a quieter
/// alternative, a text-like action, or a destructive confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The filled accent button; at most one per group.
    Primary,
    /// A quiet fill without a border.
    Secondary,
    /// Text that fills only on hover; activity-row and inline actions.
    Ghost,
    /// The filled confirmation of a destructive action.
    Danger,
}

/// The two button heights. Compact is for cards, rows and inline actions;
/// standard for dialogs, Settings and page actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
    Standard,
    Compact,
}

impl Scale {
    pub const fn height(self) -> Pixels {
        match self {
            Self::Standard => px(32.),
            Self::Compact => px(28.),
        }
    }

    /// The horizontal padding, so labels never touch the edges.
    pub const fn padding(self) -> Pixels {
        match self {
            Self::Standard => px(14.),
            Self::Compact => px(12.),
        }
    }

    /// The library size that picks the icon size and icon gap.
    const fn library(self) -> Size {
        match self {
            Self::Standard => Size::Medium,
            Self::Compact => Size::Small,
        }
    }
}

/// How strongly a disabled segment or other non-button control fades.
pub const DISABLED_OPACITY: f32 = 0.45;
/// A disabled button's label: about 40% ink on a neutral fill, whatever its
/// tier, so an unavailable primary never reads as a muted accent.
const DISABLED_LABEL: f32 = 0.4;
/// Ghost labels sit below body text and come up to full strength on hover.
const QUIET_TEXT: f32 = 0.85;
/// Unselected segment labels: quieter than the selected one, but clearly
/// available rather than disabled. Light themes' mid-tone ink needs more.
const SEGMENT_TEXT_DARK: f32 = 0.68;
const SEGMENT_TEXT_LIGHT: f32 = 0.8;
const RADIUS: Pixels = px(8.);
const THUMB_RADIUS: Pixels = px(6.);
const INSET: Pixels = px(2.);
/// A hugging segment's horizontal padding around its label.
const HUG_PADDING: Pixels = px(16.);

pub trait Control: Sized {
    /// Styles a labeled button as the kit's `tier` at `scale`, and disables
    /// it when `disabled`.
    fn control(self, tier: Tier, scale: Scale, disabled: bool, cx: &App) -> Self;

    /// Pulls a ghost button left by its padding so its text, not its hover
    /// fill, lines up with the content edge.
    fn flush_start(self, scale: Scale) -> Self;
}

impl Control for Button {
    fn control(self, tier: Tier, scale: Scale, disabled: bool, cx: &App) -> Self {
        let theme = cx.theme();
        let button = self
            .focus_ring(false)
            .with_size(scale.library())
            .h(scale.height())
            .px(scale.padding())
            .rounded(RADIUS)
            .font_weight(FontWeight::MEDIUM)
            .disabled(disabled);
        let button = match tier {
            Tier::Primary => button.primary(),
            Tier::Secondary => button.border_0(),
            Tier::Ghost => button
                .ghost()
                .text_color(theme.foreground.opacity(QUIET_TEXT)),
            Tier::Danger => button.danger(),
        };
        if !disabled {
            return button.cursor_pointer();
        }
        // The library paints disabled buttons in its own faint style. Filled
        // tiers become one neutral grey well; ghosts stay bare.
        let fill = match tier {
            Tier::Primary | Tier::Danger | Tier::Secondary => neutral_well(cx),
            Tier::Ghost => theme.transparent,
        };
        button
            .border_0()
            .bg(fill)
            .text_color(theme.foreground.opacity(DISABLED_LABEL))
    }

    fn flush_start(self, scale: Scale) -> Self {
        self.ml(-scale.padding())
    }
}

/// The neutral grey fill of a disabled button or an unready send button.
fn neutral_well(cx: &App) -> Hsla {
    let theme = cx.theme();
    theme
        .foreground
        .opacity(if theme.mode.is_dark() { 0.08 } else { 0.07 })
}

/// The well behind a segmented control: 32 px tall with a 2 px inset, one
/// faint step off the surface it sits on. Give it a width (or `w_full`) for
/// equal [`Segment::segment`]s. With [`hugging_segment`]s it takes their
/// width: in a row as it is, in a column with `.self_start()` so the column
/// doesn't stretch it.
///
/// A form field (provider, reasoning, speed) fills its column with equal
/// segments, edges aligned to the inputs above and below it; a tab-like
/// switch hugs its labels:
///
/// ```ignore
/// segment_track(cx).w_full().children([
///     segment("speed-standard", "Standard", true, false, cx),
///     segment("speed-fast", "Fast", false, false, cx),
/// ])
/// segment_track(cx).self_start().children([
///     hugging_segment("details", "Details", true, false, cx),
///     hugging_segment("computer", "Computer", false, false, cx),
/// ])
/// ```
pub fn segment_track(cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(px(32.))
        .p(INSET)
        .gap(INSET)
        .rounded(RADIUS)
        .bg(theme
            .foreground
            .opacity(if theme.mode.is_dark() { 0.05 } else { 0.06 }))
}

/// The selected segment: in dark, a moderate step over the track so a
/// stack of controls doesn't read as heavy bars; in light, the raised sheet
/// color.
fn segment_thumb(cx: &App) -> Hsla {
    let theme = cx.theme();
    if theme.mode.is_dark() {
        theme.foreground.opacity(0.13)
    } else {
        theme.popover
    }
}

/// How a segment takes its share of the track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fit {
    /// Equal widths across the whole track.
    Fill,
    /// As wide as its label plus 16 px on each side.
    Hug,
}

pub trait Segment: Sized {
    /// Styles a button as one equal-width segment of a [`segment_track`].
    fn segment(self, selected: bool, disabled: bool, cx: &App) -> Self;

    /// Styles a button as a segment that hugs its label (16 px either side,
    /// 28 px tall in a [`segment_track`]).
    fn hugging_segment(self, selected: bool, disabled: bool, cx: &App) -> Self;
}

impl Segment for Button {
    fn segment(self, selected: bool, disabled: bool, cx: &App) -> Self {
        styled_segment(self, Fit::Fill, selected, disabled, cx)
    }

    fn hugging_segment(self, selected: bool, disabled: bool, cx: &App) -> Self {
        styled_segment(self, Fit::Hug, selected, disabled, cx)
    }
}

fn styled_segment(button: Button, fit: Fit, selected: bool, disabled: bool, cx: &App) -> Button {
    let theme = cx.theme();
    let segment = button
        .ghost()
        .focus_ring(false)
        .with_size(Size::Small)
        .map(|segment| match fit {
            Fit::Fill => segment.flex_1().flex_basis(px(0.)).min_w_0().px_3(),
            Fit::Hug => segment.flex_none().px(HUG_PADDING),
        })
        .h_full()
        .rounded(THUMB_RADIUS)
        .font_weight(FontWeight::MEDIUM)
        .selected(selected)
        .disabled(disabled);
    let segment = if selected {
        segment
            .bg(segment_thumb(cx))
            .shadow_xs()
            .text_color(theme.foreground)
    } else {
        segment.text_color(theme.foreground.opacity(if theme.mode.is_dark() {
            SEGMENT_TEXT_DARK
        } else {
            SEGMENT_TEXT_LIGHT
        }))
    };
    if disabled {
        segment.opacity(DISABLED_OPACITY)
    } else {
        segment.cursor_pointer()
    }
}

/// A text segment with the kit's 13 px label; its label is also its name.
pub fn segment(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    disabled: bool,
    cx: &App,
) -> Button {
    labeled_segment(id, label).segment(selected, disabled, cx)
}

/// A text segment that hugs its label; see [`segment_track`].
pub fn hugging_segment(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    disabled: bool,
    cx: &App,
) -> Button {
    labeled_segment(id, label).hugging_segment(selected, disabled, cx)
}

fn labeled_segment(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    let label = label.into();
    Button::new(id)
        .accessibility_label(label.clone())
        .child(div().text_size(px(13.)).truncate().child(label))
}

const TOGGLE_WIDTH: Pixels = px(32.);
const TOGGLE_HEIGHT: Pixels = px(18.);
const TOGGLE_KNOB: Pixels = px(14.);

/// A 32×18 switch with a solid 14 px knob, quiet enough that the row's
/// title leads. Its name comes from the call site's `accessibility_label`.
pub fn toggle(id: impl Into<ElementId>, checked: bool, disabled: bool, _: &App) -> Toggle {
    Toggle {
        id: id.into(),
        checked,
        disabled,
        label: None,
        tooltip: None,
        on_click: None,
    }
}

/// The kit's switch; see [`toggle`].
#[derive(IntoElement)]
pub struct Toggle {
    id: ElementId,
    checked: bool,
    disabled: bool,
    label: Option<SharedString>,
    tooltip: Option<SharedString>,
    on_click: Option<ToggleHandler>,
}

type ToggleHandler = Rc<dyn Fn(&bool, &mut Window, &mut App)>;

impl Toggle {
    /// The name announced for the switch.
    pub fn accessibility_label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    /// Runs with the next checked value when the switch is activated.
    pub fn on_click(mut self, handler: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for Toggle {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        // The on-state is the accent a little calmer: 85% in dark, 80% in
        // light, so a column of switches never outshines the row titles.
        let on = theme
            .primary
            .opacity(if theme.mode.is_dark() { 0.85 } else { 0.8 });
        let track = if self.checked { on } else { theme.switch };
        let track = if self.disabled {
            track.opacity(track.a * 0.5)
        } else {
            track
        };
        let knob = theme.switch_thumb;
        let travel = TOGGLE_WIDTH - TOGGLE_KNOB - INSET * 2.;
        let motion = theme.motion_tokens().spring_move;
        let offset = spring(
            (self.id.clone(), "knob"),
            if self.checked { travel } else { px(0.) },
            motion,
            window,
            cx,
        );
        BaseSwitch::new(self.id)
            .checked(self.checked)
            .disabled(self.disabled)
            .when_some(self.label, |switch, label| {
                switch.accessibility_label(label)
            })
            .when_some(self.on_click, |switch, on_click| {
                switch.on_change(move |next, _, window, cx| on_click(&next, window, cx))
            })
            .when_some(self.tooltip, |switch, text| {
                switch.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
            })
            .flex_none()
            .flex()
            .items_center()
            .w(TOGGLE_WIDTH)
            .h(TOGGLE_HEIGHT)
            .p(INSET)
            .rounded(TOGGLE_HEIGHT / 2.)
            .bg(track)
            .when(!self.disabled, |switch| switch.cursor_pointer())
            .child(
                SwitchThumb::new(self.checked)
                    .size(TOGGLE_KNOB)
                    .rounded(TOGGLE_KNOB / 2.)
                    .left(offset)
                    .bg(knob)
                    .shadow_xs(),
            )
    }
}

/// An editable single-line field: one step lighter than whatever card,
/// dialog or page it sits on in dark, white in light, always with a visible
/// edge, so it reads as a field on any surface.
pub fn text_field(input: Input, cx: &App) -> Input {
    field_style(input.focus_ring(false), cx)
}

/// Gives any field-like control, such as a select, the text field's fill
/// and edge.
pub fn field_style<S: Styled>(control: S, cx: &App) -> S {
    let (fill, edge) = field_colors(cx);
    control
        .bg(fill)
        .border_color(edge)
        .text_color(cx.theme().foreground)
}

fn field_colors(cx: &App) -> (Hsla, Hsla) {
    let theme = cx.theme();
    if theme.mode.is_dark() {
        (
            theme.foreground.opacity(0.05),
            theme.foreground.opacity(0.14),
        )
    } else {
        (gpui_kit::white(), theme.foreground.opacity(0.16))
    }
}

/// Where the conversation's turn stands, as far as the composer cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turn {
    /// Nothing runs, or the bot only waits for the owner.
    Idle,
    Running,
    Stopping,
}

/// The composer's trailing buttons for a turn: send whenever nothing runs
/// or there is something to send, stop only while a turn runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposerActions {
    /// Shown, and ready when there is something to send.
    pub send: Option<bool>,
    /// Shown, and stopping when a stop is already under way.
    pub stop: Option<bool>,
}

pub const fn composer_actions(turn: Turn, can_send: bool) -> ComposerActions {
    match turn {
        Turn::Idle => ComposerActions {
            send: Some(can_send),
            stop: None,
        },
        Turn::Running | Turn::Stopping => ComposerActions {
            send: if can_send { Some(true) } else { None },
            stop: Some(matches!(turn, Turn::Stopping)),
        },
    }
}

/// The composer's 32 px round action size.
pub const COMPOSER_ACTION: Pixels = px(32.);

/// The send button: a neutral circle until there is something to send,
/// then the accent.
pub fn send_button(id: impl Into<ElementId>, ready: bool, cx: &App) -> Button {
    let theme = cx.theme();
    let button = Button::new(id)
        .focus_ring(false)
        .size(COMPOSER_ACTION)
        .rounded(COMPOSER_ACTION / 2.)
        .disabled(!ready);
    if ready {
        button.primary().cursor_pointer()
    } else {
        button
            .border_0()
            .bg(neutral_well(cx))
            .text_color(theme.foreground.opacity(0.45))
    }
}

/// The stop button: a neutral circle the size of send with a solid square
/// in full ink, present without outshining the conversation.
pub fn stop_button(id: impl Into<ElementId>, stopping: bool, cx: &App) -> Button {
    let theme = cx.theme();
    let glyph = theme.foreground;
    let button = Button::new(id)
        .focus_ring(false)
        .size(COMPOSER_ACTION)
        .rounded(COMPOSER_ACTION / 2.)
        .border_0()
        .bg(theme.foreground.opacity(0.12))
        .text_color(glyph)
        .loading(stopping)
        .map(|button| {
            if stopping {
                // The loading spinner takes the icon's place.
                button.icon(gpui_kit::assets::IconName::Square)
            } else {
                button.child(div().size(px(10.)).rounded(px(2.)).bg(glyph))
            }
        });
    if stopping {
        button
    } else {
        button.cursor_pointer()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_scales_are_32_and_28_px_with_room_for_their_labels() {
        assert_eq!(Scale::Standard.height(), px(32.));
        assert_eq!(Scale::Compact.height(), px(28.));
        for scale in [Scale::Standard, Scale::Compact] {
            let padding = scale.padding();
            assert!(padding >= px(12.) && padding <= px(14.));
        }
    }

    #[test]
    fn stop_shows_only_while_a_turn_runs() {
        assert_eq!(
            composer_actions(Turn::Idle, false),
            ComposerActions {
                send: Some(false),
                stop: None
            }
        );
        assert_eq!(
            composer_actions(Turn::Idle, true),
            ComposerActions {
                send: Some(true),
                stop: None
            }
        );
        assert_eq!(
            composer_actions(Turn::Running, false),
            ComposerActions {
                send: None,
                stop: Some(false)
            }
        );
        assert_eq!(
            composer_actions(Turn::Running, true),
            ComposerActions {
                send: Some(true),
                stop: Some(false)
            }
        );
        assert_eq!(
            composer_actions(Turn::Stopping, false),
            ComposerActions {
                send: None,
                stop: Some(true)
            }
        );
    }
}
