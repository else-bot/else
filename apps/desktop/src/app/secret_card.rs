//! The private form for a bot's secret request, shown as a card in the
//! conversation. Values live only in the card's masked inputs (or, for a
//! multi-line paste such as a private key, beside them) until they are sent
//! to the owner API; they are cleared on success, decline, navigation, and
//! disconnect, and never enter a draft, a pending message, or the chat.

use crate::app::transcript::CardKey;
use crate::controls::{Control as _, Scale, Tier, text_field};
use crate::credential_fields::{CredentialFields, FieldsChanged, Form};
use gpui_kit::assets::IconName;

use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use super::request::Request;
use super::{Connection, ElseApp, RowKey};
use crate::api::ClientError;
use crate::model::{SecretRequest, SecretRequestStatus};

pub(super) struct SecretForm {
    fields: Entity<CredentialFields>,
    description: Entity<InputState>,
    website: Entity<InputState>,
    _fields: Subscription,
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
            app.remeasure_row(RowKey::Card(CardKey::SecretRequest(id)), cx);
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
    pub(super) fn sync_secret_cards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        view.cards.secrets.retain(|id, card| {
            let keep = pending.iter().any(|r| r.id == *id);
            if !keep && let Some(form) = card.form.as_mut() {
                form.clear(window, cx);
            }
            keep
        });
        let connected = self.connection == Connection::Connected;
        for request in pending {
            let card = view.cards.secrets.entry(request.id).or_default();
            if !matches!(card.request, Request::Accepted(())) && card.form.is_none() {
                card.form = Some(SecretForm::new(&request, window, cx));
            }
        }
        for card in view.cards.secrets.values() {
            let Some(form) = card.form.as_ref() else {
                continue;
            };
            let busy = card.request.busy() || !connected;
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
            // An answered request stays locked until it settles.
            for card in view.cards.secrets.values_mut() {
                if let Some(mut form) = card.form.take() {
                    form.clear(window, cx);
                }
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
        let Some(card) = view.cards.secrets.get_mut(&request) else {
            return;
        };
        let Some(form) = card.form.as_ref().filter(|_| !card.request.busy()) else {
            return;
        };
        let env = if store {
            match form.values(cx) {
                Ok(env) => Some(env),
                Err(message) => {
                    card.request.reject(message);
                    self.remeasure_row(RowKey::Card(CardKey::SecretRequest(request)), cx);
                    return;
                }
            }
        } else {
            None
        };
        form.fields.update(cx, |fields, cx| {
            fields.busy = true;
            cx.notify();
        });
        let Some(attempt) = card.request.begin(()) else {
            return;
        };
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = client.answer_secret(bot, request, env).await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.secret_answered(bot, request, attempt, result, window, cx)
            });
        });
        task.detach();
        self.remeasure_row(RowKey::Card(CardKey::SecretRequest(request)), cx);
    }

    fn secret_answered(
        &mut self,
        bot: Uuid,
        request: Uuid,
        attempt: Uuid,
        result: Result<(), ClientError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settled = self
            .view_of(bot)
            .and_then(|view| view.cards.secrets.get_mut(&request))
            .is_some_and(|card| card.request.settle(attempt, &result));
        let rejected = result
            .as_ref()
            .is_err_and(|err| self.unauthorized(err, window, cx));
        if rejected || !settled {
            return;
        }
        let Some(card) = self
            .view_of(bot)
            .and_then(|view| view.cards.secrets.get_mut(&request))
        else {
            return;
        };
        if result.is_ok() {
            if let Some(mut form) = card.form.take() {
                form.clear(window, cx);
            }
        } else if let Some(form) = &card.form {
            form.fields.update(cx, |fields, cx| {
                fields.busy = false;
                cx.notify();
            });
        }
        self.remeasure_row(RowKey::Card(CardKey::SecretRequest(request)), cx);
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
        let card_state = view.cards.secrets.get(&id);
        let form = card_state.and_then(|card| card.form.as_ref());
        let (Some(form), Some(card_state), true) = (form, card_state, request.is_pending()) else {
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
        let sending = card_state.request.busy();
        let busy = sending || self.connection != Connection::Connected;
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
        if let Some(error) = card_state.request.error() {
            card = card.child(super::action_card::failure(&theme, error.to_owned()));
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
                        app.remeasure_row(super::RowKey::Card(CardKey::SecretRequest(id)), cx);
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
                    .label(if sending {
                        "Saving…"
                    } else {
                        "Save and continue"
                    })
                    .on_click(cx.listener(move |app, _, window, cx| {
                        app.answer_secret(id, true, window, cx)
                    })),
            );
        let card = row.child(card.footer(footer)).into_any_element();
        if sending {
            card
        } else {
            self.sighted(RowKey::Card(CardKey::SecretRequest(id)), card, cx)
        }
    }
}
