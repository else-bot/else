mod form;
mod oauth;

use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::*;
use uuid::Uuid;

use crate::api::{Client, EmailAccount, EmailPreset};
use form::Form;

struct Introduction {
    account: Uuid,
    bot: Uuid,
    request: Uuid,
}

pub struct EmailSettings {
    client: Arc<Client>,
    accounts: Vec<EmailAccount>,
    presets: Vec<EmailPreset>,
    form: Option<Form>,
    introduction: Option<Introduction>,
    busy: bool,
    loaded: bool,
    error: Option<String>,
    feedback: Option<String>,
    _request: Option<Task<()>>,
}

pub struct OpenBot(pub Uuid);
impl EventEmitter<OpenBot> for EmailSettings {}

impl EmailSettings {
    pub(crate) fn has_unsaved_work(&self, cx: &App) -> bool {
        self.busy
            || self
                .form
                .as_ref()
                .is_some_and(|form| form.has_unsaved_work(cx))
    }
    pub fn new(
        client: Arc<Client>,
        initial_bot: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            client,
            accounts: Vec::new(),
            presets: Vec::new(),
            form: None,
            introduction: None,
            busy: false,
            loaded: false,
            error: None,
            feedback: None,
            _request: None,
        };
        view.load(initial_bot, window, cx);
        view
    }

    fn load(&mut self, initial_bot: Option<Uuid>, window: &mut Window, cx: &mut Context<Self>) {
        self.busy = true;
        self.error = None;
        self.feedback = None;
        let client = self.client.clone();
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let (accounts, presets, bots) = tokio::join!(
                client.clone().email_accounts(),
                client.clone().email_presets(),
                client.bots()
            );
            let _ = this.update_in(cx, |view, window, cx| {
                view.busy = false;
                match (accounts, presets, bots) {
                    (Ok(accounts), Ok(presets), Ok(bots)) => {
                        view.accounts = accounts;
                        view.presets = presets;
                        view.loaded = true;
                        view.form = Some(Form::new(bots.bots, initial_bot, window, cx));
                    }
                    (Err(err), _, _) | (_, Err(err), _) | (_, _, Err(err)) => {
                        view.error = Some(err.to_string())
                    }
                }
                cx.notify();
            });
        }));
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let request = match form.request(&self.presets, cx) {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let bot = form.bot(cx);
        let client = self.client.clone();
        self.busy = true;
        self.error = None;
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = request.send(client).await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.busy = false;
                match result {
                    Ok(form::Connected::Authorization(signin)) => {
                        view.wait_for_signin(signin, bot, window, cx);
                    }
                    Ok(form::Connected::Account(account)) => {
                        view.connected(*account, bot, window, cx);
                    }
                    Err(err) => view.error = Some(err.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn wait_for_signin(
        &mut self,
        mut signin: oauth::SignIn,
        bot: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.open_url(&signin.authorization.authorization_url);
        if let Some(form) = &mut self.form {
            form.authorized(signin.authorization.authorization_url.clone());
        }
        self.busy = true;
        let client = self.client.clone();
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let callback = signin.wait().await?;
                this.update(cx, |view, cx| {
                    if let Some(form) = &mut view.form {
                        form.completing();
                    }
                    cx.notify();
                })
                .map_err(|_| "Email settings closed.".to_owned())?;
                client
                    .complete_email(
                        signin.authorization.request_id,
                        callback.state,
                        callback.code,
                    )
                    .await
                    .map_err(|err| err.to_string())
            }
            .await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.busy = false;
                if let Some(form) = &mut view.form {
                    form.cancel_authorization();
                }
                match result {
                    Ok(account) => view.connected(account, bot, window, cx),
                    Err(error) => view.error = Some(error),
                }
                cx.notify();
            });
        }));
    }

    fn cancel_signin(&mut self, cx: &mut Context<Self>) {
        self._request = None;
        if let Some(form) = &mut self.form {
            form.cancel_authorization();
        }
        self.busy = false;
        self.error = None;
        cx.notify();
    }

    fn connected(
        &mut self,
        account: EmailAccount,
        bot: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.feedback = Some(format!(
            "{} is connected. Every bot can use it when you ask.",
            account.config.address
        ));
        if let Some(form) = &mut self.form {
            form.clear_secrets(window, cx);
        }
        self.form = None;
        self.introduction = bot.map(|bot| Introduction {
            account: account.id,
            bot,
            request: Uuid::new_v4(),
        });
        self.accounts.push(account);
        if self.introduction.is_some() {
            self.introduce(window, cx);
        }
    }

    fn introduce(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(intro) = &self.introduction else {
            return;
        };
        let (account, bot, request) = (intro.account, intro.bot, intro.request);
        let client = self.client.clone();
        self.busy = true;
        self.error = None;
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.introduce_email(account, bot, request).await;
            let _ = this.update(cx, |view, cx| {
                view.busy = false;
                match result {
                    Ok(()) => {
                        view.introduction = None;
                        cx.emit(OpenBot(bot));
                    }
                    Err(err) => {
                        view.error = Some(format!(
                            "Email connected, but the bot conversation could not start. {err}"
                        ))
                    }
                }
                cx.notify();
            });
        }));
    }
}

impl Render for EmailSettings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut content = v_flex()
            .gap_5()
            .w_full()
            .child(div().text_xl().font_semibold().child("Email"))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Connect an inbox for any of your bots to use when you ask."),
            );
        for account in &self.accounts {
            content = content.child(
                h_flex()
                    .gap_3()
                    .justify_between()
                    .p_3()
                    .rounded(theme.radius)
                    .bg(theme.secondary)
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().font_semibold().child(account.config.address.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(account.config.provider.label()),
                            ),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        if account.state == "paused" {
                            "Paused"
                        } else {
                            match account.health.as_str() {
                                "connected" => "Connected",
                                "unknown" => "Checking connection…",
                                _ => "Check credentials and server settings",
                            }
                        },
                    )),
            );
        }
        if let Some(form) = &self.form {
            content = content.child(form.render(self.busy, cx));
        } else if self.loaded && self.introduction.is_none() {
            content = content.child(
                Button::new("email-add")
                    .focus_ring(false)
                    .cursor_pointer()
                    .primary()
                    .label("Add email account")
                    .disabled(self.busy)
                    .on_click(cx.listener(|view, _, window, cx| view.load(None, window, cx))),
            );
        }
        if let Some(feedback) = &self.feedback {
            content = content.child(div().text_sm().child(feedback.clone()));
        }
        if let Some(error) = &self.error {
            content = content.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
            if !self.loaded {
                content = content.child(
                    Button::new("email-reload")
                        .focus_ring(false)
                        .cursor_pointer()
                        .label("Try again")
                        .disabled(self.busy)
                        .on_click(cx.listener(|view, _, window, cx| view.load(None, window, cx))),
                );
            }
        }
        if self.introduction.is_some() {
            content = content.child(
                Button::new("email-retry-introduction")
                    .focus_ring(false)
                    .cursor_pointer()
                    .label(if self.busy {
                        "Starting conversation…"
                    } else {
                        "Start bot conversation"
                    })
                    .disabled(self.busy)
                    .on_click(cx.listener(|view, _, window, cx| view.introduce(window, cx))),
            );
        }
        if !self.loaded && self.busy {
            content = content.child("Loading email accounts…");
        }
        content
    }
}
