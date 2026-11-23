//! Creating a bot: its name and its avatar's shape and color. It starts
//! with the server's default provider and model.

use else_wire::bots::{Avatar, AvatarColor, AvatarShape};
use nsh_base::{input, nav_page};
use nsh_core::element::header::HeaderItem;
use nsh_core::prelude::*;
use uuid::Uuid;

use super::parts::{self, colors};
use super::session;
use crate::api::ClientError;

pub const KEY: &str = "new-bot";

/// The create request. One whose outcome is unknown keeps its request ID,
/// so creating the same name again cannot make two bots.
#[derive(Clone, PartialEq)]
enum Step {
    Editing,
    Creating,
    Failed(String),
    Unknown {
        request_id: Uuid,
        tried: (String, Avatar),
        message: String,
    },
}

impl Step {
    /// The ID to create with: the unresolved one when name and avatar are
    /// unchanged, since the server takes a repeat of it once.
    fn request_id(&self, name: &str, avatar: Avatar) -> Uuid {
        match self {
            Self::Unknown {
                request_id, tried, ..
            } if tried.0 == name && tried.1 == avatar => *request_id,
            _ => Uuid::new_v4(),
        }
    }
}

pub fn open() {
    super::push(KEY, screen);
}

fn screen() -> Screen {
    let name = signal(String::new());
    let picked = signal(AvatarColor::ALL[usize::from(Uuid::new_v4().as_bytes()[0]) % 12]);
    let shape = signal(AvatarShape::ALL[usize::from(Uuid::new_v4().as_bytes()[0]) % 12]);
    let look = move || Avatar {
        shape: shape.get(),
        color: picked.get(),
    };
    let step = signal(Step::Editing);
    let ready =
        computed(move || !name.with(|n| n.trim().is_empty()) && step.get() != Step::Creating);

    let create = move || {
        if !ready.get_untracked() {
            return;
        }
        let name_now = name.get_untracked().trim().to_owned();
        let avatar = Avatar {
            shape: shape.get_untracked(),
            color: picked.get_untracked(),
        };
        let request_id = step.with_untracked(|s| s.request_id(&name_now, avatar));
        step.set(Step::Creating);
        // Held by the server, not by the header button (rebuilt when its
        // label changes) or this screen: leaving it does not cancel the
        // create, and the new bot still reaches the list.
        let client = session::server().client.clone();
        session::spawn_held(async move {
            match client
                .create_bot(request_id, name_now.clone(), avatar)
                .await
            {
                Ok(bot) => {
                    let (id, title) = (bot.id, bot.name.clone());
                    session::update_bot(bot);
                    if step.try_set(Step::Editing) {
                        super::routes().update(|r| r.0.retain(|route| route.key != KEY));
                        super::push(format!("chat-{id}"), move || {
                            super::chat::screen(id, title.clone())
                        });
                    }
                }
                Err(err) if err.outcome_unknown() => {
                    step.try_set(Step::Unknown {
                        request_id,
                        message: format!(
                            "{err} The bot may have been created. Creating it again unchanged cannot make two."
                        ),
                        tried: (name_now, avatar),
                    });
                }
                Err(ClientError::Api { message, .. }) => {
                    step.try_set(Step::Failed(message));
                }
                Err(err) => {
                    step.try_set(Step::Failed(err.to_string()));
                }
            }
        });
    };

    let create_item = move || {
        vec![
            HeaderItem::new(if step.get() == Step::Creating {
                "Creating…"
            } else {
                "Create"
            })
            .key("create")
            .disabled(!ready.get())
            .on_press(create),
        ]
    };

    let shapes = picker(AvatarShape::ALL, shape, AvatarShape::label, move |choice| {
        parts::avatar(
            move || Avatar {
                shape: choice,
                color: picked.get(),
            },
            34.0,
        )
    });
    let swatches = picker(AvatarColor::ALL, picked, AvatarColor::label, |choice| {
        let swatch = view()
            .size(px(28.0))
            .rounded(px(14.0))
            .bg(rgb(choice.rgb()));
        if choice == AvatarColor::White {
            swatch.border(px(0.5)).border_color(colors::separator())
        } else {
            swatch
        }
    });

    let message = dynamic(move || match step.get() {
        Step::Failed(message) | Step::Unknown { message, .. } => Some(
            text(message)
                .mx_4()
                .text_size(px(13.0))
                .line_height(px(18.0))
                .text_color(colors::destructive()),
        ),
        Step::Editing | Step::Creating => None,
    });

    nav_page()
        .key(KEY)
        .title("New Bot")
        .header_large_title(false)
        .header_right_items(create_item)
        .child(
            scroll()
                .flex_1()
                .bg(colors::background())
                .keyboard_taps(KeyboardTaps::Handled)
                .child(
                    view()
                        .py_6()
                        .gap_6()
                        .child(view().items_center().child(parts::avatar(look, 96.0)))
                        .child(
                            parts::group().child(
                                input()
                                    .px_4()
                                    .min_h(px(44.0))
                                    .value(name)
                                    .on_change(move |t: &str| {
                                        name.set(t.to_owned());
                                        if matches!(step.get_untracked(), Step::Failed(_)) {
                                            step.set(Step::Editing);
                                        }
                                    })
                                    .placeholder("Name")
                                    .auto_focus(true)
                                    .auto_capitalize(AutoCapitalize::Words)
                                    .return_key_type(ReturnKeyType::Done)
                                    .on_submit(move |_: &str| create())
                                    .text_size(px(17.0))
                                    .text_color(colors::label())
                                    .placeholder_color(|| color(colors::secondary()))
                                    .caret_color(|| color(colors::tint()))
                                    .label("Name"),
                            ),
                        )
                        .child(message)
                        .child(parts::group().p_4().child(shapes))
                        .child(parts::group().p_4().child(swatches)),
                ),
        )
}

/// A wrapping row of choices, the selected one ringed.
fn picker<T: Copy + PartialEq + 'static, E: IntoElement>(
    choices: impl IntoIterator<Item = T>,
    selected: Signal<T>,
    label: fn(T) -> &'static str,
    draw: impl Fn(T) -> E,
) -> View {
    view()
        .flex_row()
        .flex_wrap()
        .gap_3()
        .justify_center()
        .children(choices.into_iter().map(move |choice| {
            let chosen = move || selected.get() == choice;
            view()
                .size(px(42.0))
                .rounded(px(21.0))
                .items_center()
                .justify_center()
                .border_2()
                .border_color(move || {
                    if chosen() {
                        colors::label()
                    } else {
                        colors::card()
                    }
                })
                .role(AccessibilityRole::Button)
                .label(move || {
                    let mark = if chosen() { ", selected" } else { "" };
                    format!("{}{mark}", label(choice))
                })
                .on_press(move || selected.set(choice))
                .child(draw(choice))
                .into_element()
        }))
}
