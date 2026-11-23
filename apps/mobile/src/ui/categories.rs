//! Categories group the bot list, as the desktop's sidebar does: the list
//! of them, a screen to create one or to rename, move or delete it, and the
//! picker that moves a bot into one. The server keeps them and their order
//! for every device; the bot list follows its live updates.

use else_wire::bots::Category;
use nsh_base::{alert_dialog, input, nav_page};
use nsh_core::element::header::HeaderItem;
use nsh_core::prelude::*;
use uuid::Uuid;

use super::list;
use super::parts::{self, colors};
use super::session;
use else_client::request::Request;

const LIST: &str = "categories";

fn categories() -> Vec<Category> {
    session::bots().with(|b| b.categories.clone())
}

fn count(category: Option<Uuid>) -> usize {
    session::bots().with(|b| {
        b.list
            .iter()
            .filter(|bot| bot.is_active())
            .filter(|bot| {
                else_client::layout::known_category(&b.categories, bot.category_id) == category
            })
            .count()
    })
}

fn bots_label(n: usize) -> String {
    match n {
        1 => "1 bot".to_owned(),
        n => format!("{n} bots"),
    }
}

/// The categories, in the owner's order, each opening its editor.
pub fn open() {
    super::push(LIST, list_screen);
}

fn list_screen() -> Screen {
    let body = dynamic(move || {
        let rows: Vec<AnyElement> = categories()
            .into_iter()
            .map(|category| {
                let id = category.id;
                list::link_row(
                    category.name.clone(),
                    bots_label(count(Some(id))),
                    Some(move || open_editor(Some(id))),
                )
            })
            .chain(std::iter::once(list::action_row(
                "New Category",
                false,
                || true,
                || open_editor(None),
            )))
            .collect();
        Some(list::section(
            None,
            rows,
            Some(
                "Categories group your bots here and on the desktop. Move a bot into one from its details."
                    .to_owned(),
            ),
        ))
    });
    nav_page()
        .key(LIST)
        .title("Categories")
        .header_large_title(false)
        .child(
            scroll()
                .flex_1()
                .bg(colors::background())
                .child(view().py_6().child(body)),
        )
}

fn editor_key(category: Option<Uuid>) -> String {
    category.map_or_else(|| "category-new".to_owned(), |id| format!("category-{id}"))
}

fn open_editor(category: Option<Uuid>) {
    super::push(editor_key(category), move || editor(category));
}

/// What the editor asks of the server.
#[derive(Debug, Clone, PartialEq)]
enum Edit {
    /// Under its request ID, so a resend creates it once.
    Create(Uuid, String),
    Rename(String),
    Reorder(Vec<Uuid>),
    Delete,
}

/// Creates a category (`None`) or edits one: its name, its place in the
/// list, or deleting it, which moves its bots to Unassigned.
fn editor(category: Option<Uuid>) -> Screen {
    let existing = category.and_then(|id| categories().into_iter().find(|c| c.id == id));
    let name = signal(
        existing
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_default(),
    );
    let request = signal(Request::<Edit>::Idle);
    let idle = move || !request.with(Request::busy);
    let close = move || {
        super::routes().update(|r| r.0.retain(|route| *route.key != *editor_key(category)));
    };
    let send = move |edit: Edit, leave: bool| {
        let client = session::server().client.clone();
        session::submit(
            request,
            edit,
            move |edit| async move {
                match (edit, category) {
                    (Edit::Create(request_id, name), _) => {
                        client.create_category(request_id, name).await.map(drop)
                    }
                    (Edit::Rename(name), Some(id)) => {
                        client.rename_category(id, name).await.map(drop)
                    }
                    (Edit::Reorder(ids), _) => client.reorder_categories(ids).await.map(drop),
                    (Edit::Delete, Some(id)) => client.delete_category(id).await,
                    _ => Ok(()),
                }
            },
            move |()| {
                // The bots channel brings the new list.
                session::refresh_bots();
                request.try_set(Request::Idle);
                if leave {
                    close();
                }
            },
        );
    };
    let save = move || {
        let wanted = name.get_untracked().trim().to_owned();
        if wanted.is_empty() {
            return;
        }
        match category {
            None => {
                // The same name again resends the unconfirmed create.
                let request_id = request.with_untracked(|r| match r.unconfirmed_op() {
                    Some(Edit::Create(id, unsent)) if *unsent == wanted => *id,
                    _ => Uuid::new_v4(),
                });
                send(Edit::Create(request_id, wanted), true);
            }
            Some(_) => send(Edit::Rename(wanted), true),
        }
    };
    let changed = {
        let before = existing
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_default();
        computed(move || {
            let now = name.get().trim().to_owned();
            !now.is_empty() && now != before
        })
    };
    let save_item = move || {
        vec![
            HeaderItem::new(if category.is_some() { "Save" } else { "Create" })
                .key("save")
                .disabled(!(changed.get() && idle()))
                .on_press(save),
        ]
    };
    let shift = move |by: isize| {
        let Some(id) = category else { return };
        let mut ids: Vec<Uuid> = categories().iter().map(|c| c.id).collect();
        let Some(at) = ids.iter().position(|c| *c == id) else {
            return;
        };
        let to = at.saturating_add_signed(by).min(ids.len() - 1);
        if to == at {
            return;
        }
        ids.remove(at);
        ids.insert(to, id);
        send(Edit::Reorder(ids), false);
    };
    let place = move || {
        let ids: Vec<Uuid> = session::bots().with(|b| b.categories.iter().map(|c| c.id).collect());
        let at = category.and_then(|id| ids.iter().position(|c| *c == id));
        (at, ids.len())
    };
    let delete = move || {
        let Some(id) = category else { return };
        let label = categories()
            .into_iter()
            .find(|c| c.id == id)
            .map(|c| c.name)
            .unwrap_or_default();
        let moved = count(Some(id));
        spawn(async move {
            let choice = alert_dialog()
                .title(format!("Delete {label}?"))
                .description(match moved {
                    0 => "Only the grouping is deleted. It has no bots.".to_owned(),
                    1 => "Only the grouping is deleted. Its bot moves to Unassigned.".to_owned(),
                    n => format!("Only the grouping is deleted. Its {n} bots move to Unassigned."),
                })
                .cancel("cancel", "Cancel")
                .destructive("delete", "Delete")
                .show()
                .await;
            if matches!(choice, Ok(r) if r.action.as_deref() == Some("delete")) {
                send(Edit::Delete, true);
            }
        });
    };
    let message = dynamic(move || {
        request
            .with(|r| r.error().map(str::to_owned))
            .map(parts::footnote)
    });
    let actions = category.map(|_| {
        view()
            .child(list::card([
                list::action_row(
                    "Move Up",
                    false,
                    move || idle() && place().0.is_some_and(|at| at > 0),
                    move || shift(-1),
                ),
                list::action_row(
                    "Move Down",
                    false,
                    move || idle() && matches!(place(), (Some(at), n) if at + 1 < n),
                    move || shift(1),
                ),
            ]))
            .child(parts::footnote("The order shows here and on the desktop."))
            .child(view().pt_6().child(list::card([list::action_row(
                "Delete Category",
                true,
                idle,
                delete,
            )])))
    });
    nav_page()
        .key(editor_key(category))
        .title(if category.is_some() {
            "Category"
        } else {
            "New Category"
        })
        .header_large_title(false)
        .header_right_items(save_item)
        .child(
            scroll()
                .flex_1()
                .bg(colors::background())
                .keyboard_taps(KeyboardTaps::Handled)
                .child(
                    view()
                        .py_6()
                        .gap_6()
                        .child(
                            view()
                                .child(
                                    parts::group().child(
                                        input()
                                            .px_4()
                                            .min_h(px(44.0))
                                            .value(name)
                                            .on_change(move |t: &str| name.set(t.to_owned()))
                                            .placeholder("Name")
                                            .auto_focus(category.is_none())
                                            .auto_capitalize(AutoCapitalize::Words)
                                            .return_key_type(ReturnKeyType::Done)
                                            .on_submit(move |_: &str| {
                                                if changed.get_untracked() {
                                                    save()
                                                }
                                            })
                                            .text_size(px(17.0))
                                            .text_color(colors::label())
                                            .placeholder_color(|| color(colors::secondary()))
                                            .caret_color(|| color(colors::tint()))
                                            .label("Name"),
                                    ),
                                )
                                .child(message),
                        )
                        .child(actions),
                ),
        )
}

/// Moves `bot` into a category, or out to Unassigned.
pub fn open_picker(bot: Uuid) {
    super::push(format!("category-of-{bot}"), move || picker(bot));
}

fn picker(bot: Uuid) -> Screen {
    let request = signal(Request::<Option<Uuid>>::Idle);
    let current = computed(move || {
        session::bots().with(|b| {
            b.list
                .iter()
                .find(|it| it.id == bot)
                .and_then(|it| else_client::layout::known_category(&b.categories, it.category_id))
        })
    });
    let pick = move |category: Option<Uuid>| {
        if current.get_untracked() == category {
            return;
        }
        let client = session::server().client.clone();
        session::submit(
            request,
            category,
            move |category| client.set_category(bot, category),
            move |saved| {
                session::update_bot(saved);
                request.try_set(Request::Idle);
            },
        );
    };
    let rows = dynamic(move || {
        let options: Vec<AnyElement> = std::iter::once((None, "Unassigned".to_owned()))
            .chain(categories().into_iter().map(|c| (Some(c.id), c.name)))
            .map(|(id, label)| {
                list::option_row(
                    label,
                    None,
                    move || current.get() == id,
                    true,
                    move || pick(id),
                )
            })
            .chain(std::iter::once(list::action_row(
                "New Category",
                false,
                || true,
                || open_editor(None),
            )))
            .collect();
        Some(list::section(None, options, None))
    });
    let message = dynamic(move || {
        request
            .with(|r| r.error().map(str::to_owned))
            .map(parts::footnote)
    });
    nav_page()
        .key(format!("category-of-{bot}"))
        .title("Category")
        .header_large_title(false)
        .child(
            scroll()
                .flex_1()
                .bg(colors::background())
                .child(view().py_6().child(rows).child(message)),
        )
}
