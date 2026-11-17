//! Looping indicators (working dots, waiting rings, progress sweeps) move
//! only while their window is in front of the owner. Every animation frame
//! repaints the whole window, and the states behind these loops can last for
//! hours, so a loop left running in a background window keeps the GPU busy
//! the whole time.
//!
//! The window redraws when it gains or loses focus or visibility, so a loop
//! freezes in its still pose on blur and resumes on focus.

use gpui_kit::{AnyElement, App, IntoElement, RenderOnce, Window};

/// Whether looping motion may run in `window`.
pub fn ambient_motion(window: &Window, cx: &App) -> bool {
    !cx.reduce_motion() && window.is_window_active() && window.is_visible()
}

/// Builds an indicator with `build(true)` while looping motion may run in
/// its window, and with `build(false)` for its still pose otherwise.
pub fn looping(build: impl FnOnce(bool) -> AnyElement + 'static) -> Looping {
    Looping(Box::new(build))
}

#[derive(IntoElement)]
pub struct Looping(Box<dyn FnOnce(bool) -> AnyElement>);

impl RenderOnce for Looping {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        (self.0)(ambient_motion(window, cx))
    }
}
