//! A bot's request for credentials, saved in the server's vault.

use std::collections::BTreeMap;

use else_client::api::{SaveSecret, validate_secret_metadata};
use else_wire::secrets::SecretRequest;
use nsh_base::input;
use nsh_core::prelude::*;
use uuid::Uuid;

use super::super::parts::colors;
use super::super::session;
use super::{Fill, button, eyebrow, failure, frame};
use else_client::request::Request;

/// A bot's request for credentials: one masked field per variable it
/// named. Save stores them in the server's vault, where every bot may use
/// them; the card never shows a stored value again.
pub(super) fn card(bot: Uuid, asked: SecretRequest) -> impl IntoElement {
    let request = signal(Request::<Option<SaveSecret>>::Idle);
    let idle = move || !request.with(Request::busy);
    let values = signal(BTreeMap::<String, String>::new());
    let names = asked.env.clone();
    let complete = {
        let names = names.clone();
        computed(move || {
            values.with(|v| {
                names
                    .iter()
                    .all(|n| v.get(n).is_some_and(|s| !s.is_empty()))
            })
        })
    };
    let id = asked.id;
    // Saved with the description and website the bot gave, as the desktop
    // saves it; the phone does not edit them.
    let meta = stored((
        asked.description.clone().unwrap_or_default(),
        asked.website.clone().unwrap_or_default(),
    ));
    let answer = move |env: Option<BTreeMap<String, String>>| {
        let (description, website) = meta.get();
        let secret = match env.map(|env| {
            validate_secret_metadata(&description, &website).map(|website| SaveSecret {
                env,
                description: description.clone(),
                website,
            })
        }) {
            Some(Ok(secret)) => Some(secret),
            Some(Err(message)) => {
                request.update(|r| r.reject(message));
                return;
            }
            None => None,
        };
        // The values leave the fields with the request: a lost response
        // never leaves them on screen.
        values.set(BTreeMap::new());
        let client = session::server().client.clone();
        // Once accepted, the card keeps no copy of the values while it waits
        // for the server's update to remove it.
        session::submit(
            request,
            secret,
            move |secret| client.answer_secret(bot, id, secret),
            move |()| {
                request.try_set(Request::Accepted(None));
            },
        );
    };
    let fields: Vec<AnyElement> = names
        .iter()
        .map(|name| {
            let key = name.clone();
            let shown = name.clone();
            view()
                .gap_1()
                .child(
                    text(name.clone())
                        .text_size(px(13.0))
                        .font_family("Menlo")
                        .text_color(colors::secondary()),
                )
                .child(
                    input()
                        .min_h(px(40.0))
                        .px_3()
                        .rounded(px(10.0))
                        .bg(colors::fill())
                        .value(move || values.with(|v| v.get(&shown).cloned().unwrap_or_default()))
                        .on_change(move |t: &str| {
                            values.update(|v| {
                                v.insert(key.clone(), t.to_owned());
                            })
                        })
                        .masked(true)
                        .auto_capitalize(AutoCapitalize::None)
                        .auto_correct(false)
                        .text_size(px(15.0))
                        .text_color(colors::label())
                        .caret_color(|| color(colors::tint()))
                        .label(name.clone()),
                )
                .into_element()
        })
        .collect();
    let detail = asked.description.clone().filter(|d| !d.trim().is_empty());
    frame()
        .child(eyebrow("NEEDS A SECRET"))
        .child(
            text(asked.name.clone())
                .text_size(px(17.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors::label()),
        )
        .child(
            text(asked.reason.clone())
                .text_size(px(15.0))
                .line_height(px(20.0))
                .text_color(colors::label()),
        )
        .child(detail.map(|detail| {
            text(detail)
                .text_size(px(15.0))
                .line_height(px(20.0))
                .text_color(colors::secondary())
        }))
        .child(asked.website.clone().map(|site| {
            text(format!("For {site}"))
                .text_size(px(15.0))
                .text_color(colors::secondary())
        }))
        .child(view().gap_3().children(fields))
        .child(
            text("Saved in the server's vault, where every bot can use it.")
                .text_size(px(13.0))
                .text_color(colors::secondary()),
        )
        .child(
            view()
                .flex_row()
                .gap_2()
                .child(button("Decline", Fill::PlainDestructive, idle, move || {
                    answer(None)
                }))
                .child(button(
                    "Save",
                    Fill::Prominent,
                    move || idle() && complete.get(),
                    move || answer(Some(values.get_untracked())),
                )),
        )
        .child(failure(request))
}
