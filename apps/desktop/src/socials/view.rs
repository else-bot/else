use super::*;
use crate::controls::{Control as _, Scale, Tier, text_field};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;

impl SocialsView {
    fn render_receipt(&self, receipt: &Receipt, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let card = v_flex()
            .w_full()
            .gap_3()
            .p_4()
            .rounded(theme.radius_lg)
            .bg(theme.secondary);
        let Receipt::Ready(connection) = receipt else {
            return card.child(div().text_sm().child(match receipt {
                Receipt::Loading { .. } => "Loading connection…",
                _ => "This connection is no longer available. Manage connections in Settings → Socials.",
            })).into_any_element();
        };
        card.child(div().font_semibold().child(format!("{} connected", connection.provider.label())))
            .child(div().text_sm().child(connection.display_name.clone()))
            .children(connection.username.as_ref().map(|username| div().text_sm().child(format!("@{username}"))))
            .child(div().text_xs().text_color(theme.muted_foreground).child(format!("{} · {}", connection.state.as_str(), connection.health_label())))
            .child(div().text_sm().child("Any bot can use this connection when you ask. Ask the bot in chat to create a trigger to watch messages."))
            .into_any_element()
    }

    fn render_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = &self.form else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let provider = form.provider;
        let replacing = form.replacement.is_some();
        let token = match &form.entry {
            Entry::Token(token) => token,
            Entry::Pairing(card) => return self.render_pairing(provider, replacing, card, cx),
        };
        let mut card = v_flex().w_full().gap_3().p_4().rounded(theme.radius_lg).bg(theme.secondary)
            .child(h_flex().justify_between().items_center()
                .child(div().font_semibold().child(format!("{} · {}", provider.label(), if replacing { "Replace token" } else { "Connect" })))
                .child(Button::new("social-cancel").control(Tier::Ghost, Scale::Compact, self.busy || !self.available, cx).label("Cancel")
                    .on_click(cx.listener(|view, _, window, cx| view.cancel(window, cx)))))
            .child(div().text_sm().child(provider.token_help()))
            .child(div().text_xs().text_color(theme.muted_foreground).child("The API stores the token encrypted and uses it to call the platform. The assistant never sees the token, and it stays out of chat history."))
            .children(provider.portal().map(|(label, url)| {
                Button::new("social-token-help").control(Tier::Ghost, Scale::Standard, false, cx).label(label)
                    .on_click(move |_, _, cx| cx.open_url(url))
            }))
            .child(text_field(Input::new(token), cx).aria_label(format!("{} bot token", provider.label())).disabled(self.busy || !self.available))
            .child(h_flex().justify_end().child(Button::new("social-connect").control(Tier::Primary, Scale::Standard, self.busy || !self.available, cx).label(if replacing { "Validate replacement token" } else { "Validate and connect" })
                .on_click(cx.listener(|view, _, window, cx| view.connect(window, cx)))));
        if self.setup.is_some() && !replacing {
            let existing: Vec<_> = self
                .connections
                .iter()
                .filter(|c| c.provider == provider)
                .cloned()
                .collect();
            if !existing.is_empty() {
                card = card.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Or use a bot you already connected"),
                );
                for connection in existing {
                    let id = connection.id;
                    card = card.child(
                        Button::new(SharedString::from(format!("social-existing-{id}")))
                            .control(
                                Tier::Secondary,
                                Scale::Standard,
                                self.busy || !self.available,
                                cx,
                            )
                            .label(connection.display_name.clone())
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.complete(connection.clone(), None, window, cx)
                            })),
                    );
                }
            }
        }
        card.into_any_element()
    }

    fn render_pairing(
        &self,
        provider: Provider,
        replacing: bool,
        card: &PairingCard,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let blocked = self.busy || !self.available;
        let pairing = card.view.as_ref().map(|v| &v.pairing);
        let status: AnyElement = match pairing {
            None | Some(Pairing::Starting) => div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Preparing a code…")
                .into_any_element(),
            Some(Pairing::Code { .. }) => v_flex()
                .gap_2()
                .items_center()
                .children(card.qr.as_ref().map(|(_, image)| {
                    img(image.clone())
                        .size(px(240.))
                        .rounded(theme.radius)
                        .bg(gpui_kit::white())
                }))
                .child(div().text_sm().child(
                    "On your phone, open WhatsApp → Settings → Linked devices → Link a device, and scan this code. It refreshes on its own.",
                ))
                .into_any_element(),
            Some(Pairing::Paired { connection }) => div()
                .text_sm()
                .child(format!(
                    "Linked as {}{}.",
                    connection.display_name,
                    connection
                        .username
                        .as_ref()
                        .map(|number| format!(" ({number})"))
                        .unwrap_or_default()
                ))
                .into_any_element(),
            Some(finished) => v_flex()
                .gap_2()
                .child(div().text_sm().child(match finished {
                    Pairing::Expired => "No code was scanned in time.".to_owned(),
                    Pairing::Failed { message } => message.clone(),
                    _ => "Pairing stopped.".to_owned(),
                }))
                .child(
                    Button::new("social-pairing-restart")
                        .control(Tier::Secondary, Scale::Standard, blocked, cx)
                        .label("Show a new code")
                        .on_click(cx.listener(|view, _, window, cx| view.restart_pairing(window, cx))),
                )
                .into_any_element(),
        };
        let paired = matches!(pairing, Some(Pairing::Paired { .. }));
        v_flex()
            .w_full()
            .gap_3()
            .p_4()
            .rounded(theme.radius_lg)
            .bg(theme.secondary)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(div().font_semibold().child(format!(
                        "Your personal {} · {}",
                        provider.label(),
                        if replacing { "Pair again" } else { "Link" }
                    )))
                    .when(!paired, |row| {
                        row.child(
                            Button::new("social-cancel")
                                .control(Tier::Ghost, Scale::Compact, blocked, cx)
                                .label("Cancel")
                                .on_click(
                                    cx.listener(|view, _, window, cx| view.cancel(window, cx)),
                                ),
                        )
                    }),
            )
            .child(div().text_sm().child(provider.token_help()))
            .child(
                div().text_xs().text_color(theme.muted_foreground).child(
                    "The API keeps the device keys encrypted. The assistant never sees them.",
                ),
            )
            .child(status)
            .when(self.setup.is_some() && !paired, |mut c| {
                for connection in self.connections.iter().filter(|c| c.provider == provider) {
                    let chosen = connection.clone();
                    c = c.child(
                        Button::new(SharedString::from(format!(
                            "social-existing-{}",
                            connection.id
                        )))
                        .control(Tier::Secondary, Scale::Standard, blocked, cx)
                        .label(format!("Use {} already linked", connection.display_name))
                        .on_click(cx.listener(
                            move |view, _, window, cx| {
                                let note = view
                                    .form
                                    .as_mut()
                                    .and_then(Form::pairing_card)
                                    .and_then(|card| card.note(cx));
                                view.complete(chosen.clone(), note, window, cx)
                            },
                        )),
                    );
                }
                c
            })
            .when(self.setup.is_some(), |c| {
                c.child(
                    Textarea::new(&card.note)
                        .aria_label("What the bot should do once connected")
                        .disabled(blocked),
                )
                .child(
                    h_flex().justify_end().child(
                        Button::new("social-pairing-done")
                            .control(Tier::Primary, Scale::Standard, blocked || !paired, cx)
                            .label("Done")
                            .on_click(
                                cx.listener(|view, _, window, cx| view.finish_pairing(window, cx)),
                            ),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_connection(&self, connection: &Connection, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let id = connection.id;
        let paused = connection.state != ConnectionState::Active;
        let active_connection = connection.clone();
        let replacement = connection.clone();
        // A grouped-list row: the account, its state, and its actions at
        // the trailing edge. A conflict or a pending disconnect follows it.
        let detail = connection
            .username
            .as_ref()
            .map(|username| format!("@{username} · "))
            .unwrap_or_default()
            + &format!(
                "{} · {}",
                connection.state.as_str(),
                connection.health_label()
            );
        let card = v_flex().w_full().child(
            crate::panel_kit::list_row()
                .child(crate::panel_kit::row_icon(
                    provider_icon(connection.provider),
                    cx,
                ))
                .child(crate::panel_kit::row_text(
                    format!(
                        "{} · {}",
                        connection.provider.label(),
                        connection.display_name
                    ),
                    detail,
                    cx,
                ))
                .child(
                    h_flex()
                        .flex_none()
                        .gap_1()
                        .child(
                            Button::new(SharedString::from(format!("social-pause-{id}")))
                                .control(
                                    Tier::Secondary,
                                    Scale::Compact,
                                    self.busy || !self.available,
                                    cx,
                                )
                                .label(if paused { "Resume" } else { "Pause" })
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.active(active_connection.clone(), window, cx)
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("social-replace-{id}")))
                                .control(
                                    Tier::Secondary,
                                    Scale::Compact,
                                    self.busy || !self.available,
                                    cx,
                                )
                                .label(if replacement.provider.pairs() {
                                    "Pair again"
                                } else {
                                    "Replace token"
                                })
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.replace(replacement.clone(), window, cx)
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("social-disconnect-{id}")))
                                .control(
                                    Tier::Secondary,
                                    Scale::Compact,
                                    self.busy || !self.available,
                                    cx,
                                )
                                .label("Disconnect")
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.confirm_disconnect = Some(id);
                                    cx.notify();
                                })),
                        ),
                ),
        );
        let mut notes = v_flex().px_4().pb_3().gap_3();
        let mut notes_shown = false;
        if connection.receiver_conflict() {
            notes_shown = true;
            notes = notes.child(div().text_sm().text_color(theme.danger).child(
                "Another webhook, app, or server is receiving this bot's messages. Telegram delivers them to only one receiver. Remove the bot's webhook or stop the other app, then refresh.",
            ));
        }
        if connection.health == Health::SignedOut {
            notes_shown = true;
            notes = notes.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child("WhatsApp signed this device out. Pair again to keep using it."),
            );
        }
        if self.confirm_disconnect == Some(id) {
            notes_shown = true;
            notes = notes
                .child(
                    div()
                        .text_sm()
                        .child("Disconnect this account? All bots will lose this connection."),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new(SharedString::from(format!("social-keep-{id}")))
                                .control(
                                    Tier::Secondary,
                                    Scale::Compact,
                                    self.busy || !self.available,
                                    cx,
                                )
                                .label("Keep connected")
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.confirm_disconnect = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!(
                                "social-confirm-disconnect-{id}"
                            )))
                            .control(
                                Tier::Danger,
                                Scale::Standard,
                                self.busy || !self.available,
                                cx,
                            )
                            .label("Disconnect account")
                            .on_click(cx.listener(
                                move |view, _, window, cx| view.disconnect(id, window, cx),
                            )),
                        ),
                );
        }
        card.when(notes_shown, |card| card.child(notes))
            .into_any_element()
    }
}

fn provider_icon(provider: Provider) -> gpui_kit::component::Icon {
    match provider {
        Provider::Telegram => gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Send),
        Provider::Discord => gpui_kit::component::Icon::empty().path(crate::DISCORD_ICON),
        Provider::WhatsApp => {
            gpui_kit::component::Icon::new(gpui_kit::assets::IconName::MessageSquare)
        }
    }
}

impl Render for SocialsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut root = v_flex().w_full().gap_3();
        if self.setup.is_none() {
            root = root.child(crate::panel_kit::page_heading(
                "Socials",
                "Connect Telegram and Discord bots, or your personal WhatsApp, for any of your bots to use.",
                vec![
                    Button::new("social-refresh")
                        .control(
                            Tier::Secondary,
                            Scale::Compact,
                            self.busy || !self.available,
                            cx,
                        )
                        .icon(gpui_kit::assets::IconName::RefreshCw)
                        .label("Refresh")
                        .on_click(cx.listener(|view, _, window, cx| view.refresh(window, cx)))
                        .into_any_element(),
                ],
                cx,
            ));
            if !self.connections.is_empty() {
                let rows: Vec<_> = self
                    .connections
                    .iter()
                    .map(|connection| self.render_connection(connection, cx))
                    .collect();
                root = root.child(div().mt_3().child(crate::panel_kit::grouped_list(rows, cx)));
            }
            let mut providers = Vec::new();
            for (provider, id, description) in [
                (
                    Provider::Telegram,
                    "social-add-telegram",
                    "Send messages and watch Telegram chats.",
                ),
                (
                    Provider::Discord,
                    "social-add-discord",
                    "Send messages and follow Discord channels.",
                ),
            ] {
                providers.push(
                    crate::panel_kit::list_row()
                        .child(crate::panel_kit::row_icon(provider_icon(provider), cx))
                        .child(crate::panel_kit::row_text(
                            provider.label(),
                            description,
                            cx,
                        ))
                        .child(
                            Button::new(id)
                                .control(
                                    Tier::Secondary,
                                    Scale::Compact,
                                    self.busy || !self.available,
                                    cx,
                                )
                                .label(format!("Add {}", provider.label()))
                                // Paired rows share one button width.
                                .w(px(120.))
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.add(provider, window, cx)
                                })),
                        )
                        .into_any_element(),
                );
            }
            providers.push(
                crate::panel_kit::list_row()
                    .child(crate::panel_kit::row_icon(provider_icon(Provider::WhatsApp), cx))
                    .child(crate::panel_kit::row_text(
                        Provider::WhatsApp.label(),
                        "Ask a bot to connect WhatsApp. It shows a QR code in chat to link your personal account.",
                        cx,
                    ))
                    .into_any_element(),
            );
            root = root.child(
                div()
                    .mt_3()
                    .child(crate::panel_kit::grouped_list(providers, cx)),
            );
        }
        if self.busy {
            root = root.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Updating connection…"),
            );
        }
        if !self.available {
            root = root.child(div().text_sm().text_color(theme.muted_foreground).child("Reconnect to the API to manage connections. The private token field has been cleared."));
        }
        if let Some(error) = &self.error {
            root = root.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        if let Some(notice) = &self.notice {
            root = root.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(notice.clone()),
            );
        }
        if let Some(receipt) = &self.receipt {
            root.child(self.render_receipt(receipt, cx))
        } else {
            root.child(self.render_form(cx))
        }
    }
}
