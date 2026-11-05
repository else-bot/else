//! Onboarding and bot deletion dialogs.

use super::delete_dialog::DeleteTarget;
use super::{Connection, ElseApp};
use crate::api::{Bot, Capabilities};
use crate::profile_picker::ProfilePicker;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, h_flex, v_flex};
use gpui_kit::*;
use uuid::Uuid;

/// The welcome screen, shown until the main bot is named and its provider
/// and model chosen.
pub(super) struct Welcome {
    name: Entity<InputState>,
    picker: Entity<ProfilePicker>,
    busy: bool,
    error: Option<String>,
}

impl ElseApp {
    pub(super) fn open_welcome(
        &mut self,
        caps: &Capabilities,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .default_bot_id
            .and_then(|id| self.bots.iter().find(|b| b.id == id))
            .map(|b| b.name.clone())
            .unwrap_or_default();
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Your assistant's name, e.g. Jarvis")
                .default_value(current)
        });
        let picker = cx.new(|cx| ProfilePicker::new(caps, caps.defaults.as_ref(), window, cx));
        self._welcome_enter =
            Some(
                cx.subscribe_in(&name, window, |app, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        app.submit_welcome(window, cx);
                    }
                }),
            );
        name.read(cx).focus_handle(cx).focus(window, cx);
        self.welcome = Some(Welcome {
            name,
            picker,
            busy: false,
            error: None,
        });
    }

    fn submit_welcome(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (client, Some(welcome)) = (self.client.clone(), self.welcome.as_mut()) else {
            return;
        };
        if welcome.busy {
            return;
        }
        let name = welcome.name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            welcome.error = Some("Give your assistant a name.".into());
            cx.notify();
            return;
        }
        let Some(profile) = welcome.picker.read(cx).choice() else {
            welcome.error = Some("Choose a model.".into());
            cx.notify();
            return;
        };
        welcome.busy = true;
        welcome.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.clone().onboard(name, profile).await;
            let caps = match result {
                Ok(_) => Some(client.clone().capabilities().await),
                Err(_) => None,
            };
            let _ = this.update_in(cx, |app, window, cx| {
                match result {
                    Ok(main) => {
                        app.welcome = None;
                        app._welcome_enter = None;
                        if let Some(caps) = caps {
                            app.apply_caps(caps, window, cx);
                        }
                        app.refresh_now(window, cx);
                        app.select(Some(main), window, cx);
                        app.composer.read(cx).focus_handle(cx).focus(window, cx);
                    }
                    Err(err) => {
                        if let Some(welcome) = app.welcome.as_mut() {
                            welcome.busy = false;
                            welcome.error = Some(err.to_string());
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_welcome(&self, welcome: &Welcome, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let ready = welcome.picker.read(cx).any_available();
        let mut card = v_flex()
            .w(px(560.))
            .gap_4()
            .p_6()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(div().text_xl().child("Welcome to else.bot"))
            .child(div().text_sm().text_color(theme.muted_foreground).child(
                "Your main assistant is your entry point: it knows you, remembers \
                         what matters, and hands work to other bots. Name it and choose how \
                         it thinks. New bots start with the same choice, and each bot can \
                         have its own.",
            ))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Name"),
                    )
                    .child(Input::new(&welcome.name).focus_ring(false)),
            )
            .child(welcome.picker.clone());
        if let Some(error) = &welcome.error {
            card = card.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        card = card.child(
            h_flex().justify_end().child(
                Button::new("welcome-start")
                    .focus_ring(false)
                    .cursor_pointer()
                    .label("Get started")
                    .primary()
                    .loading(welcome.busy)
                    .disabled(welcome.busy || !ready)
                    .on_click(cx.listener(|app, _, window, cx| app.submit_welcome(window, cx))),
            ),
        );
        h_flex()
            .size_full()
            .items_center()
            .justify_center()
            .bg(theme.muted)
            .child(card)
            .into_any_element()
    }

    fn deletable_bot(&mut self, id: Uuid, cx: &mut Context<Self>) -> Option<Bot> {
        if self.connection != Connection::Connected || self.deleting.contains(&id) {
            return None;
        }
        let bot = self.active_bots().find(|b| b.id == id)?.clone();
        if self.default_bot_id == Some(id) {
            self.notice = Some(format!(
                "{} is your main bot and cannot be deleted.",
                bot.name
            ));
            cx.notify();
            return None;
        }
        Some(bot)
    }

    pub(super) fn open_delete_dialog(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bot) = self.deletable_bot(id, cx) else {
            return;
        };
        self.open_delete_confirmation(
            DeleteTarget::Bot(bot.name),
            window,
            cx,
            move |app, window, cx| {
                app.delete_bot(id, window, cx);
            },
        );
    }

    fn delete_bot(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if self.deletable_bot(id, cx).is_none() {
            return;
        }
        self.deleting.insert(id);
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.delete_bot(id).await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.deleting.remove(&id);
                match result {
                    Ok(receipt) => {
                        if let Some(bot) = app.bots.iter_mut().find(|b| b.id == id) {
                            *bot = receipt;
                        }
                        app.reconcile_selection(window, cx);
                    }
                    Err(err) => app.notice = Some(format!("Delete failed: {err}")),
                }
                app.refresh_now(window, cx);
            });
        })
        .detach();
    }

    // ----- Rendering --------------------------------------------------------
}
