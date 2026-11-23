//! Renaming a bot and changing its avatar. Saving the same name and avatar
//! again leaves the bot as it is, so a resend after a lost answer is safe.

use else_wire::bots::{Avatar, Bot};
use nsh_base::{input, nav_page};
use nsh_core::element::header::HeaderItem;
use nsh_core::prelude::*;
use uuid::Uuid;

use super::parts::{self, colors};
use super::session;
use else_client::request::Request;

fn key(bot: Uuid) -> String {
    format!("edit-bot-{bot}")
}

pub fn open(bot: Bot) {
    let id = bot.id;
    super::push(key(id), move || screen(bot.clone()));
}

fn screen(bot: Bot) -> Screen {
    let id = bot.id;
    let name = signal(bot.name.clone());
    let shape = signal(bot.avatar.shape);
    let fill = signal(bot.avatar.color);
    let look = move || Avatar {
        shape: shape.get(),
        color: fill.get(),
    };
    let save = signal(Request::<(String, Avatar)>::Idle);
    let saved = stored((bot.name.clone(), bot.avatar));
    let changed = computed(move || {
        let now = (name.get().trim().to_owned(), look());
        !now.0.is_empty() && now != saved.get()
    });
    let ready = computed(move || changed.get() && !save.with(Request::busy));
    let submit = move || {
        if !ready.get_untracked() {
            return;
        }
        let (before_name, before_avatar) = saved.get();
        let after = (name.get_untracked().trim().to_owned(), untracked(look));
        let client = session::server().client.clone();
        session::submit(
            save,
            after,
            move |(new_name, avatar)| {
                // Only what changed is sent.
                client.update_bot(
                    id,
                    (new_name != before_name).then_some(new_name),
                    (avatar != before_avatar).then_some(avatar),
                )
            },
            move |bot| {
                session::update_bot(bot);
                if save.try_set(Request::Idle) {
                    super::routes().update(|r| r.0.retain(|route| *route.key != *key(id)));
                }
            },
        );
    };
    let save_item = move || {
        vec![
            HeaderItem::new(if save.with(Request::busy) {
                "Saving…"
            } else {
                "Save"
            })
            .key("save")
            .disabled(!ready.get())
            .on_press(submit),
        ]
    };
    let (shapes, swatches) = super::new_bot::avatar_pickers(shape, fill);
    let message = dynamic(move || {
        let message = match save.get() {
            Request::Failed(message) => message,
            Request::Unconfirmed { error, .. } => {
                format!("{error} It may have been saved. Saving again is safe.")
            }
            _ => return None,
        };
        Some(
            text(message)
                .mx_4()
                .text_size(px(13.0))
                .line_height(px(18.0))
                .text_color(colors::destructive()),
        )
    });
    nav_page()
        .key(key(id))
        .title("Edit Bot")
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
                        .child(view().items_center().child(parts::avatar(look, 96.0)))
                        .child(
                            parts::group().child(
                                input()
                                    .px_4()
                                    .min_h(px(44.0))
                                    .value(name)
                                    .on_change(move |t: &str| name.set(t.to_owned()))
                                    .placeholder("Name")
                                    .auto_capitalize(AutoCapitalize::Words)
                                    .return_key_type(ReturnKeyType::Done)
                                    .on_submit(move |_: &str| submit())
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
