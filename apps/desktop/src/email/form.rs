use super::*;
use crate::api::{Bot, ClientError, EmailConfig, EmailEndpoint, EmailProvider};
use crate::controls::{Control as _, Scale, Tier, segment, segment_track, text_field};
use gpui_kit::component::IndexPath;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::select::{Select, SelectState};

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
    bot: Entity<SelectState<Vec<BotChoice>>>,
    authorization: Option<Authorization>,
    _inputs: Vec<Subscription>,
}

enum Authorization {
    Waiting(String),
    Completing,
}

pub(super) enum Request {
    Password(Box<EmailConfig>, String),
    Authorize(super::oauth::Provider, String),
}

pub(super) enum Connected {
    Account(Box<EmailAccount>),
    Authorization(super::oauth::SignIn),
}

impl Request {
    pub(super) async fn send(self, client: Arc<Client>) -> Result<Connected, ClientError> {
        match self {
            Self::Password(config, password) => client
                .connect_email(*config, password)
                .await
                .map(|account| Connected::Account(Box::new(account))),
            Self::Authorize(provider, address) => {
                super::oauth::SignIn::start(client, provider, address)
                    .await
                    .map(Connected::Authorization)
            }
        }
    }
}

impl Form {
    pub(super) fn has_unsaved_work(&self, cx: &App) -> bool {
        [
            &self.address,
            &self.password,
            &self.username,
            &self.imap_host,
            &self.smtp_host,
        ]
        .into_iter()
        .any(|field| !field.read(cx).value().trim().is_empty())
            || self.authorization.is_some()
    }
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
            &address, &password, &username, &imap_host, &imap_port, &smtp_host, &smtp_port,
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
            bot,
            authorization: None,
            _inputs: inputs,
        }
    }

    pub(super) fn bot(&self, cx: &App) -> Option<Uuid> {
        self.bot.read(cx).selected_value().copied().flatten()
    }

    pub(super) fn clear_secrets(&mut self, window: &mut Window, cx: &mut App) {
        self.password
            .update(cx, |state, cx| state.set_value("", window, cx));
    }

    pub(super) fn authorized(&mut self, url: String) {
        self.authorization = Some(Authorization::Waiting(url));
    }

    pub(super) fn completing(&mut self) {
        self.authorization = Some(Authorization::Completing);
    }

    pub(super) fn cancel_authorization(&mut self) {
        self.authorization = None;
    }

    pub(super) fn request(&self, presets: &[EmailPreset], cx: &App) -> Result<Request, String> {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let address = value(&self.address);
        if self.provider != EmailProvider::Gmail && !address.contains('@') {
            return Err("Enter your email address first.".into());
        }
        if let Some(provider) = super::oauth::Provider::for_email(self.provider) {
            return Ok(Request::Authorize(
                provider,
                if self.provider == EmailProvider::Gmail {
                    String::new()
                } else {
                    address
                },
            ));
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
        Ok(Request::Password(Box::new(config), password))
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
                    text_field(Input::new(input), cx)
                        .aria_label(label.to_owned())
                        .disabled(busy),
                )
        };
        let mut form = v_flex()
            .gap_4()
            .w_full()
            .child(div().text_base().font_semibold().child("Add email account"));
        if let Some(authorization) = &self.authorization {
            let waiting = matches!(authorization, Authorization::Waiting(_));
            form = form.child(note(&if waiting {
                format!("Finish signing in with {} in your browser. This page will update automatically.", self.provider.label())
            } else { format!("Connecting your {} account…", self.provider.label()) }));
            if let Authorization::Waiting(url) = authorization {
                let url = url.clone();
                form = form.child(
                    Button::new("email-open-signin")
                        .control(Tier::Ghost, Scale::Standard, false, cx)
                        .label("Open sign-in page")
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                );
            }
            return form
                .child(
                    Button::new("email-cancel-signin")
                        .control(Tier::Ghost, Scale::Standard, !waiting, cx)
                        .label("Cancel")
                        .on_click(cx.listener(|view, _, _, cx| view.cancel_signin(cx))),
                )
                .into_any_element();
        }
        let mut providers = segment_track(cx).w_full();
        for provider in [
            EmailProvider::Gmail,
            EmailProvider::Microsoft,
            EmailProvider::Yahoo,
            EmailProvider::Icloud,
            EmailProvider::Imap,
        ] {
            let chosen = self.provider == provider;
            providers = providers.child(
                segment(
                    SharedString::from(format!("email-provider-{provider:?}")),
                    provider.label(),
                    chosen,
                    busy,
                    cx,
                )
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
        form = form.child(div().max_w(px(520.)).child(providers));
        if self.provider != EmailProvider::Gmail {
            form = form.child(field("Email address", &self.address));
        }
        let oauth = super::oauth::Provider::for_email(self.provider);
        if let Some(provider) = oauth {
            form = form.child(note(&if provider.configured() {
                format!(
                    "Sign in with {} and allow mailbox access in your browser.",
                    provider.label()
                )
            } else {
                format!(
                    "This build does not include {} sign-in. Use an official else.bot release.",
                    provider.label()
                )
            }));
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
            .child(div().max_w(px(320.)).child(Select::new(&self.bot).focus_ring(false).cursor_pointer().accessibility_label("Email bot").disabled(busy))))
            .child(note("The bot will ask what this inbox is for and whether you want a first triage. All your other bots can still use it when asked."));
        form.child(
            h_flex().gap_2().justify_end().child(
                Button::new("email-connect")
                    .control(
                        Tier::Primary,
                        Scale::Standard,
                        busy || oauth.is_some_and(|provider| !provider.configured()),
                        cx,
                    )
                    .label(if busy {
                        "Connecting…".to_owned()
                    } else if let Some(provider) = oauth {
                        format!("Connect {}", provider.label())
                    } else {
                        "Connect email".into()
                    })
                    .on_click(cx.listener(|view, _, window, cx| view.submit(window, cx))),
            ),
        )
        .into_any_element()
    }
}
