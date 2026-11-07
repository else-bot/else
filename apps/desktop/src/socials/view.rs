use super::*;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Selectable as _, Sizable as _,
    StyledExt as _, h_flex, v_flex,
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
        let Receipt::Ready {
            connection,
            access,
            destinations,
        } = receipt
        else {
            return card.child(div().text_sm().child(match receipt {
                Receipt::Loading { .. } => "Loading current connection access…",
                _ => "This connection is no longer available. Manage connections in Settings → Socials.",
            })).into_any_element();
        };
        let rights = if connection.state != "active" {
            "Paused; no tools may use this connection"
        } else if !access.permissions.read {
            "No access"
        } else if access.permissions.send {
            "Read, message actions, and approved sends"
        } else if access.permissions.write {
            "Read and message actions"
        } else {
            "Read only"
        };
        card.child(
            div()
                .font_semibold()
                .child(format!("{} · Current access", connection.provider.label())),
        )
        .child(div().text_sm().child(connection.display_name.clone()))
        .child(
            div()
                .text_sm()
                .child(format!("Conversations: {destinations}")),
        )
        .child(div().text_sm().child(format!("Access: {rights}")))
        .child(div().text_sm().child(access.instructions.clone()))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Manage access and instructions any time in Settings → Socials."),
        )
        .into_any_element()
    }

    fn render_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = &self.form else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let provider = form.provider;
        let mut card = v_flex()
            .w_full()
            .gap_3()
            .p_4()
            .rounded(theme.radius_lg)
            .bg(theme.secondary);
        card = card.child(
            h_flex()
                .justify_between()
                .items_center()
                .child(div().font_semibold().child(format!(
                    "{} · {}",
                    provider.label(),
                    if matches!(form.editor, Editor::Credential { .. }) {
                        "Connect"
                    } else {
                        "Access"
                    }
                )))
                .child(
                    Button::new("social-cancel")
                        .focus_ring(false)
                        .cursor_pointer()
                        .ghost()
                        .xsmall()
                        .label("Cancel")
                        .disabled(self.busy || !self.available)
                        .on_click(cx.listener(|view, _, window, cx| view.cancel(window, cx))),
                ),
        );
        if let Editor::Credential { replacement } = &form.editor {
            let replacing = replacement.is_some();
            card = card.child(div().text_sm().child(provider.token_help()))
                .child(div().text_xs().text_color(theme.muted_foreground).child("The API stores the token encrypted and uses it to call the platform. The assistant never sees the token, and it stays out of chat history."))
                .child(Button::new("social-token-help").focus_ring(false).cursor_pointer().ghost().xsmall().label(if provider == Provider::Telegram { "Open BotFather" } else { "Open Discord Developer Portal" }).on_click(move |_, _, cx| {
                    cx.open_url(match provider { Provider::Telegram => "https://t.me/BotFather", Provider::Discord => "https://discord.com/developers/applications" });
                }))
                .child(Input::new(&form.token).focus_ring(false).aria_label(format!("{} bot token", provider.label())).disabled(self.busy || !self.available))
                .child(h_flex().justify_end().child(Button::new("social-connect").focus_ring(false).cursor_pointer().primary().small().label(if replacing { "Validate replacement token" } else { "Validate bot token" }).disabled(self.busy || !self.available).on_click(cx.listener(|view, _, window, cx| view.connect(window, cx)))));
            if !replacing {
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
                                .ghost()
                                .small()
                                .label(connection.display_name.clone())
                                .disabled(self.busy || !self.available)
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.open(connection.clone(), window, cx)
                                })),
                        );
                    }
                }
            }
            return card.into_any_element();
        }
        let Some(access) = form.editor.access() else {
            return match &form.editor {
                Editor::Failed(connection) => {
                    let connection = connection.clone();
                    card.child(
                        Button::new("social-retry-access")
                            .focus_ring(false)
                            .cursor_pointer()
                            .ghost()
                            .small()
                            .label("Retry loading access")
                            .disabled(self.busy || !self.available)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.open(connection.clone(), window, cx)
                            })),
                    )
                    .into_any_element()
                }
                _ => card
                    .child(div().text_sm().child("Loading current connection access…"))
                    .into_any_element(),
            };
        };
        let connection = &access.connection;
        card = card
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child(connection.display_name.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("{} · {}", connection.state, connection.health)),
            );
        let mut bots = h_flex().gap_1().flex_wrap();
        for bot in &self.bots {
            if self.setup.is_some_and(|(_, assigned)| bot.id != assigned) {
                continue;
            }
            let id = bot.id;
            bots = bots.child(
                Button::new(SharedString::from(format!("social-bot-{id}")))
                    .focus_ring(false)
                    .cursor_pointer()
                    .small()
                    .ghost()
                    .selected(access.bot == Some(id))
                    .toggled(access.bot == Some(id))
                    .label(bot.name.clone())
                    .disabled(self.busy || !self.available)
                    .on_click(cx.listener(move |view, _, window, cx| {
                        if let Some(form) = &mut view.form {
                            form.choose_bot(id, window, cx);
                        }
                        cx.notify();
                    })),
            );
        }
        card = card
            .child(div().text_xs().font_semibold().child("Assistant"))
            .child(bots)
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .child("Choose conversations"),
            );
        for destination in &access.destinations {
            let id = destination.id.clone();
            let selected = access.draft.destinations.contains(&id);
            let name = format!(
                "{} {} · {}",
                if selected { "✓" } else { "+" },
                destination.name,
                if destination.can_send {
                    "Read and send available"
                } else {
                    "Read only"
                }
            );
            card = card.child(
                Button::new(SharedString::from(format!("social-destination-{id}")))
                    .focus_ring(false)
                    .cursor_pointer()
                    .ghost()
                    .small()
                    .w_full()
                    .selected(selected)
                    .toggled(selected)
                    .label(name)
                    .disabled(self.busy || !self.available)
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if let Some(access) =
                            view.form.as_mut().and_then(|form| form.editor.access_mut())
                        {
                            if access.draft.destinations.contains(&id) {
                                access.draft.destinations.retain(|selected| selected != &id);
                            } else {
                                access.draft.destinations.push(id.clone());
                            }
                        }
                        cx.notify();
                    })),
            );
        }
        if access.destinations.is_empty() {
            card = card.child(div().text_sm().text_color(theme.muted_foreground).child(match provider {
                Provider::Telegram => "No chats received yet. Add this bot to your group, or send it a private message, then refresh. A Telegram bot cannot import your old personal chat history.",
                Provider::Discord => "No channels are available. Add the bot to your server and allow View Channels, then refresh. Reading ordinary server messages may also require Message Content Intent.",
            }));
        }
        card = card.child(
            Button::new("social-refresh-destinations")
                .focus_ring(false)
                .cursor_pointer()
                .ghost()
                .xsmall()
                .label("Refresh conversations")
                .disabled(self.busy || !self.available)
                .on_click(cx.listener(|view, _, window, cx| view.refresh_destinations(window, cx))),
        );
        card = card.child(
            div()
                .text_xs()
                .font_semibold()
                .child("What may this assistant do?"),
        );
        for (key, label, enabled) in [
            (
                "read",
                "Read selected conversations",
                access.draft.permissions.read,
            ),
            (
                "write",
                "Edit, delete, react, pin, and create threads",
                access.draft.permissions.write,
            ),
            (
                "send",
                "Send drafts after your approval",
                access.draft.permissions.send,
            ),
        ] {
            card = card.child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_3()
                    .child(div().text_sm().child(label))
                    .child(
                        Button::new(SharedString::from(format!("social-permission-{key}")))
                            .focus_ring(false)
                            .cursor_pointer()
                            .ghost()
                            .xsmall()
                            .selected(enabled)
                            .toggled(enabled)
                            .label(if enabled { "Allowed" } else { "Off" })
                            .disabled(self.busy || !self.available)
                            .on_click(
                                cx.listener(move |view, _, _, cx| view.toggle_permission(key, cx)),
                            ),
                    ),
            );
        }
        let mut wakes = h_flex().gap_1().flex_wrap();
        for (policy, label) in [
            (WakePolicy::Never, "Never"),
            (WakePolicy::Mentions, "On mention"),
            (WakePolicy::All, "Every message"),
        ] {
            wakes = wakes.child(
                Button::new(SharedString::from(format!("social-wake-{policy:?}")))
                    .focus_ring(false)
                    .cursor_pointer()
                    .ghost()
                    .xsmall()
                    .selected(access.draft.wake_policy == policy)
                    .toggled(access.draft.wake_policy == policy)
                    .label(label)
                    .disabled(self.busy || !self.available || !access.draft.permissions.read)
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if let Some(access) =
                            view.form.as_mut().and_then(|form| form.editor.access_mut())
                        {
                            access.draft.wake_policy = policy;
                            access.draft.permissions.wake = policy != WakePolicy::Never;
                        }
                        cx.notify();
                    })),
            );
        }
        let bot_name = access
            .bot
            .and_then(|id| self.bots.iter().find(|b| b.id == id))
            .map(|b| b.name.as_str())
            .unwrap_or("the assistant");
        card = card.child(div().text_xs().font_semibold().child("Wake the assistant")).child(wakes)
            .child(div().text_xs().font_semibold().child(format!("What should {bot_name} do here?")))
            .child(Textarea::new(&form.instructions).bordered(false).aria_label(format!("What should {bot_name} do here?")).disabled(self.busy || !self.available))
            .child(div().text_xs().text_color(theme.muted_foreground).child("Describe what matters, when to reply, and what to ask you first. These instructions guide behavior. The permissions above still limit every tool call, and new conversations stay excluded."))
            .child(h_flex().justify_end().child(Button::new("social-save-access").focus_ring(false).cursor_pointer().primary().small().label(if self.setup.is_some() { "Connect to assistant" } else { "Save access" }).disabled(self.busy || !self.available || access.bot.is_none() || (self.setup.is_some() && !access.draft.permissions.read) || (access.draft.permissions.read && access.draft.destinations.is_empty())).on_click(cx.listener(|view, _, window, cx| view.save(window, cx)))));
        let paused = connection.state != "active";
        card = card.child(
            h_flex()
                .gap_1()
                .flex_wrap()
                .child(
                    Button::new("social-pause")
                        .focus_ring(false)
                        .cursor_pointer()
                        .ghost()
                        .xsmall()
                        .label(if paused {
                            "Resume connection"
                        } else {
                            "Pause connection"
                        })
                        .disabled(self.busy || !self.available)
                        .on_click(cx.listener(|view, _, window, cx| view.active(window, cx))),
                )
                .child(
                    Button::new("social-replace-token")
                        .focus_ring(false)
                        .cursor_pointer()
                        .ghost()
                        .xsmall()
                        .label("Replace token")
                        .disabled(self.busy || !self.available)
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.clear(window, cx);
                            if let Some(form) = &mut view.form
                                && let Some(connection) = form.editor.connection().cloned()
                            {
                                form.editor = Editor::Credential {
                                    replacement: Some(connection),
                                };
                            }
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("social-disconnect")
                        .focus_ring(false)
                        .cursor_pointer()
                        .ghost()
                        .xsmall()
                        .label("Disconnect")
                        .disabled(self.busy || !self.available)
                        .on_click(cx.listener(|view, _, _, cx| {
                            if let Some(form) = &mut view.form {
                                form.confirm_disconnect = true;
                            }
                            cx.notify();
                        })),
                ),
        );
        if form.confirm_disconnect {
            card = card
                .child(
                    div()
                        .text_sm()
                        .child("Disconnect this account? All assistants will lose access."),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("social-keep")
                                .focus_ring(false)
                                .cursor_pointer()
                                .ghost()
                                .small()
                                .label("Keep connected")
                                .on_click(cx.listener(|view, _, _, cx| {
                                    if let Some(form) = &mut view.form {
                                        form.confirm_disconnect = false;
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("social-confirm-disconnect")
                                .focus_ring(false)
                                .cursor_pointer()
                                .danger()
                                .small()
                                .label("Disconnect account")
                                .disabled(self.busy || !self.available)
                                .on_click(
                                    cx.listener(|view, _, window, cx| view.disconnect(window, cx)),
                                ),
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
            root = root.child(div().text_lg().font_semibold().child("Socials"))
                .child(div().text_sm().text_color(theme.muted_foreground).child("Connect a bot, choose its conversations, and tell your assistants how to help."));
            for connection in self.connections.clone() {
                let id = connection.id;
                root = root.child(
                    h_flex()
                        .w_full()
                        .gap_3()
                        .items_center()
                        .child(
                            v_flex()
                                .flex_1()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_semibold()
                                        .child(connection.display_name.clone()),
                                )
                                .child(div().text_xs().text_color(theme.muted_foreground).child(
                                    format!(
                                        "{} · {} · {}",
                                        connection.provider.label(),
                                        connection.state,
                                        connection.health
                                    ),
                                )),
                        )
                        .child(
                            Button::new(SharedString::from(format!("social-manage-{id}")))
                                .focus_ring(false)
                                .cursor_pointer()
                                .ghost()
                                .small()
                                .label("Manage")
                                .disabled(self.busy || !self.available)
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.open(connection.clone(), window, cx)
                                })),
                        ),
                );
            }
            root = root.child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("social-add-telegram")
                            .focus_ring(false)
                            .cursor_pointer()
                            .ghost()
                            .small()
                            .label("Add Telegram")
                            .disabled(self.busy || !self.available)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.add(Provider::Telegram, window, cx)
                            })),
                    )
                    .child(
                        Button::new("social-add-discord")
                            .focus_ring(false)
                            .cursor_pointer()
                            .ghost()
                            .small()
                            .label("Add Discord")
                            .disabled(self.busy || !self.available)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.add(Provider::Discord, window, cx)
                            })),
                    )
                    .child(
                        Button::new("social-refresh")
                            .focus_ring(false)
                            .cursor_pointer()
                            .ghost()
                            .xsmall()
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
