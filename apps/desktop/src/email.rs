mod form;
pub(crate) mod oauth;

use else_wire::email::AccountState;

use else_wire::email::AccountHealth;

use crate::controls::{Control as _, Scale, Tier};
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
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
    /// The account a confirmed removal is waiting on.
    removing: Option<Uuid>,
    _request: Option<Task<()>>,
    _removal: Option<Task<()>>,
}

pub struct OpenBot(pub Uuid);

/// How a confirmed removal ended.
enum Removal {
    /// The server removed the account; the reloaded list, or why it failed.
    Removed(Result<Vec<EmailAccount>, String>),
    Failed(String),
}

impl Removal {
    /// A failed request counts as removed when the reloaded list no longer
    /// has the account: the server may have removed it and lost its reply.
    fn reconcile(
        account: Uuid,
        removed: Result<(), String>,
        listed: Result<Vec<EmailAccount>, String>,
    ) -> Self {
        match (removed, listed) {
            (Ok(()), listed) => Self::Removed(listed),
            (Err(_), Ok(listed)) if listed.iter().all(|a| a.id != account) => {
                Self::Removed(Ok(listed))
            }
            (Err(err), _) => Self::Failed(err),
        }
    }
}

const REMOVAL_NOTICE: &str = "Your bots lose access to this inbox, and their mail triggers \
    for it stop. The saved sign-in is erased. Messages stay in the mailbox, and you can add \
    the account again.";
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
            removing: None,
            _request: None,
            _removal: None,
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
                signin.finish(callback).await.map_err(|err| err.to_string())
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

    fn confirm_remove(&mut self, account: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(address) = self
            .accounts
            .iter()
            .find(|a| a.id == account)
            .map(|a| a.config.address.clone())
        else {
            return;
        };
        let view = cx.weak_entity();
        let cancel = cx.focus_handle();
        let focus_once = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_once.replace(false) {
                cancel.focus(window, cx);
            }
            let view = view.clone();
            dialog
                .w(px(440.))
                .title(format!("Remove {address}?"))
                .close_button(false)
                .on_ok(|_, _, _| false)
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(REMOVAL_NOTICE),
                )
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("email-remove-cancel")
                                .control(Tier::Secondary, Scale::Standard, false, cx)
                                .label("Cancel")
                                .track_focus(&cancel)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("email-remove-confirm")
                                .control(Tier::Danger, Scale::Standard, false, cx)
                                .label("Remove account")
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ = view.update(cx, |view, cx| {
                                        view.remove(account, window, cx);
                                    });
                                }),
                        ),
                )
        });
    }

    fn remove(&mut self, account: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if !self.start_removal(account) {
            return;
        }
        let client = self.client.clone();
        self._removal = Some(cx.spawn_in(window, async move |this, cx| {
            let removed = client.clone().remove_email(account).await;
            let listed = client.email_accounts().await;
            let outcome = Removal::reconcile(
                account,
                removed.map_err(|err| err.to_string()),
                listed.map_err(|err| err.to_string()),
            );
            let _ = this.update(cx, |view, cx| {
                view.settle_removal(account, outcome);
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Marks `account` as being removed; false while another removal runs.
    fn start_removal(&mut self, account: Uuid) -> bool {
        if self.removing.is_some() {
            return false;
        }
        self.removing = Some(account);
        self.error = None;
        self.feedback = None;
        true
    }

    fn settle_removal(&mut self, account: Uuid, outcome: Removal) {
        if self.removing != Some(account) {
            return;
        }
        self.removing = None;
        let address = self
            .accounts
            .iter()
            .find(|a| a.id == account)
            .map(|a| a.config.address.clone())
            .unwrap_or_default();
        if matches!(outcome, Removal::Removed(_))
            && self
                .introduction
                .as_ref()
                .is_some_and(|i| i.account == account)
        {
            self.introduction = None;
        }
        match outcome {
            Removal::Removed(Ok(accounts)) => {
                self.accounts = accounts;
                self.feedback = Some(format!(
                    "{address} is removed. Add it again any time to reconnect."
                ));
            }
            Removal::Removed(Err(error)) => {
                self.accounts.retain(|a| a.id != account);
                self.error = Some(format!(
                    "{address} is removed, but the list could not reload. {error}"
                ));
            }
            Removal::Failed(error) => self.error = Some(error),
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
            .gap_6()
            .w_full()
            .child(crate::panel_kit::page_heading(
                "Email",
                "Connect an inbox for any of your bots to use when you ask.",
                Vec::new(),
                cx,
            ));
        if !self.accounts.is_empty() {
            let rows: Vec<AnyElement> = self
                .accounts
                .iter()
                .map(|account| {
                    let status = if account.state == AccountState::Paused {
                        "Paused"
                    } else {
                        match account.health {
                            AccountHealth::Connected => "Connected",
                            AccountHealth::Unchecked => "Checking connection…",
                            AccountHealth::Failing(_) => "Check credentials and server settings",
                        }
                    };
                    let id = account.id;
                    let removing = self.removing == Some(id);
                    crate::panel_kit::list_row()
                        .child(crate::panel_kit::row_icon(
                            gpui_kit::component::Icon::new(IconName::Mail),
                            cx,
                        ))
                        .child(crate::panel_kit::row_text(
                            account.config.address.clone(),
                            if removing {
                                "Removing…".to_owned()
                            } else {
                                format!("{} · {status}", account.config.provider.label())
                            },
                            cx,
                        ))
                        .child(
                            Button::new(SharedString::from(format!("email-remove-{id}")))
                                .focus_ring(false)
                                .cursor_pointer()
                                .ghost()
                                .small()
                                .icon(IconName::Trash)
                                .text_color(theme.danger)
                                .accessibility_label(format!("Remove {}", account.config.address))
                                .disabled(self.removing.is_some())
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.confirm_remove(id, window, cx);
                                })),
                        )
                        .into_any_element()
                })
                .collect();
            content = content.child(crate::panel_kit::grouped_list(rows, cx));
        }
        if let Some(form) = &self.form {
            content = content.child(form.render(self.busy, cx));
        } else if self.loaded && self.introduction.is_none() {
            content = content.child(
                Button::new("email-add")
                    .control(Tier::Primary, Scale::Standard, self.busy, cx)
                    .label("Add email account")
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
                        .control(Tier::Secondary, Scale::Standard, self.busy, cx)
                        .label("Try again")
                        .on_click(cx.listener(|view, _, window, cx| view.load(None, window, cx))),
                );
            }
        }
        if self.introduction.is_some() {
            content = content.child(
                Button::new("email-retry-introduction")
                    .control(Tier::Secondary, Scale::Standard, self.busy, cx)
                    .label(if self.busy {
                        "Starting conversation…"
                    } else {
                        "Start bot conversation"
                    })
                    .on_click(cx.listener(|view, _, window, cx| view.introduce(window, cx))),
            );
        }
        if !self.loaded && self.busy {
            content = content.child("Loading email accounts…");
        }
        content
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: u128, address: &str) -> EmailAccount {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::from_u128(id),
            "config": {
                "provider": "gmail", "address": address, "username": address,
                "imap": { "host": "imap.gmail.com", "port": 993, "start_tls": false },
                "smtp": { "host": "smtp.gmail.com", "port": 465, "start_tls": false },
                "archive_folder": "Archive", "trash_folder": "Trash",
                "drafts_folder": "Drafts", "sent_folder": "Sent",
            },
            "state": "active",
            "health": "connected",
        }))
        .unwrap()
    }

    fn settings(accounts: Vec<EmailAccount>) -> EmailSettings {
        let origin = crate::config::Origin::parse("http://127.0.0.1:1").unwrap();
        let token = crate::config::OwnerToken::parse(&"T".repeat(43)).unwrap();
        EmailSettings {
            client: Arc::new(
                Client::new(origin, &token, tokio::runtime::Handle::current()).unwrap(),
            ),
            accounts,
            presets: Vec::new(),
            form: None,
            introduction: None,
            busy: false,
            loaded: true,
            error: None,
            feedback: None,
            removing: None,
            _request: None,
            _removal: None,
        }
    }

    #[tokio::test]
    async fn a_confirmed_removal_runs_once_and_drops_the_row() {
        let gmail = account(1, "owner@gmail.com");
        let work = account(2, "work@example.com");
        let mut view = settings(vec![gmail.clone(), work.clone()]);
        assert!(view.start_removal(gmail.id));
        assert!(!view.start_removal(work.id), "one removal runs at a time");
        view.settle_removal(work.id, Removal::Removed(Ok(Vec::new())));
        assert_eq!(
            view.accounts.len(),
            2,
            "another account's result is ignored"
        );
        view.settle_removal(gmail.id, Removal::Removed(Ok(vec![work.clone()])));
        assert_eq!(view.removing, None);
        assert_eq!(
            view.accounts.iter().map(|a| a.id).collect::<Vec<_>>(),
            [work.id]
        );
        assert!(
            view.feedback
                .as_deref()
                .unwrap()
                .starts_with("owner@gmail.com is removed"),
            "{:?}",
            view.feedback
        );
    }

    #[tokio::test]
    async fn a_lost_reply_counts_as_removed_once_the_list_lacks_the_account() {
        let gmail = account(1, "owner@gmail.com");
        let work = account(2, "work@example.com");
        let lost = || Err("timed out".to_owned());
        assert!(matches!(
            Removal::reconcile(gmail.id, lost(), Ok(vec![work.clone()])),
            Removal::Removed(Ok(listed)) if listed.len() == 1
        ));
        assert!(matches!(
            Removal::reconcile(gmail.id, lost(), Ok(vec![gmail.clone(), work])),
            Removal::Failed(err) if err == "timed out"
        ));
        assert!(matches!(
            Removal::reconcile(gmail.id, lost(), Err("offline".into())),
            Removal::Failed(_)
        ));
    }

    #[tokio::test]
    async fn removing_an_account_drops_its_pending_bot_introduction() {
        let gmail = account(1, "owner@gmail.com");
        let mut view = settings(vec![gmail.clone()]);
        view.introduction = Some(Introduction {
            account: gmail.id,
            bot: Uuid::from_u128(9),
            request: Uuid::from_u128(10),
        });
        assert!(view.start_removal(gmail.id));
        view.settle_removal(gmail.id, Removal::Removed(Ok(Vec::new())));
        assert!(
            view.introduction.is_none(),
            "Add email account shows again after the removal"
        );
    }

    #[tokio::test]
    async fn a_failed_removal_keeps_the_row_and_shows_why() {
        let gmail = account(1, "owner@gmail.com");
        let mut view = settings(vec![gmail.clone()]);
        assert!(view.start_removal(gmail.id));
        view.settle_removal(gmail.id, Removal::Failed("Server unavailable".into()));
        assert_eq!(view.accounts.len(), 1);
        assert_eq!(view.error.as_deref(), Some("Server unavailable"));
        assert!(view.start_removal(gmail.id), "the owner can try again");

        view.settle_removal(gmail.id, Removal::Removed(Err("timed out".into())));
        assert!(
            view.accounts.is_empty(),
            "a removed account leaves the list"
        );
        assert!(
            view.error.as_deref().unwrap().contains("could not reload"),
            "{:?}",
            view.error
        );
    }
}
