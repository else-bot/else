//! The private form for a bot's secret request, shown as a card in the
//! conversation. Values live only in the card's masked inputs (or, for a
//! multi-line paste such as a private key, beside them) until they are sent
//! to the owner API; they are cleared on success, decline, navigation, and
//! disconnect, and never enter a draft, a pending message, or the chat.

use gpui_kit::component::FocusableExt as _;
use std::collections::BTreeMap;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::*;
use uuid::Uuid;

use super::{Connection, ElseApp, RowKey};
use crate::api::ClientError;
use crate::model::SecretRequest;

pub(super) struct SecretForm {
    fields: Vec<SecretField>,
    submitting: bool,
    error: Option<String>,
}

struct SecretField {
    var: String,
    input: Entity<InputState>,
    /// A multi-line paste, held here because a single-line input cannot
    /// keep line breaks. Shown only as its size.
    captured: Option<String>,
}

/// Whether a paste needs its line breaks kept (a trailing line break alone
/// does not count: the input drops it, as for any single-line value).
pub(super) fn is_multiline(text: &str) -> bool {
    text.trim_end_matches(['\r', '\n']).contains(['\r', '\n'])
}

impl SecretForm {
    pub(super) fn new(request: &SecretRequest, window: &mut Window, cx: &mut App) -> Self {
        let fields = request
            .env
            .iter()
            .map(|var| SecretField {
                var: var.clone(),
                input: cx.new(|cx| {
                    InputState::new(window, cx)
                        .masked(true)
                        .placeholder(format!("Paste or type {var}"))
                }),
                captured: None,
            })
            .collect();
        Self {
            fields,
            submitting: false,
            error: None,
        }
    }

    fn values(&self, cx: &App) -> Result<BTreeMap<String, String>, String> {
        let mut env = BTreeMap::new();
        for field in &self.fields {
            let value = match &field.captured {
                Some(text) => text.clone(),
                None => field.input.read(cx).value().to_string(),
            };
            if value.is_empty() {
                return Err(format!("Fill in {} first.", field.var));
            }
            env.insert(field.var.clone(), value);
        }
        Ok(env)
    }

    pub(super) fn clear(&mut self, window: &mut Window, cx: &mut App) {
        for field in &mut self.fields {
            field.captured = None;
            field
                .input
                .update(cx, |state, cx| state.set_value("", window, cx));
        }
    }
}

impl ElseApp {
    pub(super) fn sync_secret_forms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        for request in pending {
            if !view.answered.contains(&request.id) {
                view.secret_forms
                    .entry(request.id)
                    .or_insert_with(|| SecretForm::new(&request, window, cx));
            }
        }
    }

    pub(super) fn clear_secret_forms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.view.as_mut() {
            for (_, mut form) in view.secret_forms.drain() {
                form.clear(window, cx);
            }
        }
    }

    fn capture_paste(
        &mut self,
        request: Uuid,
        field: usize,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self
            .view
            .as_mut()
            .and_then(|v| v.secret_forms.get_mut(&request))
        else {
            return;
        };
        if let Some(field) = form.fields.get_mut(field) {
            field
                .input
                .update(cx, |state, cx| state.set_value("", window, cx));
            field.captured = Some(text);
        }
        self.remeasure_row(RowKey::SecretRequest(request), cx);
    }

    fn replace_paste(
        &mut self,
        request: Uuid,
        field: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self
            .view
            .as_mut()
            .and_then(|v| v.secret_forms.get_mut(&request))
        else {
            return;
        };
        if let Some(field) = form.fields.get_mut(field) {
            field.captured = None;
            field.input.update(cx, |state, cx| state.focus(window, cx));
        }
        self.remeasure_row(RowKey::SecretRequest(request), cx);
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
        let Some(view) = self.view.as_mut().filter(|v| v.bot_id == bot) else {
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
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let row = v_flex().w_full();
        let form = view.secret_forms.get(&id);
        let (Some(form), true) = (form, request.is_pending()) else {
            let text = match request.status.as_str() {
                "stored" => format!(
                    "Saved the task secret {}. Every bot on this server can use it.",
                    request.name
                ),
                "cancelled" => format!(
                    "The request for {} was declined or withdrawn.",
                    request.name
                ),
                _ => format!("Sent {} to {bot_name}…", request.name),
            };
            return row.child(note(text)).into_any_element();
        };
        let app = cx.entity().downgrade();
        let busy = form.submitting || self.connection != Connection::Connected;
        let mut fields = v_flex().gap_2();
        for (ix, field) in form.fields.iter().enumerate() {
            let label = div().text_xs().font_semibold().child(field.var.clone());
            let control: AnyElement = match &field.captured {
                Some(text) => h_flex()
                    .gap_2()
                    .items_center()
                    .child(note(format!(
                        "Pasted {} lines ({} bytes), hidden.",
                        text.lines().count(),
                        text.len()
                    )))
                    .child(
                        Button::new(SharedString::from(format!("secret-replace-{id}-{ix}")))
                            .focus_ring(false)
                            .cursor_pointer()
                            .label("Replace")
                            .xsmall()
                            .ghost()
                            .disabled(busy)
                            .on_click(cx.listener(move |app, _, window, cx| {
                                app.replace_paste(id, ix, window, cx)
                            })),
                    )
                    .into_any_element(),
                None => {
                    let app = app.clone();
                    Input::new(&field.input)
                        .focus_ring(false)
                        .disabled(busy)
                        .on_paste(move |item, window, cx| {
                            let Some(text) = item.text().filter(|t| is_multiline(t)) else {
                                return false;
                            };
                            let _ = app
                                .update(cx, |app, cx| app.capture_paste(id, ix, text, window, cx));
                            true
                        })
                        .into_any_element()
                }
            };
            fields = fields.child(v_flex().gap_1().child(label).child(control));
        }
        let mut card = v_flex()
            .w_full()
            .gap_2()
            .p_3()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(
                div()
                    .font_semibold()
                    .child(format!("{bot_name} needs credentials: {}", request.name)),
            )
            .child(div().text_sm().child(request.reason.clone()))
            .child(note(format!(
                "Stored encrypted as the task secret {} for every bot on this server when you save, \
                 replacing one with the same name. {bot_name} never sees the values, \
                 and they are not added to the conversation. A multi-line value such as a \
                 private key can be pasted whole.",
                request.name
            )))
            .child(fields);
        if let Some(error) = &form.error {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        card = card.child(
            h_flex()
                .justify_end()
                .gap_2()
                .child(
                    Button::new(SharedString::from(format!("secret-decline-{id}")))
                        .focus_ring(false)
                        .cursor_pointer()
                        .label("Decline")
                        .small()
                        .ghost()
                        .disabled(busy)
                        .on_click(cx.listener(move |app, _, window, cx| {
                            app.answer_secret(id, false, window, cx)
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("secret-save-{id}")))
                        .focus_ring(false)
                        .cursor_pointer()
                        .label(if form.submitting {
                            "Saving…"
                        } else {
                            "Save and continue"
                        })
                        .small()
                        .primary()
                        .disabled(busy)
                        .on_click(cx.listener(move |app, _, window, cx| {
                            app.answer_secret(id, true, window, cx)
                        })),
                ),
        );
        row.child(card).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::is_multiline;

    #[test]
    fn only_inner_line_breaks_need_a_captured_paste() {
        assert!(is_multiline(
            "-----BEGIN KEY-----\nabc\n-----END KEY-----\n"
        ));
        assert!(is_multiline("a\r\nb"));
        assert!(!is_multiline("token-123\n"));
        assert!(!is_multiline("plain"));
    }
}
