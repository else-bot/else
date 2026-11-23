//! A bot asking for a Telegram, Discord or WhatsApp connection. Telegram
//! and Discord take a bot token; WhatsApp links the owner's account as a
//! device through a code scanned in WhatsApp. A connection the server
//! already has can be used instead. Done, the setup records which
//! connection the bot may use.

use std::time::Duration;

use else_client::api::socials::PairingTarget;
use else_client::request::Request;
use else_wire::socials::{Connection, Pairing, Provider, SocialSetup};
use nsh_base::input;
use nsh_core::element::ImageSrc;
use nsh_core::prelude::*;
use uuid::Uuid;

use super::super::parts::colors;
use super::super::session;
use super::{Fill, button, eyebrow, failure, frame};

/// A bot token typed for the setup. It never prints, even in a debug log.
#[derive(Clone, PartialEq)]
struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

/// How the card settles the setup. Both are safe to repeat: the server
/// returns the settled setup again for the same connection.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    /// Finish with this connection.
    Use(Uuid),
    Cancel,
}

pub(super) fn card(setup: SocialSetup) -> impl IntoElement {
    // Creating the connection is its own request: once it exists, a retry
    // only finishes the setup with it and never creates it twice.
    let create = signal(Request::<Token>::Idle);
    let request = signal(Request::<Step>::Idle);
    let idle = move || !request.with(Request::busy) && !create.with(Request::busy);
    let id = setup.id;
    let provider = setup.provider;
    let token = signal(String::new());
    let existing = resource(
        || Some(()),
        move |(), _| {
            let client = session::server().client.clone();
            async move {
                client
                    .social_connections()
                    .await
                    .map(|all| {
                        all.into_iter()
                            .filter(|c| c.provider == provider)
                            .collect::<Vec<Connection>>()
                    })
                    .map_err(|err| err.to_string())
            }
        },
    );
    let send = move |step: Step| {
        let client = session::server().client.clone();
        session::submit(
            request,
            step,
            move |step| async move {
                match step {
                    Step::Use(connection) => {
                        client.complete_social_setup(id, connection, None).await
                    }
                    Step::Cancel => client.cancel_social_setup(id).await,
                }
            },
            |()| {},
        );
    };
    let connect = move |typed: Token| {
        // The token leaves the field with the request, and the request
        // drops it once the server answered.
        token.set(String::new());
        let client = session::server().client.clone();
        session::submit(
            create,
            typed,
            move |Token(token)| async move {
                match client.connect_social(provider, token).await {
                    // An earlier try whose answer was lost may have
                    // created it: list it so the owner can use it.
                    Err(err) if err.code() == Some("social_conflict") => {
                        existing.refetch();
                        Err(err)
                    }
                    result => result,
                }
            },
            move |connection: Connection| {
                create.try_set(Request::Idle);
                existing.refetch();
                send(Step::Use(connection.id));
            },
        );
    };
    let reuse = dynamic(move || {
        let connections = existing.get().filter(|c| !c.is_empty())?;
        Some(
            view()
                .gap_2()
                .child(
                    text("Or use one already connected")
                        .text_size(px(13.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors::secondary()),
                )
                .children(
                    connections
                        .into_iter()
                        .map(|connection| {
                            let label = match &connection.username {
                                Some(user) => format!("Use {} (@{user})", connection.display_name),
                                None => format!("Use {}", connection.display_name),
                            };
                            button(label, Fill::Plain, idle, move || {
                                send(Step::Use(connection.id))
                            })
                            .into_element()
                        })
                        .collect::<Vec<_>>(),
                ),
        )
    });
    let entry = if provider.pairs() {
        pairing(id, request).into_element()
    } else {
        view()
            .gap_2()
            .child(
                input()
                    .min_h(px(40.0))
                    .px_3()
                    .rounded(px(10.0))
                    .bg(colors::fill())
                    .value(token)
                    .on_change(move |t: &str| token.set(t.to_owned()))
                    .masked(true)
                    .auto_capitalize(AutoCapitalize::None)
                    .auto_correct(false)
                    .placeholder("Bot token")
                    .text_size(px(15.0))
                    .text_color(colors::label())
                    .caret_color(|| color(colors::tint()))
                    .label(format!("{} bot token", provider.label())),
            )
            .child(provider.portal().map(|(label, url)| {
                button(
                    label,
                    Fill::Plain,
                    || true,
                    move || {
                        spawn(async move {
                            let _ = open_url(url).await;
                        });
                    },
                )
            }))
            .child(
                view()
                    .flex_row()
                    .gap_2()
                    .child(button("Decline", Fill::PlainDestructive, idle, move || {
                        send(Step::Cancel)
                    }))
                    .child(button(
                        "Connect",
                        Fill::Prominent,
                        move || idle() && token.with(|t| !t.trim().is_empty()),
                        move || connect(Token(token.get_untracked().trim().to_owned())),
                    )),
            )
            .into_element()
    };
    frame()
        .child(eyebrow(match provider {
            Provider::WhatsApp => "LINK WHATSAPP",
            _ => "CONNECT A BOT ACCOUNT",
        }))
        .child(
            text(format!("Connect {}", provider.label()))
                .text_size(px(17.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors::label()),
        )
        .child(
            text(setup.reason.clone())
                .text_size(px(15.0))
                .line_height(px(20.0))
                .text_color(colors::label()),
        )
        .child(
            text(provider.token_help())
                .text_size(px(13.0))
                .line_height(px(18.0))
                .text_color(colors::secondary()),
        )
        .child(entry)
        .child(reuse)
        .child(failure(create))
        .child(failure(request))
}

/// WhatsApp: a code to scan in WhatsApp → Linked devices, renewed by the
/// server until scanned; then Finish records the linked account.
fn pairing(setup: Uuid, request: Signal<Request<Step>>) -> impl IntoElement {
    let idle = move || !request.with(Request::busy);
    let state = signal(None::<Pairing>);
    let error = signal(None::<String>);
    let round = signal(0_u32);
    // Follows the pairing while the card shows: the server answers once
    // it changed past the revision the card has.
    effect(move || {
        round.get();
        state.set(None);
        let client = session::server().client.clone();
        spawn(async move {
            let target = PairingTarget::Setup(setup);
            let mut next = client.clone().start_social_pairing(target).await;
            loop {
                match next {
                    Ok(view) => {
                        error.try_set(None);
                        let finished = view.pairing.finished();
                        if !state.try_set(Some(view.pairing)) || finished {
                            return;
                        }
                        next = client.clone().social_pairing(target, view.revision).await;
                    }
                    Err(err) => {
                        if !error.try_set(Some(err.to_string())) {
                            return;
                        }
                        timer(Duration::from_secs(3)).await;
                        next = client.clone().start_social_pairing(target).await;
                    }
                }
            }
        });
    });
    let body = dynamic(move || {
        let shown = state.get();
        untracked(|| {
            match shown {
            None | Some(Pairing::Starting) => text("Getting a code…")
                .text_size(px(15.0))
                .text_color(colors::secondary())
                .into_element(),
            Some(Pairing::Code { code, .. }) => view()
                .gap_2()
                .items_center()
                .child(qr(&code).map(|markup| {
                    svg(ImageSrc::svg_markup(&markup))
                        .w(px(220.0))
                        .h(px(220.0))
                        .rounded(px(8.0))
                        .bg(rgb(0xffffff))
                        .label("WhatsApp pairing code")
                }))
                .child(
                    text("In WhatsApp on the phone that has your account, open Settings → Linked devices → Link a device, then scan this code.")
                        .text_size(px(13.0))
                        .line_height(px(18.0))
                        .text_center()
                        .text_color(colors::secondary()),
                )
                .into_element(),
            Some(Pairing::Paired { connection }) => {
                let id = connection.id;
                view()
                    .gap_2()
                    .child(
                        text(format!("Linked {}", connection.display_name))
                            .text_size(px(15.0))
                            .text_color(colors::label()),
                    )
                    .child(button("Finish", Fill::Prominent, idle, move || {
                        finish(setup, request, id)
                    }))
                    .into_element()
            }
            Some(Pairing::Expired) => view()
                .gap_2()
                .child(
                    text("The code expired before it was scanned.")
                        .text_size(px(15.0))
                        .text_color(colors::secondary()),
                )
                .child(button("New code", Fill::Prominent, || true, move || {
                    round.update(|r| *r += 1)
                }))
                .into_element(),
            Some(Pairing::Failed { message }) => view()
                .gap_2()
                .child(
                    text(message)
                        .text_size(px(15.0))
                        .text_color(colors::destructive()),
                )
                .child(button("Try again", Fill::Prominent, || true, move || {
                    round.update(|r| *r += 1)
                }))
                .into_element(),
            Some(_) => text("This pairing state is not supported by this app.")
                .text_size(px(15.0))
                .text_color(colors::secondary())
                .into_element(),
        }
        })
    });
    view()
        .gap_2()
        .child(body)
        .child(dynamic(move || {
            error.get().map(|message| {
                text(message)
                    .text_size(px(13.0))
                    .text_color(colors::destructive())
            })
        }))
        .child(button("Decline", Fill::PlainDestructive, idle, move || {
            let client = session::server().client.clone();
            session::submit(
                request,
                Step::Cancel,
                move |_| client.cancel_social_setup(setup),
                |()| {},
            );
        }))
}

fn finish(setup: Uuid, request: Signal<Request<Step>>, connection: Uuid) {
    let client = session::server().client.clone();
    session::submit(
        request,
        Step::Use(connection),
        move |_| client.complete_social_setup(setup, connection, None),
        |()| {},
    );
}

/// `code` as a QR code's SVG, with its quiet zone.
fn qr(code: &str) -> Option<String> {
    Some(
        qrcode::QrCode::new(code.as_bytes())
            .ok()?
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(220, 220)
            .quiet_zone(true)
            .build(),
    )
}
