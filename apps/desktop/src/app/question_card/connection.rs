use super::*;
use crate::api::EmailProvider;
use flow::{Consent, Flow, Resolution};

mod flow;

pub(in crate::app) struct Form {
    flow: Flow,
    phase: Phase,
    request: Option<Task<()>>,
    /// The sign-in page opened for this card, so the owner can reopen it.
    signin_url: Option<String>,
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
    pub(in crate::app) fn sync_connections(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let conversation = view.conversation.as_ref();
        view.connections.retain(|id, _| {
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
                if !question.is_pending() || view.connections.contains_key(&id) {
                    return None;
                }
                Some((id, Consent::of(question)?))
            })
            .collect::<Vec<_>>();
        for (question, consent) in missing {
            let flow = Flow {
                client: self.client.clone(),
                bot: view.bot_id,
                question,
                consent,
            };
            view.connections.insert(
                question,
                Form {
                    flow,
                    phase: Phase::Checking,
                    request: None,
                    signin_url: None,
                },
            );
        }
        let checking = view
            .connections
            .iter()
            .filter_map(|(id, form)| {
                (matches!(form.phase, Phase::Checking) && form.request.is_none()).then_some(*id)
            })
            .collect::<Vec<_>>();
        for question in checking {
            self.check_connection(question, cx);
        }
    }

    fn resolved(
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
            .and_then(|v| v.connections.get_mut(&flow.question))
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

    fn check_connection(&mut self, question: Uuid, cx: &mut Context<Self>) {
        let Some(form) = self
            .view
            .as_mut()
            .and_then(|v| v.connections.get_mut(&question))
        else {
            return;
        };
        form.phase = Phase::Checking;
        let flow = form.flow.clone();
        form.request = Some(cx.spawn(async move |this, cx| {
            let result = flow.recover().await;
            let _ = this.update(cx, |app, cx| app.resolved(&flow, result, None, cx));
        }));
        self.remeasure_row(RowKey::Question(question), cx);
    }

    fn activate_connection(&mut self, question: Uuid, cx: &mut Context<Self>) {
        let Some(form) = self
            .view
            .as_mut()
            .and_then(|v| v.connections.get_mut(&question))
        else {
            return;
        };
        match form.phase {
            Phase::Ready(_) => self.sign_in(question, cx),
            Phase::CheckFailed(_) => self.check_connection(question, cx),
            Phase::ConfirmationRequired(_) => {
                form.phase = Phase::Confirming;
                let flow = form.flow.clone();
                form.request = Some(cx.spawn(async move |this, cx| {
                    let result = flow.confirm().await;
                    let _ = this.update(cx, |app, cx| app.resolved(&flow, result, None, cx));
                }));
                self.remeasure_row(RowKey::Question(question), cx);
            }
            _ => {}
        }
    }

    fn sign_in(&mut self, question: Uuid, cx: &mut Context<Self>) {
        let Some(form) = self
            .view
            .as_mut()
            .and_then(|v| v.connections.get_mut(&question))
        else {
            return;
        };
        form.phase = Phase::SigningIn;
        form.signin_url = None;
        let flow = form.flow.clone();
        form.request = Some(cx.spawn(async move |this, cx| {
            let signin = async {
                let mut signin = flow.sign_in().await?;
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
                            cx.open_url(signin.url());
                            if let Some(form) = app
                                .view
                                .as_mut()
                                .filter(|v| v.bot_id == flow.bot)
                                .and_then(|v| v.connections.get_mut(&question))
                            {
                                form.signin_url = Some(signin.url().to_owned());
                            }
                        }
                        pending
                    })
                    .unwrap_or(false);
                if !opened {
                    return Err(ClientError::Protocol(
                        "The connection request was withdrawn.".into(),
                    ));
                }
                signin
                    .complete(|| {
                        this.update(cx, |app, cx| {
                            let Some(view) = app.view.as_mut().filter(|v| v.bot_id == flow.bot)
                            else {
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
                            if let Some(form) = view.connections.get_mut(&question) {
                                form.phase = Phase::Exchanging;
                            }
                            app.remeasure_row(RowKey::Question(question), cx);
                            true
                        })
                        .unwrap_or(false)
                    })
                    .await
            }
            .await;
            let (result, hint) = match signin {
                Ok(()) => {
                    let _ = this.update(cx, |app, cx| {
                        if let Some(form) = app
                            .view
                            .as_mut()
                            .filter(|v| v.bot_id == flow.bot)
                            .and_then(|v| v.connections.get_mut(&question))
                        {
                            form.phase = Phase::Confirming;
                        }
                        app.remeasure_row(RowKey::Question(question), cx);
                    });
                    (flow.confirm().await, None)
                }
                Err(error) => (flow.recover().await, Some(error.to_string())),
            };
            let _ = this.update(cx, |app, cx| app.resolved(&flow, result, hint, cx));
        }));
        self.remeasure_row(RowKey::Question(question), cx);
    }

    fn reopen_signin(&mut self, question: Uuid, cx: &mut Context<Self>) {
        let url = self
            .view
            .as_ref()
            .and_then(|v| v.connections.get(&question))
            .filter(|form| matches!(form.phase, Phase::SigningIn))
            .and_then(|form| form.signin_url.clone());
        if let Some(url) = url {
            cx.open_url(&url);
        }
    }

    fn cancel_connection(&mut self, question: Uuid, cx: &mut Context<Self>) {
        let Some(form) = self
            .view
            .as_mut()
            .and_then(|v| v.connections.get_mut(&question))
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
            let _ = this.update(cx, |app, cx| app.resolved(&flow, result, None, cx));
        }));
        self.remeasure_row(RowKey::Question(question), cx);
    }

    pub(super) fn render_connection_card(
        &self,
        question: &Question,
        bot_name: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let id = question.id;
        let expanded = self
            .view
            .as_ref()
            .is_some_and(|v| v.expanded_questions.contains(&id));
        let note = |text: SharedString| {
            div()
                .text_xs()
                .whitespace_normal()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let account = Account::of(question, &theme);
        if !question.is_pending() {
            let connected = question.status == "answered"
                && question.answer.as_ref().and_then(QuestionAnswer::single) == Some(APPROVE);
            let (title, outcome) = if connected {
                (
                    format!("{} connected{}", account.provider, account.address_suffix()),
                    "Every bot can use it when you ask.".to_owned(),
                )
            } else if question.status == "skipped" {
                (
                    question.title.clone(),
                    "Skipped: you sent a new message instead. Nothing was connected.".to_owned(),
                )
            } else {
                (
                    question.title.clone(),
                    "Cancelled. Nothing was connected.".to_owned(),
                )
            };
            let mut card = v_flex()
                .w_full()
                .rounded(theme.radius_lg)
                .bg(theme.secondary)
                .child(
                    receipt(id, connected, title, outcome, expanded, &theme)
                        .when(expanded && question.context.is_some(), |button| {
                            button.rounded_b(px(0.))
                        })
                        .on_click(cx.listener(move |app, _, _, cx| {
                            app.toggle_question_details(id, cx);
                        })),
                );
            if expanded && let Some(context) = &question.context {
                card = card.child(
                    div()
                        .w_full()
                        .px_4()
                        .pb_4()
                        .text_sm()
                        .whitespace_normal()
                        .text_color(theme.muted_foreground)
                        .child(context.clone()),
                );
            }
            return card.into_any_element();
        }
        let form = self.view.as_ref().and_then(|v| v.connections.get(&id));
        let phase = form.map_or(&Phase::Checking, |f| &f.phase);
        let error = match phase {
            Phase::Ready(Some(error))
            | Phase::CheckFailed(error)
            | Phase::ConfirmationRequired(error) => Some(error.clone()),
            _ => None,
        };
        let still = cx.reduce_motion();
        let status = match phase {
            _ if error.is_some() => pill(id, "Needs attention", theme.danger, false, still),
            Phase::Ready(_) => pill(id, "Waiting for you", theme.warning, false, still),
            Phase::SigningIn => pill(id, "Signing in", theme.info, true, still),
            Phase::Exchanging | Phase::Confirming => {
                pill(id, "Connecting", theme.info, true, still)
            }
            Phase::Recorded => pill(id, "Connected", theme.success, false, still),
            Phase::Cancelling => pill(id, "Cancelling", theme.muted_foreground, true, still),
            _ => pill(id, "Checking", theme.muted_foreground, true, still),
        };
        let ready_label = form.map_or("Connect in browser", |f| f.flow.ready_label());
        let mut card = v_flex()
            .w_full()
            .gap_3()
            .p_4()
            .rounded(theme.radius_lg)
            .bg(theme.secondary)
            .child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .child(tile(account.icon, account.tint))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                div()
                                    .font_semibold()
                                    .whitespace_normal()
                                    .child(question.title.clone()),
                            )
                            .child(note(
                                format!("{} · requested by {bot_name}", account.source).into(),
                            )),
                    )
                    .child(status),
            );
        if let Some(context) = &question.context {
            card = card.child(
                div()
                    .text_sm()
                    .whitespace_normal()
                    .text_color(theme.muted_foreground)
                    .child(context.clone()),
            );
        }
        if let Some(error) = error {
            card = card.child(callout(error, &theme));
        } else {
            let now = match phase {
                Phase::Ready(_) => Some(0),
                Phase::SigningIn => Some(1),
                Phase::Exchanging | Phase::Confirming | Phase::Recorded => Some(2),
                _ => None,
            };
            if let Some(now) = now {
                let recorded = matches!(phase, Phase::Recorded);
                let labels = [
                    if now > 0 {
                        "Browser opened"
                    } else {
                        ready_label
                    },
                    "Grant access",
                    "Confirm",
                ];
                card = card.child(h_flex().w_full().gap_1p5().children(
                    labels.into_iter().enumerate().map(|(ix, label)| {
                        let done = ix < now || recorded;
                        let (bar, text) = if done {
                            (theme.success, theme.muted_foreground)
                        } else if ix == now {
                            (theme.info, theme.foreground)
                        } else {
                            (theme.border, theme.muted_foreground.opacity(0.7))
                        };
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .text_xs()
                            .text_color(text)
                            .child(div().h(px(3.)).rounded_full().bg(bar))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .when(done, |row| {
                                        row.child(Icon::new(IconName::Check).size_3())
                                    })
                                    .child(div().truncate().child(if done {
                                        label.to_owned()
                                    } else {
                                        format!("{} · {label}", ix + 1)
                                    })),
                            )
                    }),
                ));
            }
            let fact = |label: &'static str, value: AnyElement| {
                h_flex()
                    .w_full()
                    .items_start()
                    .gap_3()
                    .text_xs()
                    .child(
                        div()
                            .w(px(76.))
                            .flex_none()
                            .text_color(theme.muted_foreground)
                            .child(label),
                    )
                    .child(div().flex_1().min_w_0().whitespace_normal().child(value))
            };
            let mut facts = v_flex()
                .w_full()
                .gap_1p5()
                .px_3()
                .py_2p5()
                .rounded(theme.radius)
                .bg(theme.background);
            if let Some(address) = &account.address {
                facts = facts.child(fact("Account", address.clone().into_any_element()));
            }
            if !account.scopes.is_empty() {
                facts = facts.child(fact(
                    "Scopes",
                    h_flex()
                        .flex_wrap()
                        .gap_1()
                        .children(account.scopes.iter().map(|scope| {
                            div()
                                .px_1p5()
                                .rounded(px(5.))
                                .bg(theme.muted)
                                .font_family(theme.mono_font_family.clone())
                                .child(scope.clone())
                        }))
                        .into_any_element(),
                ));
            }
            card = card.child(
                facts
                    .child(fact(
                        "Used by",
                        "Every bot on this server, when you ask".into_any_element(),
                    ))
                    .child(fact(
                        "Credentials",
                        "Stored on the server, never in chat".into_any_element(),
                    )),
            );
        }
        let locked = self.connection != Connection::Connected;
        let can_cancel = matches!(
            phase,
            Phase::Ready(_) | Phase::SigningIn | Phase::CheckFailed(_)
        );
        let hint: Option<(Option<IconName>, &'static str)> = match phase {
            Phase::Ready(None) => Some((Some(IconName::Lock), "Opens your browser")),
            Phase::SigningIn => {
                Some((None, "Finish in your browser. This card updates by itself."))
            }
            _ => None,
        };
        let action = match phase {
            Phase::Ready(None) => Some((ready_label, Some(IconName::ExternalLink), true)),
            Phase::Ready(Some(_)) => Some(("Sign in again", Some(IconName::ExternalLink), true)),
            Phase::CheckFailed(_) => Some(("Check again", Some(IconName::RefreshCw), true)),
            Phase::ConfirmationRequired(_) => {
                Some(("Retry confirmation", Some(IconName::RefreshCw), true))
            }
            Phase::Checking => Some(("Checking…", None, false)),
            Phase::Exchanging => Some(("Connecting…", None, false)),
            Phase::Confirming => Some(("Confirming…", None, false)),
            Phase::Cancelling => Some(("Cancelling…", None, false)),
            Phase::Recorded => Some(("Connected", None, false)),
            Phase::SigningIn => None,
        };
        let mut footer = h_flex()
            .w_full()
            .gap_2()
            .flex_wrap()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .when_some(hint, |row, (icon, text)| {
                        row.when_some(icon, |row, icon| row.child(Icon::new(icon).size_3()))
                            .child(text)
                    }),
            )
            .child(
                Button::new(SharedString::from(format!("connection-cancel-{id}")))
                    .focus_ring(false)
                    .label("Cancel")
                    .small()
                    .ghost()
                    .disabled(locked || !can_cancel)
                    .when(!locked && can_cancel, |button| button.cursor_pointer())
                    .on_click(cx.listener(move |app, _, _, cx| app.cancel_connection(id, cx))),
            );
        if matches!(phase, Phase::SigningIn) {
            let reopen = form.is_some_and(|f| f.signin_url.is_some());
            footer = footer.child(
                Button::new(SharedString::from(format!("connection-reopen-{id}")))
                    .focus_ring(false)
                    .icon(IconName::ExternalLink)
                    .label("Reopen browser")
                    .small()
                    .outline()
                    .disabled(locked || !reopen)
                    .when(!locked && reopen, |button| button.cursor_pointer())
                    .on_click(cx.listener(move |app, _, _, cx| app.reopen_signin(id, cx))),
            );
        }
        if let Some((label, icon, active)) = action {
            footer = footer.child(
                Button::new(SharedString::from(format!("connection-connect-{id}")))
                    .focus_ring(false)
                    .when_some(icon, |button, icon| button.icon(icon))
                    .label(label)
                    .small()
                    .primary()
                    .disabled(locked || !active)
                    .when(!locked && active, |button| button.cursor_pointer())
                    .on_click(cx.listener(move |app, _, _, cx| app.activate_connection(id, cx))),
            );
        }
        card.child(footer).into_any_element()
    }
}

/// What a connection card signs in to, as its header and facts show it.
struct Account {
    icon: IconName,
    tint: Hsla,
    /// The service name, such as Gmail or an MCP server's host.
    provider: String,
    /// Where the request points: the provider, or the MCP server's issuer.
    source: String,
    address: Option<String>,
    scopes: Vec<String>,
}

impl Account {
    fn of(question: &Question, theme: &Theme) -> Self {
        if let Some(email) = question.email_connection() {
            let provider = email.provider.label().to_owned();
            return Self {
                icon: IconName::Mail,
                tint: match email.provider {
                    EmailProvider::Gmail => theme.danger,
                    EmailProvider::Microsoft => theme.info,
                    _ => theme.muted_foreground,
                },
                source: provider.clone(),
                provider,
                address: Some(email.address.clone()),
                scopes: Vec::new(),
            };
        }
        let mcp = question.mcp_connection();
        let id = mcp.map(|c| c.id.clone()).unwrap_or_default();
        let host = mcp
            .and_then(|c| c.provider.as_deref())
            .map(|provider| {
                provider
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .trim_end_matches('/')
                    .to_owned()
            })
            .filter(|host| !host.is_empty());
        Self {
            icon: IconName::Plug,
            tint: theme.primary,
            provider: id.clone(),
            source: host.unwrap_or(id),
            address: None,
            scopes: mcp.map(|c| c.scopes.clone()).unwrap_or_default(),
        }
    }

    fn address_suffix(&self) -> String {
        self.address
            .as_ref()
            .map(|address| format!(" · {address}"))
            .unwrap_or_default()
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
        payload["action"] =
            json!({"email_connection":{"provider":"gmail","address":"owner@gmail.com"}});
        let inbox: Question = serde_json::from_value(payload.clone()).unwrap();
        assert!(inbox.mcp_connection().is_none());
        assert_eq!(inbox.email_connection().unwrap().address, "owner@gmail.com");
        payload["action"] = json!({"tool":"computer_run","args":{"command":"echo hello"}});
        assert!(
            serde_json::from_value::<Question>(payload)
                .unwrap()
                .mcp_connection()
                .is_none()
        );
    }
}
