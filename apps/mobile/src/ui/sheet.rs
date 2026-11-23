//! A form presented as an iOS page sheet over the stack, as Contacts and
//! Calendar present New and Edit: its own small stack (a picker pushes
//! inside it), Cancel at the top left, and a swipe down that closes it. With
//! unsaved changes, Cancel and the swipe ask before discarding them.

use std::rc::Rc;

use nsh_base::{alert_dialog, dialog, nav_stack};
use nsh_core::HeaderSystemItem;
use nsh_core::prelude::*;

use super::Route;

/// The sheet shown, its root page first.
#[derive(Default, Clone, PartialEq)]
struct Pages(Vec<Route>);

/// Whether the sheet's form has changes a close would lose.
#[derive(Default, Clone, Copy, PartialEq)]
struct Unsaved(bool);

fn pages() -> Signal<Pages> {
    global::<Pages>()
}

fn unsaved() -> Signal<Unsaved> {
    global::<Unsaved>()
}

/// Presents `root` in a sheet, unless a sheet is already shown.
pub fn present(key: impl Into<SharedString>, root: impl Fn() -> Screen + 'static) {
    let route = Route {
        key: key.into(),
        screen: Rc::new(root),
    };
    pages().update(|pages| {
        if pages.0.is_empty() {
            pages.0.push(route);
        }
    });
}

/// Pushes `screen` inside the sheet.
pub fn push(key: impl Into<SharedString>, screen: impl Fn() -> Screen + 'static) {
    let route = Route {
        key: key.into(),
        screen: Rc::new(screen),
    };
    pages().update(|pages| {
        if !pages.0.is_empty() && !pages.0.iter().any(|r| r.key == route.key) {
            pages.0.push(route);
        }
    });
}

/// Closes the sheet, its changes saved or discarded.
pub fn close() {
    unsaved().set(Unsaved(false));
    pages().set(Pages::default());
}

/// Closes the sheet when its root page is one `gone` names, such as the
/// form of a bot that was deleted.
pub fn close_if(gone: impl Fn(&str) -> bool) {
    if pages().with_untracked(|p| p.0.first().is_some_and(|r| gone(&r.key))) {
        close();
    }
}

/// Marks the sheet's form as holding changes while `changed` is true.
pub fn guard(changed: impl Fn() -> bool + 'static) {
    effect(move || unsaved().set(Unsaved(changed())));
}

/// Closes the sheet, after asking when that would lose changes.
fn cancel() {
    if !unsaved().get_untracked().0 {
        close();
        return;
    }
    spawn(async move {
        let choice = alert_dialog()
            .action_sheet()
            .cancel("keep", "Keep Editing")
            .destructive("discard", "Discard Changes")
            .show()
            .await;
        if matches!(choice, Ok(r) if r.action.as_deref() == Some("discard")) {
            close();
        }
    });
}

/// A sheet page's header: Cancel at its start.
pub fn page(screen: Screen) -> Screen {
    screen
        .header_left(
            view()
                .role(AccessibilityRole::Button)
                .label("Cancel")
                .on_press(cancel),
        )
        .header_left_system_item(HeaderSystemItem::Cancel)
}

/// The sheet, while one is presented. Goes after the stack it covers.
pub fn layer() -> impl IntoElement {
    let shown = computed(|| !pages().with(|p| p.0.is_empty()));
    dynamic(move || {
        shown.get().then(|| {
            let stacked = each(
                || pages().with(|p| p.0.clone()),
                |route: &Route| route.key.clone(),
                |route, depth| {
                    let route = route.get_untracked();
                    (route.screen)().on_dismiss(move || {
                        pages().update(|p| p.0.truncate(depth.get_untracked().max(1)));
                    })
                },
            );
            dialog()
                .presentation(ModalPresentation::PageSheet)
                .close_on_swipe(move || !unsaved().get().0)
                .on_cancel(cancel)
                .on_close(close)
                .child(nav_stack().flex_1().child(stacked))
        })
    })
}
