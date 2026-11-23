//! Creating a bot: its name, its avatar's shape and color, and its
//! provider and model, which start at the server's defaults.

use else_wire::bots::{Avatar, AvatarColor, AvatarShape};
use else_wire::settings::{Capabilities, ProfileChoice, ProviderInfo};
use nsh_base::{input, nav_page};
use nsh_core::element::header::HeaderItem;
use nsh_core::prelude::*;
use uuid::Uuid;

use super::list;
use super::model;
use super::parts::{self, colors};
use super::session;
use else_client::request::Request;

use crate::api::CreateBotRequest;

pub const KEY: &str = "new-bot";

/// The request that creates the bot the owner asked for: the one whose
/// outcome is unknown when it asks for the same bot again, which the server
/// takes once under its request ID, else a new one.
fn create_request(
    unconfirmed: Option<&CreateBotRequest>,
    name: String,
    avatar: Avatar,
    profile: Option<ProfileChoice>,
) -> CreateBotRequest {
    match unconfirmed {
        Some(request)
            if request.name == name
                && request.avatar == Some(avatar)
                && request.profile == profile =>
        {
            request.clone()
        }
        _ => CreateBotRequest {
            request_id: Uuid::new_v4(),
            name,
            profile,
            category_id: None,
            avatar: Some(avatar),
        },
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
    let step = signal(Request::<CreateBotRequest>::Idle);
    // The server's providers and defaults. Without them, the bot is
    // created with the defaults; the chat's Model screen changes them later.
    let caps = resource(
        || Some(()),
        |(), _| {
            let client = session::server().client.clone();
            async move { client.capabilities().await.map_err(|err| err.to_string()) }
        },
    );
    let model = signal(None::<ProfileChoice>);
    // Shown as the catalog applies the defaults now. When that differs
    // from the raw defaults, the shown one is sent.
    effect(move || {
        if let Some(caps) = caps.get() {
            let providers = caps.offered();
            model.set(caps.defaults.as_ref().and_then(|wanted| {
                providers
                    .iter()
                    .find(|p| p.state.id == wanted.provider_id)
                    .and_then(|p| p.choice(Some(wanted)))
            }));
        }
    });
    // Only a change from the defaults is sent: a bot without its own
    // profile keeps following the server's defaults.
    let profile = move || {
        let defaults = untracked(|| caps.get())?.defaults;
        model
            .get_untracked()
            .filter(|m| Some(m) != defaults.as_ref())
    };
    let ready = computed(move || !name.with(|n| n.trim().is_empty()) && !step.with(Request::busy));

    let create = move || {
        if !ready.get_untracked() {
            return;
        }
        let name_now = name.get_untracked().trim().to_owned();
        let avatar = Avatar {
            shape: shape.get_untracked(),
            color: picked.get_untracked(),
        };
        let request = step
            .with_untracked(|s| create_request(s.unconfirmed_op(), name_now, avatar, profile()));
        // Held by the server, not by the header button (rebuilt when its
        // label changes) or this screen: leaving it does not cancel the
        // create, and the new bot still reaches the list.
        let client = session::server().client.clone();
        session::submit(
            step,
            request,
            move |request| client.create_bot(request),
            move |bot| {
                let (id, title) = (bot.id, bot.name.clone());
                session::update_bot(bot);
                if step.try_set(Request::Idle) {
                    super::routes().update(|r| r.0.retain(|route| route.key != KEY));
                    super::push(format!("chat-{id}"), move || {
                        super::chat::screen(id, title.clone())
                    });
                }
            },
        );
    };

    let create_item = move || {
        vec![
            HeaderItem::new(if step.with(Request::busy) {
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

    let message = dynamic(move || {
        let message = match step.get() {
            Request::Failed(message) => message,
            Request::Unconfirmed { error, .. } => format!(
                "{error} The bot may have been created. Creating it again unchanged cannot make two."
            ),
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
                                        if matches!(step.get_untracked(), Request::Failed(_)) {
                                            step.set(Request::Idle);
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
                        .child(parts::group().p_4().child(swatches))
                        .child(model_row(caps, model)),
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
                .label(label(choice))
                .accessibility_state(move || AccessibilityState {
                    selected: chosen(),
                    ..AccessibilityState::default()
                })
                .on_press(move || selected.set(choice))
                .child(draw(choice))
                .into_element()
        }))
}

/// The Model row: the profile the bot starts with, opening the picker.
fn model_row(
    caps: Resource<Capabilities, String>,
    model: Signal<Option<ProfileChoice>>,
) -> impl IntoElement {
    dynamic(move || {
        let (detail, providers) = match (caps.get(), caps.error()) {
            (None, None) => ("Loading…".to_owned(), None),
            (None, Some(_)) => ("The server's default".to_owned(), None),
            (Some(caps), _) => match (caps.offered(), model.get()) {
                (providers, Some(choice)) => (model::summary(&providers, &choice), Some(providers)),
                (providers, None) if providers.is_empty() => {
                    ("No provider lists models".to_owned(), None)
                }
                (providers, None) => ("Choose".to_owned(), Some(providers)),
            },
        };
        let open = providers.map(|providers| {
            move || {
                let providers = providers.clone();
                super::push(format!("{KEY}-model"), move || {
                    picker_screen(providers.clone(), model)
                });
            }
        });
        Some(list::section(
            None,
            [list::link_row("Model", detail, open)],
            Some(
                "Starts at the server's default provider and model. Change it later from the bot's chat."
                    .to_owned(),
            ),
        ))
    })
}

fn picker_screen(providers: Vec<ProviderInfo>, model: Signal<Option<ProfileChoice>>) -> Screen {
    nav_page()
        .key(format!("{KEY}-model"))
        .title("Model")
        .header_large_title(false)
        .child(
            scroll()
                .flex_1()
                .bg(colors::background())
                .child(view().py_6().child(model::picker(providers, model))),
        )
}
