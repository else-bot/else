//! The app's one control kit: buttons, segmented controls, toggles, text
//! fields and the composer's send and stop buttons.
//!
//! Call sites say what a control is, a [`Tier`] and a [`Scale`], and never
//! set its colors, height or padding. The component library fixes button
//! heights at 24 and 32 px and paints disabled controls in its own bordered
//! style, so the kit overrides both here, once.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Selectable as _, Sizable as _, Size,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Div, ElementId, FontWeight, Hsla, ParentElement as _, Pixels, SharedString, Styled, div,
    px,
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

/// How strongly a disabled control fades. Disabled keeps the control's own
/// shape on a neutral fill, never a dimmed accent.
pub const DISABLED_OPACITY: f32 = 0.4;
/// Ghost and unselected segment labels sit below body text and come up to
/// full strength on hover or selection.
const QUIET_TEXT: f32 = 0.72;
const SEGMENT_TEXT: f32 = 0.6;
const RADIUS: Pixels = px(8.);
const THUMB_RADIUS: Pixels = px(6.);
const INSET: Pixels = px(2.);

/// The fill a tier shows while disabled: accent and danger fills turn
/// neutral so a disabled action never reads as a muddy accent.
pub const fn disabled_tier(tier: Tier) -> Tier {
    match tier {
        Tier::Primary | Tier::Danger | Tier::Secondary => Tier::Secondary,
        Tier::Ghost => Tier::Ghost,
    }
}

pub trait Control: Sized {
    /// Styles a labeled button as the kit's `tier` at `scale`, and disables
    /// it when `disabled`.
    fn control(self, tier: Tier, scale: Scale, disabled: bool, cx: &App) -> Self;
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
        let fill = match disabled_tier(tier) {
            Tier::Ghost => theme.transparent,
            _ => theme.button,
        };
        button
            .opacity(DISABLED_OPACITY)
            .border_0()
            .bg(fill)
            .text_color(theme.button_foreground)
    }
}

/// The well behind a segmented control: 32 px tall with a 2 px inset, one
/// faint step off the surface it sits on.
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

fn segment_thumb(cx: &App) -> Hsla {
    let theme = cx.theme();
    if theme.mode.is_dark() {
        theme.foreground.opacity(0.1)
    } else {
        theme.popover
    }
}

pub trait Segment: Sized {
    /// Styles a button as one equal-width segment of a [`segment_track`].
    fn segment(self, selected: bool, disabled: bool, cx: &App) -> Self;
}

impl Segment for Button {
    fn segment(self, selected: bool, disabled: bool, cx: &App) -> Self {
        let theme = cx.theme();
        let segment = self
            .ghost()
            .focus_ring(false)
            .with_size(Size::Small)
            .flex_1()
            .flex_basis(px(0.))
            .min_w_0()
            .h_full()
            .px_3()
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
            segment.text_color(theme.foreground.opacity(SEGMENT_TEXT))
        };
        if disabled {
            segment.opacity(DISABLED_OPACITY)
        } else {
            segment.cursor_pointer()
        }
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
    let label = label.into();
    Button::new(id)
        .accessibility_label(label.clone())
        .child(div().text_size(px(13.)).truncate().child(label))
        .segment(selected, disabled, cx)
}

/// A 36×20 switch with a solid 16 px knob. Its name comes from the call
/// site's `accessibility_label`.
pub fn toggle(id: impl Into<ElementId>, checked: bool, disabled: bool) -> Switch {
    Switch::new(id)
        .focus_ring(false)
        .checked(checked)
        .disabled(disabled)
        .when(!disabled, |switch| switch.cursor_pointer())
}

/// An editable single-line field: a fill set apart from the card or dialog
/// behind it, darker in dark and white in light, with a visible edge.
pub fn text_field(input: Input, cx: &App) -> Input {
    let theme = cx.theme();
    let (fill, edge) = field_colors(cx);
    input
        .focus_ring(false)
        .bg(fill)
        .border_color(edge)
        .text_color(theme.foreground)
}

/// The fill and edge every editable field shares, so custom fields that
/// are not an [`Input`] can match.
pub fn field_colors(cx: &App) -> (Hsla, Hsla) {
    let theme = cx.theme();
    if theme.mode.is_dark() {
        (
            gpui_kit::black().opacity(0.22),
            theme.foreground.opacity(0.12),
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
            .bg(theme.foreground.opacity(0.08))
            .text_color(theme.foreground.opacity(0.45))
    }
}

/// The stop button: a filled circle the size of send.
pub fn stop_button(id: impl Into<ElementId>, stopping: bool, cx: &App) -> Button {
    let theme = cx.theme();
    let button = Button::new(id)
        .focus_ring(false)
        .size(COMPOSER_ACTION)
        .rounded(COMPOSER_ACTION / 2.)
        .border_0()
        .bg(theme.foreground)
        .text_color(theme.background)
        .loading(stopping);
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
    fn a_disabled_accent_turns_neutral() {
        assert_eq!(disabled_tier(Tier::Primary), Tier::Secondary);
        assert_eq!(disabled_tier(Tier::Danger), Tier::Secondary);
        assert_eq!(disabled_tier(Tier::Secondary), Tier::Secondary);
        assert_eq!(disabled_tier(Tier::Ghost), Tier::Ghost);
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
