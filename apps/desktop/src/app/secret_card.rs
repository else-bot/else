//! The private form for a bot's secret request, shown as a card in the
//! conversation. Values live only in the card's masked inputs (or, for a
//! multi-line paste such as a private key, beside them) until they are sent
//! to the owner API; they are cleared on success, decline, navigation, and
//! disconnect, and never enter a draft, a pending message, or the chat.

use crate::controls::{Control as _, Scale, Tier, text_field};
use crate::credential_fields::{CredentialFields, FieldsChanged, Form};
use gpui_kit::assets::IconName;

use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use super::{Connection, ElseApp, RowKey};
use crate::api::ClientError;
use crate::model::{SecretRequest, SecretRequestStatus};

pub(super) struct SecretForm {
    fields: Entity<CredentialFields>,
    description: Entity<InputState>,
    website: Entity<InputState>,
    _fields: Subscription,
    submitting: bool,
    error: Option<String>,
}

impl SecretForm {
    /// Whether the owner already sent this form and waits on the server.
    pub(super) fn locks(&self) -> bool {
        self.submitting
    }
}

impl SecretForm {
    pub(super) fn new(
        request: &SecretRequest,
        window: &mut Window,
        cx: &mut Context<ElseApp>,
    ) -> Self {
        let fields = cx.new(|cx| CredentialFields::new(&request.env, Form::Requested, window, cx));
        let id = request.id;
        let subscription = cx.subscribe(&fields, move |app, _, _: &FieldsChanged, cx| {
            app.remeasure_row(RowKey::SecretRequest(id), cx);
        });
        Self {
            fields,
            description: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(request.description.clone().unwrap_or_default())
                    .placeholder("Description, such as X, @username")
            }),
            website: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(request.website.clone().unwrap_or_default())
                    .placeholder("Website host, such as x.com")
            }),
            _fields: subscription,
            submitting: false,
            error: None,
        }
    }

    fn values(&self, cx: &App) -> Result<crate::api::SaveSecret, String> {
        let description = self.description.read(cx).value().to_string();
        let website =
            crate::api::validate_secret_metadata(&description, &self.website.read(cx).value())?;
        Ok(crate::api::SaveSecret {
            env: self.fields.read(cx).values(cx)?,
            description,
            website,
        })
    }

    pub(super) fn clear(&mut self, window: &mut Window, cx: &mut App) {
        self.fields
            .update(cx, |fields, cx| fields.clear(window, cx));
    }
}

impl ElseApp {
    pub(super) fn sync_secret_forms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_skill_cards(window, cx);
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let pending: Vec<SecretRequest> = view
            .conversation
            .iter()
            .flat_map(|c| c.secret_requests())
            .filter(|(_, r)| r.is_pending())
            .map(|(_, r)| r.clone())
            .collect();
        view.secret_forms.retain(|id, form| {
            let keep = pending.iter().any(|r| r.id == *id);
            if !keep {
                form.clear(window, cx);
            }
            keep
        });
        view.answered
            .retain(|id| pending.iter().any(|r| r.id == *id));
        let connected = self.connection == Connection::Connected;
        for request in pending {
            if !view.answered.contains(&request.id) {
                view.secret_forms
                    .entry(request.id)
                    .or_insert_with(|| SecretForm::new(&request, window, cx));
            }
        }
        for form in view.secret_forms.values() {
            let busy = form.submitting || !connected;
            form.fields.update(cx, |fields, cx| {
                if fields.busy != busy {
                    fields.busy = busy;
                    cx.notify();
                }
            });
        }
    }

    pub(super) fn clear_secret_forms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_skill_cards(window, cx);
        if let Some(view) = self.view.as_mut() {
            for (_, mut form) in view.secret_forms.drain() {
                form.clear(window, cx);
            }
        }
    }

    fn answer_secret(
        &mut self,
        request: Uuid,
        store: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let bot = view.bot_id;
        let Some(form) = view.secret_forms.get_mut(&request) else {
            return;
        };
        if form.submitting {
            return;
        }
        let env = if store {
            match form.values(cx) {
                Ok(env) => Some(env),
                Err(message) => {
                    form.error = Some(message);
                    self.remeasure_row(RowKey::SecretRequest(request), cx);
                    return;
                }
            }
        } else {
            None
        };
        form.submitting = true;
        form.error = None;
        form.fields.update(cx, |fields, cx| {
            fields.busy = true;
            cx.notify();
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = client.answer_secret(bot, request, env).await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.secret_answered(bot, request, result, window, cx)
            });
        });
        task.detach();
        self.remeasure_row(RowKey::SecretRequest(request), cx);
    }

    fn secret_answered(
        &mut self,
        bot: Uuid,
        request: Uuid,
        result: Result<(), ClientError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(&result, Err(ClientError::Unauthorized)) {
            self.connection = Connection::InvalidCredentials(ClientError::Unauthorized.to_string());
            self.clear_secret_forms(window, cx);
            self.sync_secret_forms(window, cx);
            cx.notify();
            return;
        }
        let Some(view) = self.view_of(bot) else {
            return;
        };
        match result {
            Ok(()) => {
                if let Some(mut form) = view.secret_forms.remove(&request) {
                    form.clear(window, cx);
                }
                view.answered.insert(request);
            }
            Err(err) => {
                if let Some(form) = view.secret_forms.get_mut(&request) {
                    form.submitting = false;
                    form.fields.update(cx, |fields, cx| {
                        fields.busy = false;
                        cx.notify();
                    });
                    form.error = Some(err.to_string());
                }
            }
        }
        self.remeasure_row(RowKey::SecretRequest(request), cx);
    }

    pub(super) fn render_secret_card(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = self.view.as_ref() else {
            return div().into_any_element();
        };
        let Some(request) = view
            .conversation
            .as_ref()
            .and_then(|c| c.secret_request(id))
        else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let bot_name = self
            .bots
            .iter()
            .find(|b| b.id == view.bot_id)
            .map(|b| b.name.clone())
            .unwrap_or_default();
        let note = |text: String| {
            div()
                .text_xs()
                .whitespace_normal()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let row = v_flex().w_full();
        let form = view.secret_forms.get(&id);
        let (Some(form), true) = (form, request.is_pending()) else {
            let text = match request.status {
                SecretRequestStatus::Stored => format!(
                    "Saved {} in the Vault. Every bot on this server can use it.",
                    request.name
                ),
                SecretRequestStatus::Cancelled => format!(
                    "The request for {} was declined or withdrawn.",
                    request.name
                ),
                SecretRequestStatus::Pending | SecretRequestStatus::Unknown => {
                    format!("Sent {} to {bot_name}…", request.name)
                }
            };
            return row.child(note(text)).into_any_element();
        };
        let busy = form.submitting || self.connection != Connection::Connected;
        let fields = v_flex()
            .w_full()
            .gap_2()
            .child(
                text_field(
                    Input::new(&form.description).aria_label("Vault description"),
                    cx,
                )
                .disabled(busy),
            )
            .child(
                text_field(Input::new(&form.website).aria_label("Website host"), cx).disabled(busy),
            )
            .child(form.fields.clone());
        let expanded = view.expanded_events.contains(&id);
        let mut card = super::action_card::card(
            IconName::Lock,
            format!(
                "Save {}",
                request
                    .description
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&request.name)
            ),
            "",
        )
        .child(
            div()
                .text_sm()
                .whitespace_normal()
                .child(request.reason.clone()),
        )
        .child(fields)
        .when(expanded, |card| {
            card.child(note(format!(
                "Saved in the Vault as {} for every bot on this server, replacing one with the \
                 same name. {bot_name} never sees the values. Paste multi-line values such as \
                 private keys whole.",
                request.name
            )))
        });
        if let Some(error) = &form.error {
            card = card.child(super::action_card::failure(&theme, error.clone()));
        }
        let footer = super::action_card::footer()
            .note("Encrypted on this server. Values stay out of chat.")
            .note_link(
                Button::new(SharedString::from(format!("secret-about-{id}")))
                    .label(if expanded {
                        "Hide details"
                    } else {
                        "About saving"
                    })
                    .accessibility_label(if expanded {
                        "Hide details about saving credentials"
                    } else {
                        "About saving credentials"
                    })
                    .on_click(cx.listener(move |app, _, _, cx| {
                        if let Some(view) = app.view.as_mut()
                            && !view.expanded_events.remove(&id)
                        {
                            view.expanded_events.insert(id);
                        }
                        app.remeasure_row(super::RowKey::SecretRequest(id), cx);
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("secret-decline-{id}")))
                    .control(Tier::Secondary, Scale::Compact, busy, cx)
                    .label("Decline")
                    .on_click(cx.listener(move |app, _, window, cx| {
                        app.answer_secret(id, false, window, cx)
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("secret-save-{id}")))
                    .control(Tier::Primary, Scale::Compact, busy, cx)
                    .label(if form.submitting {
                        "Saving…"
                    } else {
                        "Save and continue"
                    })
                    .on_click(cx.listener(move |app, _, window, cx| {
                        app.answer_secret(id, true, window, cx)
                    })),
            );
        let card = row.child(card.footer(footer)).into_any_element();
        if form.locks() {
            card
        } else {
            self.sighted(RowKey::SecretRequest(id), card, cx)
        }
    }
}
