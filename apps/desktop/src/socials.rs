#[cfg(test)]
mod tests;
mod view;

use else_wire::socials::ConnectionState;
use else_wire::socials::Health;
use std::sync::Arc;

use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::*;
use uuid::Uuid;

use crate::api::Client;
use crate::api::socials::PairingTarget;
use else_wire::socials::{Connection, Pairing, PairingView, Provider};

enum Receipt {
    Loading {
        connection: Uuid,
        provider: Provider,
    },
    Ready(Box<Connection>),
    Unavailable {
        connection: Uuid,
        provider: Provider,
    },
}

impl Receipt {
    fn identity(&self) -> (Uuid, Provider) {
        match self {
            Self::Loading {
                connection,
                provider,
            }
            | Self::Unavailable {
                connection,
                provider,
            } => (*connection, *provider),
            Self::Ready(connection) => (connection.id, connection.provider),
        }
    }
}

struct Form {
    provider: Provider,
    replacement: Option<Connection>,
    entry: Entry,
}

/// How the owner proves the account: a pasted bot token, or a device
/// paired by scanning a code.
enum Entry {
    Token(Entity<InputState>),
    Pairing(Box<PairingCard>),
}

struct PairingCard {
    target: PairingTarget,
    view: Option<PairingView>,
    /// The code shown, rendered as a QR image.
    qr: Option<(String, Arc<Image>)>,
    /// The owner's words for the bot, returned with the result.
    note: Entity<TextareaState>,
    _poll: Task<()>,
}

impl PairingCard {
    fn note(&self, cx: &App) -> Option<String> {
        let note = self.note.read(cx).value().trim().to_string();
        (!note.is_empty()).then_some(note)
    }
}

impl Form {
    fn token(provider: Provider, window: &mut Window, cx: &mut Context<SocialsView>) -> Self {
        Self {
            provider,
            replacement: None,
            entry: Entry::Token(cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("Paste the bot token")
            })),
        }
    }

    fn pairing(
        provider: Provider,
        target: PairingTarget,
        replacement: Option<Connection>,
        window: &mut Window,
        cx: &mut Context<SocialsView>,
    ) -> Self {
        Self {
            provider,
            replacement,
            entry: Entry::Pairing(Box::new(PairingCard {
                target,
                view: None,
                qr: None,
                note: cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .auto_grow(2, 6)
                        .placeholder("What should the bot do once connected? (optional)")
                }),
                _poll: SocialsView::poll_pairing(target, window, cx),
            })),
        }
    }

    fn pairing_card(&mut self) -> Option<&mut PairingCard> {
        match &mut self.entry {
            Entry::Pairing(card) => Some(card),
            Entry::Token(_) => None,
        }
    }
}

/// `code` as a scannable QR image, dark on light whatever the theme.
fn qr_image(code: &str) -> Option<Arc<Image>> {
    let svg = qrcode::QrCode::new(code.as_bytes())
        .ok()?
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .build();
    Some(Arc::new(Image::from_bytes(
        ImageFormat::Svg,
        svg.into_bytes(),
    )))
}

pub(crate) struct SocialsView {
    client: Arc<Client>,
    setup: Option<Uuid>,
    connections: Vec<Connection>,
    form: Option<Form>,
    confirm_disconnect: Option<Uuid>,
    busy: bool,
    available: bool,
    receipt: Option<Receipt>,
    error: Option<String>,
    notice: Option<String>,
    revision: u64,
    _request: Option<Task<()>>,
}

impl SocialsView {
    pub(crate) fn has_unsaved_work(&self) -> bool {
        self.busy || self.form.is_some()
    }

    pub(crate) fn new(
        client: Arc<Client>,
        setup: Option<(Uuid, Provider)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::empty(client, setup.map(|(id, _)| id));
        view.refresh(window, cx);
        view.form = setup.map(|(id, provider)| {
            if provider.pairs() {
                Form::pairing(provider, PairingTarget::Setup(id), None, window, cx)
            } else {
                Form::token(provider, window, cx)
            }
        });
        view
    }

    pub(crate) fn new_receipt(
        client: Arc<Client>,
        setup: (Uuid, Provider),
        connection: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::empty(client, Some(setup.0));
        view.show_receipt(connection, setup.1, window, cx);
        view
    }

    fn empty(client: Arc<Client>, setup: Option<Uuid>) -> Self {
        Self {
            client,
            setup,
            connections: Vec::new(),
            form: None,
            confirm_disconnect: None,
            busy: false,
            available: true,
            receipt: None,
            error: None,
            notice: None,
            revision: 0,
            _request: None,
        }
    }

    pub(crate) fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.revision += 1;
        self._request = None;
        self.busy = false;
        if let Some(Form {
            entry: Entry::Token(token),
            ..
        }) = &self.form
        {
            token.update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.notify();
    }

    pub(crate) fn show_receipt(
        &mut self,
        connection: Uuid,
        provider: Provider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let revision = self.begin(window, cx);
        self.form = None;
        self.receipt = Some(Receipt::Loading {
            connection,
            provider,
        });
        let client = self.client.clone();
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.social_connection(connection).await;
            let _ = this.update_in(cx, |view, _, cx| {
                if view.revision != revision {
                    return;
                }
                view.busy = false;
                view.receipt = Some(match result {
                    Ok(detail) => Receipt::Ready(Box::new(detail)),
                    Err(error) => {
                        view.error = Some(error.to_string());
                        Receipt::Unavailable {
                            connection,
                            provider,
                        }
                    }
                });
                cx.notify();
            });
        }));
    }

    pub(crate) fn set_available(
        &mut self,
        available: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.available == available {
            return;
        }
        self.available = available;
        if !available {
            self.clear(window, cx);
        } else if let Some(receipt) = &self.receipt {
            let (connection, provider) = receipt.identity();
            self.show_receipt(connection, provider, window, cx);
        } else {
            self.refresh(window, cx);
        }
        cx.notify();
    }

    fn begin(&mut self, window: &mut Window, cx: &mut Context<Self>) -> u64 {
        self.clear(window, cx);
        self.busy = true;
        self.error = None;
        self.notice = None;
        self.revision
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let revision = self.begin(window, cx);
        let client = self.client.clone();
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.social_connections().await;
            let _ = this.update_in(cx, |view, _, cx| {
                if view.revision != revision {
                    return;
                }
                view.busy = false;
                match result {
                    Ok(connections) => view.connections = connections,
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn add(&mut self, provider: Provider, window: &mut Window, cx: &mut Context<Self>) {
        let form = Form::token(provider, window, cx);
        self.open(form, window, cx);
    }

    fn open(&mut self, form: Form, window: &mut Window, cx: &mut Context<Self>) {
        self.clear(window, cx);
        self.form = Some(form);
        self.error = None;
        self.notice = None;
        cx.notify();
    }

    /// Replace a connection's token, or pair its device again.
    fn replace(&mut self, connection: Connection, window: &mut Window, cx: &mut Context<Self>) {
        if !connection.provider.pairs() {
            self.add(connection.provider, window, cx);
            if let Some(form) = &mut self.form {
                form.replacement = Some(connection);
            }
            return;
        }
        let target = PairingTarget::Connection(connection.id);
        let form = Form::pairing(connection.provider, target, Some(connection), window, cx);
        self.open(form, window, cx);
    }

    /// Follow a pairing until it finishes, showing each code as it rotates.
    fn poll_pairing(
        target: PairingTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn_in(window, async move |this, cx| {
            let Ok(client) = this.read_with(cx, |view, _| view.client.clone()) else {
                return;
            };
            let mut next = client.clone().start_social_pairing(target).await;
            loop {
                let shown = match next {
                    Ok(view) => {
                        let finished = view.pairing.finished();
                        let revision = view.revision;
                        let alive = this
                            .update_in(cx, |socials, _, cx| socials.show_pairing(target, view, cx))
                            .is_ok();
                        (!finished && alive).then_some(revision)
                    }
                    Err(error) => {
                        let alive = this
                            .update(cx, |socials, cx| {
                                socials.error = Some(error.to_string());
                                cx.notify();
                            })
                            .is_ok();
                        if !alive {
                            return;
                        }
                        cx.background_executor()
                            .timer(std::time::Duration::from_secs(3))
                            .await;
                        next = client.clone().start_social_pairing(target).await;
                        continue;
                    }
                };
                let Some(after) = shown else { return };
                next = client.clone().social_pairing(target, after).await;
            }
        })
    }

    fn show_pairing(&mut self, target: PairingTarget, view: PairingView, cx: &mut Context<Self>) {
        let Some(card) = self.form.as_mut().and_then(Form::pairing_card) else {
            return;
        };
        if card.target != target {
            return;
        }
        self.error = None;
        if let Pairing::Code { code, .. } = &view.pairing
            && card.qr.as_ref().is_none_or(|(shown, _)| shown != code)
        {
            card.qr = qr_image(code).map(|image| (code.clone(), image));
        }
        let paired = match &view.pairing {
            Pairing::Paired { connection } => Some(connection.as_ref().clone()),
            _ => None,
        };
        card.view = Some(view);
        if let Some(connection) = paired {
            self.connections.retain(|c| c.id != connection.id);
            self.connections.push(connection.clone());
            if let PairingTarget::Connection(_) = target {
                self.form = None;
                self.notice = Some(format!(
                    "{} paired again. Bots can use it as before.",
                    connection.provider.label()
                ));
            }
        }
        cx.notify();
    }

    /// Ask for a new code after one expired or failed.
    fn restart_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(card) = self.form.as_mut().and_then(Form::pairing_card) else {
            return;
        };
        card.view = None;
        card.qr = None;
        card._poll = Self::poll_pairing(card.target, window, cx);
        self.error = None;
        cx.notify();
    }

    /// Finish a bot's setup with the paired connection and the owner's words.
    fn finish_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(card) = self.form.as_mut().and_then(Form::pairing_card) else {
            return;
        };
        let Some(Pairing::Paired { connection }) = card.view.as_ref().map(|v| &v.pairing) else {
            return;
        };
        let connection = connection.as_ref().clone();
        let note = card.note(cx);
        self.complete(connection, note, window, cx);
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(
            form @ Form {
                entry: Entry::Token(token),
                ..
            },
        ) = &self.form
        else {
            return;
        };
        let token = token.read(cx).value().trim().to_string();
        if token.is_empty() {
            self.error = Some("Paste the bot token first.".into());
            cx.notify();
            return;
        }
        let provider = form.provider;
        let replacing = form.replacement.as_ref().map(|c| c.id);
        let revision = self.begin(window, cx);
        let client = self.client.clone();
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = if let Some(id) = replacing {
                match client.clone().replace_social_token(id, token).await {
                    Ok(()) => client.social_connection(id).await,
                    Err(error) => Err(error),
                }
            } else {
                client.connect_social(provider, token).await
            };
            let _ = this.update_in(cx, |view, window, cx| {
                if view.revision != revision {
                    return;
                }
                view.busy = false;
                match result {
                    Ok(connection) => {
                        view.connections.retain(|c| c.id != connection.id);
                        view.connections.push(connection.clone());
                        if view.setup.is_some() {
                            view.complete(connection, None, window, cx);
                        } else {
                            view.form = None;
                            view.notice = Some(if replacing.is_some() {
                                "Token replaced.".into()
                            } else {
                                "Connected. Any bot can use this connection when you ask.".into()
                            });
                        }
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn complete(
        &mut self,
        connection: Connection,
        note: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(setup) = self.setup else {
            return;
        };
        let revision = self.begin(window, cx);
        let client = self.client.clone();
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client
                .complete_social_setup(setup, connection.id, note)
                .await;
            let _ = this.update_in(cx, |view, window, cx| {
                if view.revision != revision {
                    return;
                }
                view.busy = false;
                match result {
                    Ok(()) => view.show_receipt(connection.id, connection.provider, window, cx),
                    Err(error) => {
                        view.error = Some(format!(
                            "{error} The bot is connected. Choose it under \"Or use a bot you already connected\" to finish."
                        ))
                    }
                }
                cx.notify();
            });
        }));
    }

    fn active(&mut self, connection: Connection, window: &mut Window, cx: &mut Context<Self>) {
        let active = connection.state != ConnectionState::Active;
        let revision = self.begin(window, cx);
        let client = self.client.clone();
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.set_social_active(connection.id, active).await;
            let _ = this.update_in(cx, |view, _, cx| {
                if view.revision != revision {
                    return;
                }
                view.busy = false;
                match result {
                    Ok(connection) => {
                        if let Some(existing) =
                            view.connections.iter_mut().find(|c| c.id == connection.id)
                        {
                            *existing = connection;
                        }
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn disconnect(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let revision = self.begin(window, cx);
        let client = self.client.clone();
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.disconnect_social(id).await;
            let _ = this.update_in(cx, |view, _, cx| {
                if view.revision != revision {
                    return;
                }
                view.busy = false;
                match result {
                    Ok(()) => {
                        view.connections.retain(|c| c.id != id);
                        view.confirm_disconnect = None;
                        if view
                            .form
                            .as_ref()
                            .and_then(|f| f.replacement.as_ref())
                            .is_some_and(|c| c.id == id)
                        {
                            view.form = None;
                        }
                        view.notice =
                            Some("Disconnected. Bots can no longer use this connection.".into());
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear(window, cx);
        let Some(id) = self.setup else {
            if let Some(Form {
                replacement: Some(connection),
                entry: Entry::Pairing(_),
                ..
            }) = self.form.take()
            {
                let client = self.client.clone();
                cx.background_spawn(async move {
                    let _ = client.stop_social_pairing(connection.id).await;
                })
                .detach();
            }
            cx.notify();
            return;
        };
        let revision = self.begin(window, cx);
        let client = self.client.clone();
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.cancel_social_setup(id).await;
            let _ = this.update_in(cx, |view, _, cx| {
                if view.revision != revision {
                    return;
                }
                view.busy = false;
                match result {
                    Ok(()) => {
                        view.form = None;
                        view.notice = Some("Connection request cancelled.".into());
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }
}
