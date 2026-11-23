//! A form presented as an iOS page sheet over the stack, as Contacts and
//! Calendar present New and Edit: its own small stack (a picker pushes
//! inside it), Cancel at the top left, and a swipe down that closes it.
//! With unsaved changes, Cancel and the swipe ask before discarding them;
//! while the form saves, the sheet stays, so the server's answer always
//! finds it.

use std::rc::Rc;

use nsh_base::{alert_dialog, dialog, nav_stack};
use nsh_core::HeaderSystemItem;
use nsh_core::prelude::*;

use super::Route;

/// What closing the sheet would cost its form.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Form {
    /// Nothing to lose: it closes at once.
    #[default]
    Clean,
    /// Changes a close would discard: it asks first.
    Unsaved,
    /// A save is out: it stays until the answer.
    Saving,
}

/// One presentation of the sheet, told apart from the next by `id`.
#[derive(Clone, PartialEq)]
struct Shown {
    id: u64,
    /// The root page first.
    pages: Vec<Route>,
    form: Form,
}

/// The sheet's lifecycle. Its transitions are the functions of this
/// module; each names the presentation it is for, and one for another
/// presentation is refused.
#[derive(Clone, Default, PartialEq)]
enum Sheet {
    #[default]
    Hidden,
    Shown(Shown),
}

/// The presentations made so far, for the next one's id.
#[derive(Default, Clone, Copy, PartialEq)]
struct Presented(u64);

impl Sheet {
    /// Shows `root` as presentation `id`; refused while one is shown.
    fn present(&mut self, id: u64, root: Route) -> bool {
        match self {
            Sheet::Hidden => {
                *self = Sheet::Shown(Shown {
                    id,
                    pages: vec![root],
                    form: Form::Clean,
                });
                true
            }
            Sheet::Shown(_) => false,
        }
    }

    fn shown(&mut self, id: u64) -> Option<&mut Shown> {
        match self {
            Sheet::Shown(shown) if shown.id == id => Some(shown),
            _ => None,
        }
    }

    fn push(&mut self, id: u64, page: Route) {
        if let Some(shown) = self.shown(id)
            && !shown.pages.iter().any(|p| p.key == page.key)
        {
            shown.pages.push(page);
        }
    }

    /// Back to `depth` pages, never below the root.
    fn pop_to(&mut self, id: u64, depth: usize) {
        if let Some(shown) = self.shown(id) {
            shown.pages.truncate(depth.max(1));
        }
    }

    fn set_form(&mut self, id: u64, form: Form) {
        if let Some(shown) = self.shown(id) {
            shown.form = form;
        }
    }

    fn close(&mut self, id: u64) -> bool {
        let open = self.shown(id).is_some();
        if open {
            *self = Sheet::Hidden;
        }
        open
    }

    fn form(&self) -> Option<(u64, Form)> {
        match self {
            Sheet::Shown(shown) => Some((shown.id, shown.form)),
            Sheet::Hidden => None,
        }
    }
}

fn sheet() -> Signal<Sheet> {
    global::<Sheet>()
}

/// The presentation a sheet's page belongs to, for its form to report
/// its state, push a page or close it. Taken while the page is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handle(u64);

impl Handle {
    /// The sheet being shown, for a page built inside it.
    pub fn current() -> Self {
        Self(
            sheet()
                .with_untracked(Sheet::form)
                .map_or(u64::MAX, |(id, _)| id),
        )
    }

    /// Keeps the sheet's form state as `form` says.
    pub fn track(self, form: impl Fn() -> Form + 'static) {
        effect(move || {
            let form = form();
            sheet().update(|s| s.set_form(self.0, form));
        });
    }

    /// Pushes `screen` inside the sheet.
    pub fn push(self, key: impl Into<SharedString>, screen: impl Fn() -> Screen + 'static) {
        let page = Route {
            key: key.into(),
            screen: Rc::new(screen),
        };
        sheet().update(|s| s.push(self.0, page));
    }

    /// Closes the sheet: its changes are saved, or were discarded.
    pub fn close(self) {
        sheet().update(|s| {
            s.close(self.0);
        });
    }
}

/// Presents `root` in a sheet, unless a sheet is already shown.
pub fn present(key: impl Into<SharedString>, root: impl Fn() -> Screen + 'static) {
    let root = Route {
        key: key.into(),
        screen: Rc::new(root),
    };
    let id = global::<Presented>().get_untracked().0 + 1;
    let mut shown = false;
    sheet().update(|s| shown = s.present(id, root));
    if shown {
        global::<Presented>().set(Presented(id));
    }
}

/// Closes the sheet when its root page is one `gone` names, such as the
/// form of a bot that was deleted.
pub fn close_if(gone: impl Fn(&str) -> bool) {
    sheet().update(|s| {
        if let Sheet::Shown(shown) = s
            && shown.pages.first().is_some_and(|r| gone(&r.key))
        {
            *s = Sheet::Hidden;
        }
    });
}

/// The user asked to close the sheet (Cancel, a swipe down, Escape):
/// closes it, after asking when that would discard changes; not while it
/// saves.
fn cancel() {
    let Some((id, form)) = sheet().with_untracked(Sheet::form) else {
        return;
    };
    match form {
        Form::Clean => Handle(id).close(),
        Form::Saving => {}
        Form::Unsaved => drop(spawn(async move {
            let choice = alert_dialog()
                .action_sheet()
                .cancel("keep", "Keep Editing")
                .destructive("discard", "Discard Changes")
                .show()
                .await;
            if matches!(choice, Ok(r) if r.action.as_deref() == Some("discard")) {
                // The same presentation only: another may have replaced it.
                Handle(id).close();
            }
        })),
    }
}

/// A sheet page's header: Cancel at its start, off while the form saves.
pub fn page(screen: Screen) -> Screen {
    let saving = computed(|| {
        sheet().with(|s| s.form().is_some_and(|(_, form)| form == Form::Saving))
    });
    screen
        .header_left(
            view()
                .role(AccessibilityRole::Button)
                .label("Cancel")
                .disabled(move || saving.get())
                .on_press(cancel),
        )
        .header_left_system_item(HeaderSystemItem::Cancel)
}

/// The sheet, while one is presented. Goes after the stack it covers.
pub fn layer() -> impl IntoElement {
    let shown = computed(|| sheet().with(|s| s.form().map(|(id, _)| id)));
    dynamic(move || {
        let id = shown.get()?;
        let pages = each(
            move || {
                sheet().with(|s| match s {
                    Sheet::Shown(shown) if shown.id == id => shown.pages.clone(),
                    _ => Vec::new(),
                })
            },
            |route: &Route| route.key.clone(),
            move |route, depth| {
                let route = route.get_untracked();
                (route.screen)().on_dismiss(move || {
                    sheet().update(|s| s.pop_to(id, depth.get_untracked()));
                })
            },
        );
        let clean = move || {
            sheet().with(|s| s.form().is_some_and(|(_, form)| form == Form::Clean))
        };
        Some(
            dialog()
                .presentation(ModalPresentation::PageSheet)
                // The app decides: only a clean form closes natively.
                .close_on_escape(false)
                .close_on_swipe(clean)
                .on_cancel(cancel)
                .on_close(move || Handle(id).close())
                .child(nav_stack().flex_1().child(pages)),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(key: &'static str) -> Route {
        Route {
            key: key.into(),
            screen: Rc::new(|| unreachable!("not rendered")),
        }
    }

    fn keys(sheet: &Sheet) -> Vec<String> {
        match sheet {
            Sheet::Shown(shown) => shown.pages.iter().map(|p| p.key.to_string()).collect(),
            Sheet::Hidden => Vec::new(),
        }
    }

    #[test]
    fn a_presentation_holds_its_pages_until_it_closes() {
        let mut sheet = Sheet::default();
        assert!(sheet.present(1, route("new-bot")));
        assert!(!sheet.present(2, route("edit-bot")), "one sheet at a time");
        sheet.push(1, route("model"));
        sheet.push(1, route("model"));
        assert_eq!(keys(&sheet), ["new-bot", "model"]);
        sheet.pop_to(1, 0);
        assert_eq!(keys(&sheet), ["new-bot"], "never below the root");
        sheet.set_form(1, Form::Saving);
        assert_eq!(sheet.form(), Some((1, Form::Saving)));
        assert!(sheet.close(1));
        assert!(sheet.form().is_none());
    }

    #[test]
    fn a_closed_presentation_cannot_touch_the_next() {
        let mut sheet = Sheet::default();
        sheet.present(1, route("new-bot"));
        sheet.close(1);
        sheet.present(2, route("edit-bot"));
        // Late calls from the first: a discard confirmed, a save answered.
        sheet.set_form(1, Form::Unsaved);
        sheet.push(1, route("model"));
        assert!(!sheet.close(1));
        assert_eq!(sheet.form(), Some((2, Form::Clean)));
        assert_eq!(keys(&sheet), ["edit-bot"]);
    }
}
