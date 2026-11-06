use super::*;
use crate::api::McpConsent;

impl ElseApp {
    fn connect_mcp(
        &mut self,
        question: Uuid,
        consent: crate::api::McpConsent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let bot = view.bot_id;
        if view
            .question_submits
            .get(&question)
            .is_some_and(QuestionForm::locks)
        {
            return;
        }
        let current = view
            .conversation
            .as_ref()
            .and_then(|c| c.question(question))
            .filter(|q| q.is_pending())
            .and_then(Question::mcp_connection);
        if current != Some(&consent) {
            return;
        }
        view.question_submits.insert(
            question,
            QuestionForm {
                answer: QuestionAnswer::Single(APPROVE.into()),
                state: SubmitState::Authorizing,
            },
        );
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let mut signin =
                    crate::mcp_oauth::SignIn::start(client.clone(), bot, consent).await?;
                let opened = this
                    .update(cx, |app, cx| {
                        let pending = app
                            .view
                            .as_ref()
                            .filter(|v| v.bot_id == bot)
                            .and_then(|v| v.conversation.as_ref())
                            .and_then(|c| c.question(question))
                            .is_some_and(Question::is_pending);
                        if pending {
                            cx.open_url(&signin.authorization.authorization_url);
                        }
                        pending
                    })
                    .unwrap_or(false);
                if !opened {
                    return Err(ClientError::Protocol(
                        "The connection request was withdrawn.".into(),
                    ));
                }
                let callback = signin.wait().await?;
                let pending = this
                    .update(cx, |app, cx| {
                        let Some(view) = app.view.as_mut().filter(|v| v.bot_id == bot) else {
                            return false;
                        };
                        if !view
                            .conversation
                            .as_ref()
                            .and_then(|c| c.question(question))
                            .is_some_and(Question::is_pending)
                        {
                            return false;
                        }
                        if let Some(submit) = view.question_submits.get_mut(&question) {
                            submit.state = SubmitState::Sending;
                        }
                        app.remeasure_row(RowKey::Question(question), cx);
                        true
                    })
                    .unwrap_or(false);
                if !pending {
                    return Err(ClientError::Protocol(
                        "The connection request was withdrawn.".into(),
                    ));
                }
                signin.finish(callback).await?;
                client
                    .answer_question(bot, question, QuestionAnswer::Single(APPROVE.into()))
                    .await
            }
            .await;
            let _ = this.update_in(cx, |app, _, cx| {
                app.question_answered(bot, question, result, cx)
            });
        });
        view.mcp_signins.insert(question, task);
        self.remeasure_row(RowKey::Question(question), cx);
    }

    pub(super) fn render_mcp_card(
        &self,
        question: &Question,
        consent: &McpConsent,
        submit: Option<&QuestionForm>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let id = question.id;
        let pending = question.is_pending();
        let locked = !pending
            || self.connection != Connection::Connected
            || submit.is_some_and(QuestionForm::locks);
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
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Account connection"),
            )
            .child(div().font_semibold().child(question.title.clone()));
        if let Some(context) = &question.context {
            card = card.child(
                div()
                    .w_full()
                    .px_2p5()
                    .py_2()
                    .rounded(theme.radius)
                    .bg(theme.background)
                    .text_sm()
                    .whitespace_normal()
                    .child(context.clone()),
            );
        }
        card = card.child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Sign in using your browser. Only this bot may use the connected account."),
        );
        let connect_label = match submit.map(|s| &s.state) {
            Some(SubmitState::Authorizing) => "Waiting for sign-in…",
            Some(SubmitState::Sending) => "Connecting…",
            Some(SubmitState::Recorded) => "Recorded",
            _ => "Connect in browser",
        };
        let connection = consent.clone();
        let cancel_locked = !pending
            || self.connection != Connection::Connected
            || submit
                .is_some_and(|s| matches!(s.state, SubmitState::Sending | SubmitState::Recorded));
        card = card.child(
            h_flex()
                .w_full()
                .justify_end()
                .gap_2()
                .child(
                    Button::new(SharedString::from(format!("mcp-connect-{id}")))
                        .focus_ring(false)
                        .cursor_pointer()
                        .label(connect_label)
                        .small()
                        .primary()
                        .disabled(locked)
                        .on_click(cx.listener(move |app, _, window, cx| {
                            app.connect_mcp(id, connection.clone(), window, cx)
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("mcp-cancel-{id}")))
                        .focus_ring(false)
                        .cursor_pointer()
                        .label("Cancel")
                        .small()
                        .ghost()
                        .disabled(cancel_locked)
                        .on_click(cx.listener(move |app, _, window, cx| {
                            if let Some(view) = app.view.as_mut() {
                                view.mcp_signins.remove(&id);
                                view.question_submits.remove(&id);
                            }
                            app.answer_question(
                                id,
                                QuestionAnswer::Single(DENY.into()),
                                window,
                                cx,
                            );
                        })),
                ),
        );
        if let Some(SubmitState::Failed(error)) = submit.map(|s| &s.state) {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        if !pending {
            let result = if question.status == "answered"
                && question.answer.as_ref().and_then(QuestionAnswer::single) == Some(APPROVE)
            {
                "Account connected for this bot."
            } else {
                "Connection cancelled."
            };
            card = card.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(result),
            );
        }
        card.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{Question, Uuid};
    use serde_json::json;

    #[test]
    fn mcp_cards_parse_typed_consent_and_reject_malformed_metadata() {
        let mut payload = json!({
            "id": Uuid::new_v4(), "kind":"external", "title":"Connect fixture",
            "status":"pending", "choices":[],
            "action":{"mcp_connection":{"id":"fixture","provider":"https://issuer.test","scopes":["read"]}}
        });
        let question: Question = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(question.mcp_connection().unwrap().scopes, ["read"]);
        payload["action"]["mcp_connection"]["scopes"] = json!("read");
        assert!(serde_json::from_value::<Question>(payload.clone()).is_err());
        payload["action"] = json!({"tool":"computer_run","args":{"command":"echo hello"}});
        assert!(
            serde_json::from_value::<Question>(payload)
                .unwrap()
                .mcp_connection()
                .is_none()
        );
    }
}
