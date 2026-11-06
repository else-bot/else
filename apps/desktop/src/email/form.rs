use super::*;
use crate::api::{Bot, ClientError, EmailAuthorization, EmailConfig, EmailEndpoint, EmailProvider};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::{IndexPath, Selectable as _};

#[derive(Clone, PartialEq, Eq)]
struct BotChoice {
    id: Option<Uuid>,
    name: SharedString,
}

impl SearchableListItem for BotChoice {
    type Value = Option<Uuid>;
    fn title(&self) -> SharedString {
        self.name.clone()
    }
    fn value(&self) -> &Self::Value {
        &self.id
    }
}

pub(super) struct Form {
    provider: EmailProvider,
    address: Entity<InputState>,
    password: Entity<InputState>,
    username: Entity<InputState>,
    imap_host: Entity<InputState>,
    imap_port: Entity<InputState>,
    smtp_host: Entity<InputState>,
    smtp_port: Entity<InputState>,
    client_id: Entity<InputState>,
    client_secret: Entity<InputState>,
    redirect_uri: Entity<InputState>,
    callback: Entity<InputState>,
    bot: Entity<SelectState<Vec<BotChoice>>>,
    authorization: Option<Authorization>,
    _inputs: Vec<Subscription>,
}

enum Authorization {
    Manual(EmailAuthorization),
    Google(String),
    CompletingGoogle,
}

pub(super) enum Request {
    Google,
    Password(EmailConfig, String),
    Authorize {
        provider: EmailProvider,
        address: String,
        client_id: String,
        client_secret: Option<String>,
        redirect_uri: String,
    },
    Complete {
        request_id: Uuid,
        state: String,
        code: String,
    },
}

pub(super) enum Connected {
    Google(super::google::SignIn),
    Account(Box<EmailAccount>),
    Authorization(EmailAuthorization),
}

impl Request {
    pub(super) async fn send(self, client: Arc<Client>) -> Result<Connected, ClientError> {
        match self {
            Self::Google => super::google::SignIn::start(client)
                .await
                .map(Connected::Google),
            Self::Password(config, password) => client
                .connect_email(config, password)
                .await
                .map(|account| Connected::Account(Box::new(account))),
            Self::Authorize {
                provider,
                address,
                client_id,
                client_secret,
                redirect_uri,
            } => client
                .authorize_email(provider, address, client_id, client_secret, redirect_uri)
                .await
                .map(Connected::Authorization),
            Self::Complete {
                request_id,
                state,
                code,
            } => client
                .complete_email(request_id, state, code)
                .await
                .map(|account| Connected::Account(Box::new(account))),
        }
    }
}

impl Form {
    pub(super) fn new(
        bots: Vec<Bot>,
        initial_bot: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<EmailSettings>,
    ) -> Self {
        let mut input = |value: &str, placeholder: &str, masked| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(value.to_owned())
                    .placeholder(placeholder.to_owned())
                    .masked(masked)
            })
        };
        let address = input("", "you@example.com", false);
        let password = input("", "App password", true);
        let username = input("", "Same as email address", false);
        let imap_host = input("", "imap.example.com", false);
        let imap_port = input("993", "993", false);
        let smtp_host = input("", "smtp.example.com", false);
        let smtp_port = input("465", "465 or 587", false);
        let client_id = input("", "OAuth application client ID", false);
        let client_secret = input("", "Optional client secret", true);
        let redirect_uri = input(
            "http://localhost",
            "Your app's registered redirect URI",
            false,
        );
        let callback = input("", "Paste the full redirect URL after signing in", true);
        let choices: Vec<_> = std::iter::once(BotChoice {
            id: None,
            name: "No bot for now".into(),
        })
        .chain(
            bots.into_iter()
                .filter(|b| b.cleanup.is_none())
                .map(|b| BotChoice {
                    id: Some(b.id),
                    name: b.name.into(),
                }),
        )
        .collect();
        let selected = choices
            .iter()
            .position(|b| b.id == initial_bot)
            .unwrap_or(0);
        let bot =
            cx.new(|cx| SelectState::new(choices, Some(IndexPath::new(selected)), window, cx));
        let inputs = [
            &address,
            &password,
            &username,
            &imap_host,
            &imap_port,
            &smtp_host,
            &smtp_port,
            &client_id,
            &client_secret,
            &redirect_uri,
            &callback,
        ]
        .into_iter()
        .map(|input| cx.observe(input, |_, _, cx| cx.notify()))
        .collect();
        Self {
            provider: EmailProvider::Gmail,
            address,
            password,
            username,
            imap_host,
            imap_port,
            smtp_host,
            smtp_port,
            client_id,
            client_secret,
            redirect_uri,
            callback,
            bot,
            authorization: None,
            _inputs: inputs,
        }
    }

    pub(super) fn bot(&self, cx: &App) -> Option<Uuid> {
        self.bot.read(cx).selected_value().copied().flatten()
    }

    pub(super) fn clear_secrets(&mut self, window: &mut Window, cx: &mut App) {
        for input in [&self.password, &self.client_secret, &self.callback] {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
    }

    pub(super) fn authorized(
        &mut self,
        auth: EmailAuthorization,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.clear_secrets(window, cx);
        self.authorization = Some(Authorization::Manual(auth));
        self.callback
            .update(cx, |state, cx| state.focus(window, cx));
    }

    pub(super) fn google_authorized(&mut self, url: String) {
        self.authorization = Some(Authorization::Google(url));
    }

    pub(super) fn google_completing(&mut self) {
        self.authorization = Some(Authorization::CompletingGoogle);
    }

    pub(super) fn cancel_authorization(&mut self) {
        self.authorization = None;
    }

    pub(super) fn request(&self, presets: &[EmailPreset], cx: &App) -> Result<Request, String> {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        if let Some(Authorization::Manual(auth)) = &self.authorization {
            let callback = url::Url::parse(&value(&self.callback))
                .map_err(|_| "Paste the complete redirect URL from your browser.")?;
            let param = |name| {
                callback
                    .query_pairs()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.into_owned())
            };
            let state = param("state")
                .ok_or("The redirect URL is missing its state. Try signing in again.")?;
            let code = param("code")
                .ok_or("The redirect URL is missing its code. Check that you allowed access.")?;
            return Ok(Request::Complete {
                request_id: auth.request_id,
                state,
                code,
            });
        }
        if self.provider == EmailProvider::Gmail {
            return Ok(Request::Google);
        }
        let address = value(&self.address);
        if !address.contains('@') {
            return Err("Enter your email address first.".into());
        }
        if self.provider.oauth() {
            let client_id = value(&self.client_id);
            if client_id.is_empty() {
                return Err("Enter the client ID for your OAuth application.".into());
            }
            return Ok(Request::Authorize {
                provider: self.provider,
                address,
                client_id,
                client_secret: (!value(&self.client_secret).is_empty())
                    .then(|| value(&self.client_secret)),
                redirect_uri: value(&self.redirect_uri),
            });
        }
        let password = self.password.read(cx).value().to_string();
        if password.is_empty() {
            return Err("Enter an app password first.".into());
        }
        let config = if self.provider == EmailProvider::Imap {
            let port = |input: &Entity<InputState>| {
                value(input)
                    .parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or("Use a server port between 1 and 65535.")
            };
            let imap_port = port(&self.imap_port)?;
            let smtp_port = port(&self.smtp_port)?;
            if value(&self.imap_host).is_empty() || value(&self.smtp_host).is_empty() {
                return Err("Enter both the incoming and outgoing mail servers.".into());
            }
            EmailConfig {
                provider: self.provider,
                address: address.clone(),
                username: if value(&self.username).is_empty() {
                    address
                } else {
                    value(&self.username)
                },
                imap: EmailEndpoint {
                    host: value(&self.imap_host),
                    port: imap_port,
                    start_tls: imap_port == 143,
                },
                smtp: EmailEndpoint {
                    host: value(&self.smtp_host),
                    port: smtp_port,
                    start_tls: smtp_port == 587,
                },
                archive_folder: "Archive".into(),
                trash_folder: "Trash".into(),
                drafts_folder: "Drafts".into(),
                sent_folder: "Sent".into(),
            }
        } else {
            let mut config = presets
                .iter()
                .find(|p| p.provider == self.provider)
                .and_then(|p| p.preset.clone())
                .ok_or("This server has no preset for that provider.")?;
            config.address = address.clone();
            config.username = address;
            config
        };
        Ok(Request::Password(config, password))
    }

    pub(super) fn render(&self, busy: bool, cx: &mut Context<EmailSettings>) -> AnyElement {
        let theme = cx.theme().clone();
        let note = |text: &str| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text.to_owned())
        };
        let field = |label: &str, input: &Entity<InputState>| {
            v_flex()
                .gap_1()
                .flex_1()
                .min_w_0()
                .child(div().text_sm().font_medium().child(label.to_owned()))
                .child(
                    Input::new(input)
                        .focus_ring(false)
                        .aria_label(label.to_owned())
                        .disabled(busy),
                )
        };
        let mut form = v_flex()
            .gap_4()
            .w_full()
            .child(div().font_semibold().child("Add email account"));
        if matches!(
            &self.authorization,
            Some(Authorization::Google(_) | Authorization::CompletingGoogle)
        ) {
            let waiting = matches!(&self.authorization, Some(Authorization::Google(_)));
            form = form.child(note(if waiting {
                "Finish signing in with Google in your browser. This page will update automatically."
            } else { "Connecting your Gmail account…" }));
            if let Some(Authorization::Google(url)) = &self.authorization {
                let url = url.clone();
                form = form.child(
                    Button::new("email-open-google")
                        .focus_ring(false)
                        .cursor_pointer()
                        .label("Open Google sign-in")
                        .ghost()
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                );
            }
            return form
                .child(
                    Button::new("email-cancel-google")
                        .focus_ring(false)
                        .cursor_pointer()
                        .label("Cancel")
                        .ghost()
                        .disabled(!waiting)
                        .on_click(cx.listener(|view, _, _, cx| view.cancel_google(cx))),
                )
                .into_any_element();
        }
        if let Some(Authorization::Manual(auth)) = &self.authorization {
            let url = auth.authorization_url.clone();
            form = form.child(note("Finish signing in with your provider, then paste the full redirect URL here. The page may say it cannot connect to localhost; copy its address anyway."))
                .child(Button::new("email-open-signin").focus_ring(false).cursor_pointer().label("Open sign-in page").ghost().disabled(busy)
                    .on_click(move |_, _, cx| cx.open_url(&url)))
                .child(field("Redirect URL", &self.callback));
        } else {
            let mut providers = h_flex().gap_2().flex_wrap();
            for provider in [
                EmailProvider::Gmail,
                EmailProvider::Microsoft,
                EmailProvider::Yahoo,
                EmailProvider::Icloud,
                EmailProvider::Imap,
            ] {
                providers = providers.child(
                    Button::new(SharedString::from(format!("email-provider-{provider:?}")))
                        .focus_ring(false)
                        .cursor_pointer()
                        .small()
                        .label(provider.label())
                        .selected(self.provider == provider)
                        .disabled(busy)
                        .on_click(cx.listener(move |view, _, window, cx| {
                            if let Some(form) = &mut view.form {
                                form.clear_secrets(window, cx);
                                form.provider = provider;
                            }
                            view.error = None;
                            cx.notify();
                        })),
                );
            }
            form = form.child(providers);
            if self.provider != EmailProvider::Gmail {
                form = form.child(field("Email address", &self.address));
            }
            if self.provider == EmailProvider::Gmail {
                form = form.child(note(if super::google::configured() {
                    "Choose your Google account and allow Gmail access in your browser."
                } else {
                    "This build does not include Gmail sign-in. Use an official else.bot release."
                }));
            } else if self.provider.oauth() {
                form = form.child(note("Sign in securely with your provider. This server needs your OAuth app's client ID and registered redirect URI."))
                    .child(field("Client ID", &self.client_id))
                    .child(field("Client secret, if required", &self.client_secret))
                    .child(field("Redirect URI", &self.redirect_uri));
            } else {
                form = form.child(field("App password", &self.password))
                    .child(note("Use an app password from your email provider. Credentials stay encrypted on the server and never enter chat."));
                if self.provider == EmailProvider::Imap {
                    form = form.child(field("Username", &self.username))
                        .child(h_flex().gap_3().child(field("IMAP server", &self.imap_host)).child(field("IMAP port", &self.imap_port)))
                        .child(h_flex().gap_3().child(field("SMTP server", &self.smtp_host)).child(field("SMTP port", &self.smtp_port)))
                        .child(note("TLS is required. Ports 143 and 587 use STARTTLS; other ports use implicit TLS."));
                }
            }
            form = form.child(v_flex().gap_1()
                .child(div().text_sm().font_medium().child("Ask a bot to help, optional"))
                .child(Select::new(&self.bot).focus_ring(false).cursor_pointer().accessibility_label("Email bot").disabled(busy)))
                .child(note("The bot will ask what this inbox is for and whether you want a first triage. All your other bots can still use it when asked."));
        }
        form.child(
            h_flex()
                .gap_2()
                .justify_end()
                .children(self.authorization.as_ref().map(|_| {
                    Button::new("email-restart-signin")
                        .focus_ring(false)
                        .cursor_pointer()
                        .label("Start over")
                        .ghost()
                        .disabled(busy)
                        .on_click(cx.listener(|view, _, window, cx| {
                            if let Some(form) = &mut view.form {
                                form.authorization = None;
                                form.clear_secrets(window, cx);
                            }
                            view.error = None;
                            cx.notify();
                        }))
                }))
                .child(
                    Button::new("email-connect")
                        .focus_ring(false)
                        .cursor_pointer()
                        .primary()
                        .disabled(
                            busy || (self.provider == EmailProvider::Gmail
                                && !super::google::configured()),
                        )
                        .label(if busy {
                            "Connecting…"
                        } else if self.provider == EmailProvider::Gmail {
                            "Connect Gmail"
                        } else if self.provider.oauth() && self.authorization.is_none() {
                            "Sign in"
                        } else {
                            "Connect email"
                        })
                        .on_click(cx.listener(|view, _, window, cx| view.submit(window, cx))),
                ),
        )
        .into_any_element()
    }
}
