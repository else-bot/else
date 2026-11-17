//! Ctrl+1..9 (Cmd on macOS) switches to the rail's first nine servers.
//! Holding the modifier alone for a moment shows each server's shortcut in
//! place of its label.

use std::time::{Duration, Instant};

use super::*;

/// How long the modifier stays down alone before the hints show.
pub(super) const HOLD: Duration = Duration::from_millis(300);

/// The platform's command modifier held alone.
const PRIMARY: Modifiers = Modifiers {
    control: !cfg!(target_os = "macos"),
    alt: false,
    shift: false,
    platform: cfg!(target_os = "macos"),
    function: false,
};

/// The rail position a keystroke switches to.
pub(super) fn slot(keystroke: &Keystroke) -> Option<usize> {
    if keystroke.modifiers != PRIMARY {
        return None;
    }
    match keystroke.key.as_str() {
        key @ ("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9") => {
            Some(usize::from(key.as_bytes()[0] - b'1'))
        }
        _ => None,
    }
}

/// The hint drawn over the rail position `slot`.
pub(super) fn hint(slot: usize) -> String {
    let digit = slot + 1;
    if cfg!(target_os = "macos") {
        format!("⌘{digit}")
    } else {
        format!("Ctrl+{digit}")
    }
}

/// The shortcut screen readers announce, in ARIA `keyshortcuts` form.
pub(super) fn aria(slot: usize) -> String {
    let digit = slot + 1;
    if cfg!(target_os = "macos") {
        format!("Meta+{digit}")
    } else {
        format!("Control+{digit}")
    }
}

/// Whether the hints show. Any other key or modifier during a hold spoils
/// it until every modifier is released, so a chord such as Ctrl+C never
/// flashes the hints.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Hold {
    #[default]
    Idle,
    /// The modifier went down alone at this instant.
    Arming(Instant),
    Shown,
    Spoiled,
}

impl Hold {
    /// The hold after the modifiers change at `now`. Returns the hold and
    /// whether a timer must call [`Hold::elapsed`] after [`HOLD`].
    pub(super) fn modifiers(self, modifiers: Modifiers, now: Instant) -> (Self, bool) {
        match self {
            _ if !modifiers.modified() => (Self::Idle, false),
            _ if modifiers != PRIMARY => (Self::Spoiled, false),
            Self::Idle => (Self::Arming(now), true),
            held => (held, false),
        }
    }

    /// The hold after a key press. A digit while the hints show keeps them.
    pub(super) fn key(self, keystroke: &Keystroke) -> Self {
        match self {
            Self::Shown if slot(keystroke).is_some() => self,
            Self::Arming(_) | Self::Shown => Self::Spoiled,
            idle_or_spoiled => idle_or_spoiled,
        }
    }

    /// The hold once the timer started at `armed` fires.
    pub(super) fn elapsed(self, armed: Instant) -> Self {
        match self {
            Self::Arming(at) if at == armed => Self::Shown,
            other => other,
        }
    }

    /// The hold once the window loses focus.
    pub(super) fn blurred(self) -> Self {
        Self::Idle
    }

    /// The hold once focus returns to the shell. A release while a dialog
    /// had focus never reached the shell, so the live modifiers decide.
    pub(super) fn refocused(self, modifiers: Modifiers) -> Self {
        if modifiers.modified() {
            self
        } else {
            Self::Idle
        }
    }

    /// Whether the hints show while the window holds `modifiers`.
    pub(super) fn shows(self, modifiers: Modifiers) -> bool {
        self == Self::Shown && modifiers == PRIMARY
    }
}

impl Shell {
    pub(super) fn modifiers_changed(&mut self, modifiers: Modifiers, cx: &mut Context<Self>) {
        let now = Instant::now();
        let (hold, timer) = self.hold.modifiers(modifiers, now);
        self.move_hold(|_| hold, cx);
        if timer {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(HOLD).await;
                let _ = this.update(cx, |shell, cx| {
                    shell.move_hold(|hold| hold.elapsed(now), cx)
                });
            })
            .detach();
        }
    }

    /// Runs before key bindings, so a switch never reaches the composer.
    pub(super) fn server_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_hold(|hold| hold.key(keystroke), cx);
        let Some(id) = slot(keystroke)
            .and_then(|slot| self.registry.servers.get(slot))
            .map(|saved| saved.id)
        else {
            return;
        };
        if !self.rail_shown()
            || self.switcher_open()
            || !self.owns_focus(window, cx)
            || composing(window, cx)
        {
            return;
        }
        cx.stop_propagation();
        self.select_server(id, window, cx);
    }

    pub(super) fn move_hold(&mut self, change: impl FnOnce(Hold) -> Hold, cx: &mut Context<Self>) {
        let next = change(self.hold);
        if next != self.hold {
            self.hold = next;
            cx.notify();
        }
    }

    fn rail_shown(&self) -> bool {
        let onboarding = self
            .setup
            .as_ref()
            .is_some_and(|setup| setup.presentation == Presentation::Onboarding);
        self.active.is_some() && !onboarding
    }

    /// Whether focus is in the chat or in settings, not in a dialog such
    /// as a confirmation that a switch would bypass.
    fn owns_focus(&self, window: &Window, cx: &App) -> bool {
        let settings = self
            .setup
            .as_ref()
            .filter(|_| self.settings_section().is_some())
            .is_some_and(|setup| setup.focus.contains_focused(window, cx));
        window.focused(cx).is_none() || settings || self.settings_focus.contains_focused(window, cx)
    }
}

/// Whether an input method is composing text in the focused input.
fn composing(window: &mut Window, cx: &mut App) -> bool {
    use gpui_kit::component::input::AnyInputState;
    match window.focused_input(cx) {
        Some(AnyInputState::Input(state)) => marked(&state, window, cx),
        Some(AnyInputState::Textarea(state)) => marked(&state, window, cx),
        Some(AnyInputState::Editor(state)) => marked(&state, window, cx),
        Some(AnyInputState::Otp(_)) | None => false,
    }
}

fn marked<T: EntityInputHandler>(state: &Entity<T>, window: &mut Window, cx: &mut App) -> bool {
    state.update(cx, |state, cx| {
        state.marked_text_range(window, cx).is_some()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers {
        control: false,
        alt: false,
        shift: false,
        platform: false,
        function: false,
    };

    fn press(modifiers: Modifiers, key: &str) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: None,
            modifiers,
        }
    }

    fn primary(key: &str) -> Keystroke {
        press(PRIMARY, key)
    }

    #[::core::prelude::v1::test]
    fn primary_digits_one_to_nine_pick_rail_positions() {
        assert_eq!(slot(&primary("1")), Some(0));
        assert_eq!(slot(&primary("9")), Some(8));
        assert_eq!(slot(&primary("0")), None, "no tenth server");
        assert_eq!(slot(&primary("c")), None);
        assert_eq!(slot(&press(NONE, "2")), None, "a bare digit types");
        let shifted = Modifiers {
            shift: true,
            ..PRIMARY
        };
        assert_eq!(slot(&press(shifted, "2")), None);
        let swapped = Modifiers {
            control: PRIMARY.platform,
            platform: PRIMARY.control,
            ..NONE
        };
        assert_eq!(slot(&press(swapped, "2")), None, "the other OS's modifier");
    }

    #[::core::prelude::v1::test]
    fn holding_the_modifier_alone_shows_hints_until_release() {
        let t0 = Instant::now();
        let (hold, timer) = Hold::Idle.modifiers(PRIMARY, t0);
        assert!(timer, "pressing the modifier starts the timer");
        assert!(
            !hold.shows(PRIMARY),
            "nothing shows before the hold elapses"
        );
        let hold = hold.elapsed(t0);
        assert!(hold.shows(PRIMARY));
        let hold = hold.key(&primary("2"));
        assert!(hold.shows(PRIMARY), "switching keeps the hints");
        assert!(!hold.shows(NONE), "an unseen release hides them");
        let (hold, timer) = hold.modifiers(NONE, t0 + HOLD);
        assert!(!timer);
        assert_eq!(hold, Hold::Idle, "release hides the hints");
        assert_eq!(Hold::Shown.blurred(), Hold::Idle, "blur hides the hints");
    }

    #[::core::prelude::v1::test]
    fn a_release_missed_in_a_dialog_does_not_block_the_next_hold() {
        let t0 = Instant::now();
        let (armed, _) = Hold::Idle.modifiers(PRIMARY, t0);
        let spoiled = armed.key(&primary(","));
        assert_eq!(
            spoiled.refocused(PRIMARY),
            Hold::Spoiled,
            "still held when focus returns"
        );
        let back = spoiled.refocused(NONE);
        let (rearmed, timer) = back.modifiers(PRIMARY, t0 + HOLD);
        assert!(timer);
        assert!(rearmed.elapsed(t0 + HOLD).shows(PRIMARY));
    }

    #[::core::prelude::v1::test]
    fn chords_and_quick_switches_never_flash_hints() {
        let t0 = Instant::now();
        let (armed, _) = Hold::Idle.modifiers(PRIMARY, t0);
        let copied = armed.key(&primary("c"));
        assert!(!copied.elapsed(t0).shows(PRIMARY), "Ctrl+C then holding");
        let switched = armed.key(&primary("2"));
        assert!(!switched.elapsed(t0).shows(PRIMARY), "a quick Ctrl+2");
        let shift = Modifiers {
            shift: true,
            ..PRIMARY
        };
        let (spoiled, _) = armed.modifiers(shift, t0);
        let (back, timer) = spoiled.modifiers(PRIMARY, t0);
        assert!(!timer, "releasing Shift does not re-arm");
        assert!(!back.elapsed(t0).shows(PRIMARY));
        let (idle, _) = back.modifiers(NONE, t0);
        let (rearmed, timer) = idle.modifiers(PRIMARY, t0 + HOLD);
        assert!(timer, "a fresh press arms again");
        assert!(rearmed.elapsed(t0 + HOLD).shows(PRIMARY));
    }

    #[::core::prelude::v1::test]
    fn other_keys_hide_hints_and_stale_timers_do_nothing() {
        let t0 = Instant::now();
        let (armed, _) = Hold::Idle.modifiers(PRIMARY, t0);
        let shown = armed.elapsed(t0);
        assert_eq!(shown.key(&primary("k")), Hold::Spoiled);
        let (idle, _) = armed.modifiers(NONE, t0);
        let later = t0 + Duration::from_millis(300);
        let (rearmed, _) = idle.modifiers(PRIMARY, later);
        assert!(
            !rearmed.elapsed(t0).shows(PRIMARY),
            "the tapped press's timer is stale"
        );
        assert!(rearmed.elapsed(later).shows(PRIMARY));
    }
}
