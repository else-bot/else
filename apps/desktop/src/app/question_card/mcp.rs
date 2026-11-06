use super::*;
use flow::{Flow, Resolution};

mod flow;

pub(in crate::app) struct Form {
    flow: Flow,
    phase: Phase,
    request: Option<Task<()>>,
}

enum Phase {
    Checking,
    Ready(Option<String>),
    SigningIn,
    Exchanging,
    Confirming,
    ConfirmationRequired(String),
    CheckFailed(String),
    Cancelling,
    Recorded,
}

impl ElseApp {
    pub(in crate::app) fn sync_mcp_connections(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let conversation = view.conversation.as_ref();
        view.mcp_connections.retain(|id, _| {
            conversation
                .and_then(|c| c.question(*id))
                .is_some_and(Question::is_pending)
        });
        let missing = view
            .rows
            .iter()
            .filter_map(|row| {
                let RowKey::Question(id) = row.key else {
                    return None;
                };
                let question = conversation?.question(id)?;
                if !question.is_pending() || view.mcp_connections.contains_key(&id) {
                    return None;
                }
                Some((id, question.mcp_connection()?.clone()))
            })
            .collect::<Vec<_>>();
        for (question, consent) in missing {
            let flow = Flow {
                client: self.client.clone(),
                bot: view.bot_id,
                question,
                consent,
            };
            view.mcp_connections.insert(
                question,
                Form {
                    flow,
                    phase: Phase::Checking,
                    request: None,
                },
            );
        }
        let checking = view
            .mcp_connections
            .iter()
            .filter_map(|(id, form)| {
                (matches!(form.phase, Phase::Checking) && form.request.is_none()).then_some(*id)
            })
            .collect::<Vec<_>>();
        for question in checking {
            self.check_mcp_connection(question, cx);
        }
    }

    fn mcp_resolved(
        &mut self,
        flow: &Flow,
        result: Result<Resolution, ClientError>,
        hint: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if matches!(&result, Err(ClientError::Unauthorized)) {
            self.connection = Connection::InvalidCredentials(ClientError::Unauthorized.to_string());
        }
        let Some(form) = self
            .view
            .as_mut()
            .filter(|v| v.bot_id == flow.bot)
            .and_then(|v| v.mcp_connections.get_mut(&flow.question))
        else {
            return;
        };
        form.phase = match result {
            Ok(Resolution::Disconnected) => Phase::Ready(hint),
            Ok(Resolution::ConfirmationRequired(error)) => Phase::ConfirmationRequired(error),
            Ok(Resolution::Recorded) => Phase::Recorded,
            Err(error) => Phase::CheckFailed(error.to_string()),
        };
        self.remeasure_row(RowKey::Question(flow.question), cx);
    }

    fn check_mcp_connection(&mut self, question: Uuid, cx: &mut Context<Self>) {
        let Some(form) = self
            .view
            .as_mut()
            .and_then(|v| v.mcp_connections.get_mut(&question))
        else {
            return;
        };
        form.phase = Phase::Checking;
        let flow = form.flow.clone();
        form.request = Some(cx.spawn(async move |this, cx| {
            let result = flow.recover().await;
            let _ = this.update(cx, |app, cx| app.mcp_resolved(&flow, result, None, cx));
        }));
        self.remeasure_row(RowKey::Question(question), cx);
    }

    fn activate_mcp_connection(&mut self, question: Uuid, cx: &mut Context<Self>) {
        let Some(form) = self
            .view
            .as_mut()
            .and_then(|v| v.mcp_connections.get_mut(&question))
        else {
            return;
        };
        match form.phase {
            Phase::Ready(_) => self.sign_in_mcp(question, cx),
            Phase::CheckFailed(_) => self.check_mcp_connection(question, cx),
            Phase::ConfirmationRequired(_) => {
                form.phase = Phase::Confirming;
                let flow = form.flow.clone();
                form.request = Some(cx.spawn(async move |this, cx| {
                    let result = flow.confirm().await;
                    let _ = this.update(cx, |app, cx| app.mcp_resolved(&flow, result, None, cx));
                }));
                self.remeasure_row(RowKey::Question(question), cx);
            }
            _ => {}
        }
    }

    fn sign_in_mcp(&mut self, question: Uuid, cx: &mut Context<Self>) {
        let Some(form) = self
            .view
            .as_mut()
            .and_then(|v| v.mcp_connections.get_mut(&question))
        else {
            return;
        };
        form.phase = Phase::SigningIn;
        let flow = form.flow.clone();
        form.request = Some(cx.spawn(async move |this, cx| {
            let signin = async {
                let mut signin = crate::mcp_oauth::SignIn::start(
                    flow.client.clone(),
                    flow.bot,
                    flow.consent.clone(),
                )
                .await?;
                let opened = this
                    .update(cx, |app, cx| {
                        let pending = app
                            .view
                            .as_ref()
                            .filter(|v| v.bot_id == flow.bot)
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
                        let Some(view) = app.view.as_mut().filter(|v| v.bot_id == flow.bot) else {
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
                        if let Some(form) = view.mcp_connections.get_mut(&question) {
                            form.phase = Phase::Exchanging;
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
                signin.finish(callback).await
            }
            .await;
            let (result, hint) = match signin {
                Ok(()) => {
                    let _ = this.update(cx, |app, cx| {
                        if let Some(form) = app
                            .view
                            .as_mut()
                            .filter(|v| v.bot_id == flow.bot)
                            .and_then(|v| v.mcp_connections.get_mut(&question))
                        {
                            form.phase = Phase::Confirming;
                        }
                        app.remeasure_row(RowKey::Question(question), cx);
                    });
                    (flow.confirm().await, None)
                }
                Err(error) => (flow.recover().await, Some(error.to_string())),
            };
            let _ = this.update(cx, |app, cx| app.mcp_resolved(&flow, result, hint, cx));
        }));
        self.remeasure_row(RowKey::Question(question), cx);
    }

    fn cancel_mcp_connection(&mut self, question: Uuid, cx: &mut Context<Self>) {
        let Some(form) = self
            .view
            .as_mut()
            .and_then(|v| v.mcp_connections.get_mut(&question))
        else {
            return;
        };
        if !matches!(
            form.phase,
            Phase::Ready(_) | Phase::SigningIn | Phase::CheckFailed(_)
        ) {
            return;
        }
        form.phase = Phase::Cancelling;
        let flow = form.flow.clone();
        form.request = Some(cx.spawn(async move |this, cx| {
            let result = flow.cancel().await;
            let _ = this.update(cx, |app, cx| app.mcp_resolved(&flow, result, None, cx));
        }));
        self.remeasure_row(RowKey::Question(question), cx);
    }

    pub(super) fn render_mcp_card(
        &self,
        question: &Question,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let id = question.id;
        let pending = question.is_pending();
        let phase = self
            .view
            .as_ref()
            .and_then(|v| v.mcp_connections.get(&id))
            .map(|f| &f.phase);
        let label = match phase {
            _ if !pending
                && question.answer.as_ref().and_then(QuestionAnswer::single) == Some(APPROVE) =>
            {
                "Connected"
            }
            _ if !pending => "Closed",
            Some(Phase::Ready(_)) => "Connect in browser",
            Some(Phase::SigningIn) => "Waiting for sign-in…",
            Some(Phase::Exchanging) => "Connecting…",
            Some(Phase::Confirming) => "Confirming connection…",
            Some(Phase::ConfirmationRequired(_)) => "Retry confirmation",
            Some(Phase::CheckFailed(_)) => "Retry connection check",
            Some(Phase::Cancelling) => "Cancelling…",
            Some(Phase::Recorded) => "Recorded",
            _ => "Checking connection…",
        };
        let locked = !pending || self.connection != Connection::Connected;
        let can_activate = matches!(
            phase,
            Some(Phase::Ready(_) | Phase::ConfirmationRequired(_) | Phase::CheckFailed(_))
        );
        let can_cancel = matches!(
            phase,
            Some(Phase::Ready(_) | Phase::SigningIn | Phase::CheckFailed(_))
        );
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
        card = card
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Sign in using your browser. Every bot may use the connected account."),
            )
            .child(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("mcp-connect-{id}")))
                            .focus_ring(false)
                            .cursor_pointer()
                            .label(label)
                            .small()
                            .primary()
                            .disabled(locked || !can_activate)
                            .on_click(cx.listener(move |app, _, _, cx| {
                                app.activate_mcp_connection(id, cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("mcp-cancel-{id}")))
                            .focus_ring(false)
                            .cursor_pointer()
                            .label("Cancel")
                            .small()
                            .ghost()
                            .disabled(locked || !can_cancel)
                            .on_click(
                                cx.listener(move |app, _, _, cx| app.cancel_mcp_connection(id, cx)),
                            ),
                    ),
            );
        let error = match phase {
            Some(
                Phase::Ready(Some(error))
                | Phase::CheckFailed(error)
                | Phase::ConfirmationRequired(error),
            ) => Some(error),
            _ => None,
        };
        if let Some(error) = error {
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
                "Account connected for every bot."
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
