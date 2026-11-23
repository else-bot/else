//! Creating a bot: its name and color. It starts with the server's default
//! provider and model; the desktop app changes those.

use else_wire::bots::{Avatar, AvatarColor, AvatarShape};
use nsh_base::{input, nav_page};
use nsh_core::element::header::HeaderItem;
use nsh_core::prelude::*;
use uuid::Uuid;

use super::parts::{self, colors};
use super::session;
use crate::api::ClientError;
use crate::bots;

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
        name: String,
        message: String,
    },
}

impl Step {
    fn request_id(&self, name: &str) -> Uuid {
        match self {
            Self::Unknown {
                request_id,
                name: tried,
                ..
            } if tried == name => *request_id,
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
    let shape = AvatarShape::ALL[usize::from(Uuid::new_v4().as_bytes()[0]) % 12];
    let step = signal(Step::Editing);
    let ready =
        computed(move || !name.with(|n| n.trim().is_empty()) && step.get() != Step::Creating);

    // The request runs under the screen, not under the header button that
    // starts it: the header is rebuilt when its label changes, which would
    // end a task it owned.
    let attempt = signal(None::<(Uuid, String, Avatar)>);
    effect(move || {
        let Some((request_id, name_now, avatar)) = attempt.get() else {
            return;
        };
        let client = session::server().client.clone();
        spawn(async move {
            match client.create_bot(request_id, name_now.clone(), avatar).await {
                Ok(bot) => {
                    let (id, title) = (bot.id, bot.name.clone());
                    session::update_bot(bot);
                    super::routes().update(|r| r.0.retain(|route| route.key != KEY));
                    super::push(format!("chat-{id}"), move || {
                        super::chat::screen(id, title.clone())
                    });
                }
                Err(err) if err.outcome_unknown() => step.set(Step::Unknown {
                    request_id,
                    message: format!(
                        "{err} The bot may have been created. Creating the same name again cannot make two."
                    ),
                    name: name_now,
                }),
                Err(ClientError::Api { message, .. }) => step.set(Step::Failed(message)),
                Err(err) => step.set(Step::Failed(err.to_string())),
            }
        });
    });
    let create = move || {
        if !ready.get_untracked() {
            return;
        }
        let name_now = name.get_untracked().trim().to_owned();
        let request_id = step.with_untracked(|s| s.request_id(&name_now));
        step.set(Step::Creating);
        let avatar = Avatar {
            shape,
            color: picked.get_untracked(),
        };
        attempt.set(Some((request_id, name_now, avatar)));
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

    let initial = move || {
        SharedString::from(
            name.with(|n| n.chars().find(|c| c.is_alphanumeric()))
                .map(|c| c.to_uppercase().collect::<String>())
                .unwrap_or_else(|| "?".into()),
        )
    };

    let swatches = view()
        .flex_row()
        .flex_wrap()
        .gap_3()
        .justify_center()
        .children(AvatarColor::ALL.into_iter().map(move |choice| {
            view()
                .size(px(36.0))
                .rounded(px(18.0))
                .items_center()
                .justify_center()
                .border_2()
                .border_color(move || {
                    if picked.get() == choice {
                        colors::label()
                    } else {
                        colors::card()
                    }
                })
                .role(AccessibilityRole::Button)
                .label(move || {
                    let mark = if picked.get() == choice {
                        ", selected"
                    } else {
                        ""
                    };
                    format!("{choice:?}{mark}")
                })
                .on_press(move || picked.set(choice))
                .child(
                    view()
                        .size(px(28.0))
                        .rounded(px(14.0))
                        .bg(rgb(bots::avatar_rgb(choice))),
                )
                .into_element()
        }));

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
                        .child(
                            view().items_center().child(parts::avatar(
                                initial,
                                move || bots::avatar_rgb(picked.get()),
                                88.0,
                            )),
                        )
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
                        .child(parts::group().p_4().child(swatches))
                        .child(parts::footnote(
                            "The bot uses the server's default provider and model. Change them in the desktop app.",
                        )),
                ),
        )
}
