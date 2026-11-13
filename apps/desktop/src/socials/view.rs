use super::*;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};

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
            .child(div().text_xs().text_color(theme.muted_foreground).child(format!("{} · {}", connection.state, connection.health_label())))
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
        let mut card = v_flex().w_full().gap_3().p_4().rounded(theme.radius_lg).bg(theme.secondary)
            .child(h_flex().justify_between().items_center()
                .child(div().font_semibold().child(format!("{} · {}", provider.label(), if replacing { "Replace token" } else { "Connect" })))
                .child(Button::new("social-cancel").focus_ring(false).cursor_pointer().ghost().xsmall().label("Cancel")
                    .disabled(self.busy || !self.available)
                    .on_click(cx.listener(|view, _, window, cx| view.cancel(window, cx)))))
            .child(div().text_sm().child(provider.token_help()))
            .child(div().text_xs().text_color(theme.muted_foreground).child("The API stores the token encrypted and uses it to call the platform. The assistant never sees the token, and it stays out of chat history."))
            .child(Button::new("social-token-help").focus_ring(false).cursor_pointer().ghost()                .label(if provider == Provider::Telegram { "Open BotFather" } else { "Open Discord Developer Portal" })
                .on_click(move |_, _, cx| cx.open_url(match provider {
                    Provider::Telegram => "https://t.me/BotFather",
                    Provider::Discord => "https://discord.com/developers/applications",
                })))
            .child(Input::new(&form.token).focus_ring(false).aria_label(format!("{} bot token", provider.label())).disabled(self.busy || !self.available))
            .child(h_flex().justify_end().child(Button::new("social-connect").focus_ring(false).cursor_pointer().primary()                .label(if replacing { "Validate replacement token" } else { "Validate and connect" })
                .disabled(self.busy || !self.available)
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
                            .focus_ring(false)
                            .cursor_pointer()
                            .label(connection.display_name.clone())
                            .disabled(self.busy || !self.available)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.complete(connection.clone(), window, cx)
                            })),
                    );
                }
            }
        }
        card.into_any_element()
    }

    fn render_connection(&self, connection: &Connection, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let id = connection.id;
        let paused = connection.state != "active";
        let active_connection = connection.clone();
        let replacement = connection.clone();
        let mut card = v_flex()
            .w_full()
            .gap_3()
            .p_4()
            .rounded(theme.radius_lg)
            .bg(theme.secondary)
            .child(div().text_sm().font_semibold().child(format!(
                "{} · {}",
                connection.provider.label(),
                connection.display_name
            )))
            .children(
                connection
                    .username
                    .as_ref()
                    .map(|username| div().text_sm().child(format!("@{username}"))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "{} · {}",
                        connection.state,
                        connection.health_label()
                    )),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .child(
                        Button::new(SharedString::from(format!("social-pause-{id}")))
                            .focus_ring(false)
                            .cursor_pointer()
                            .label(if paused { "Resume" } else { "Pause" })
                            .disabled(self.busy || !self.available)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.active(active_connection.clone(), window, cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("social-replace-{id}")))
                            .focus_ring(false)
                            .cursor_pointer()
                            .label("Replace token")
                            .disabled(self.busy || !self.available)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.replace(replacement.clone(), window, cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("social-disconnect-{id}")))
                            .focus_ring(false)
                            .cursor_pointer()
                            .label("Disconnect")
                            .disabled(self.busy || !self.available)
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.confirm_disconnect = Some(id);
                                cx.notify();
                            })),
                    ),
            );
        if connection.receiver_conflict() {
            card = card.child(div().text_sm().text_color(theme.danger).child(
                "Another webhook, app, or server is receiving this bot's messages. Telegram delivers them to only one receiver. Remove the bot's webhook or stop the other app, then refresh.",
            ));
        }
        if self.confirm_disconnect == Some(id) {
            card = card
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
                                .focus_ring(false)
                                .cursor_pointer()
                                .label("Keep connected")
                                .disabled(self.busy || !self.available)
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.confirm_disconnect = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!(
                                "social-confirm-disconnect-{id}"
                            )))
                            .focus_ring(false)
                            .cursor_pointer()
                            .danger()
                            .label("Disconnect account")
                            .disabled(self.busy || !self.available)
                            .on_click(cx.listener(
                                move |view, _, window, cx| view.disconnect(id, window, cx),
                            )),
                        ),
                );
        }
        card.into_any_element()
    }
}

impl Render for SocialsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut root = v_flex().w_full().gap_3();
        if self.setup.is_none() {
            root =
                root.child(
                    v_flex()
                        .gap_2()
                        .child(div().text_xl().font_semibold().child("Socials"))
                        .child(div().text_sm().text_color(theme.muted_foreground).child(
                            "Connect Telegram and Discord bots for any of your bots to use.",
                        )),
                );
            for connection in &self.connections {
                root = root.child(self.render_connection(connection, cx));
            }
            let mut providers = v_flex().gap_3().mt_3();
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
                providers = providers.child(
                    h_flex()
                        .w_full()
                        .p_4()
                        .gap_3()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .child(
                            gpui_kit::component::Icon::new(match provider {
                                Provider::Telegram => gpui_kit::assets::IconName::MessageSquare,
                                Provider::Discord => gpui_kit::assets::IconName::MessageSquare,
                            })
                            .size_5()
                            .text_color(theme.muted_foreground),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .gap_1()
                                .child(div().text_sm().font_semibold().child(provider.label()))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(description),
                                ),
                        )
                        .child(
                            Button::new(id)
                                .focus_ring(false)
                                .cursor_pointer()
                                .label(format!("Add {}", provider.label()))
                                .disabled(self.busy || !self.available)
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.add(provider, window, cx)
                                })),
                        ),
                );
            }
            root = root.child(providers).child(
                h_flex().justify_end().child(
                    Button::new("social-refresh")
                        .focus_ring(false)
                        .cursor_pointer()
                        .label("Refresh")
                        .disabled(self.busy || !self.available)
                        .on_click(cx.listener(|view, _, window, cx| view.refresh(window, cx))),
                ),
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
