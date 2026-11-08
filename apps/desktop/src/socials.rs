#[cfg(test)]
mod tests;
mod view;

use std::sync::Arc;

use gpui_kit::component::input::InputState;
use gpui_kit::*;
use uuid::Uuid;

use crate::api::Client;
use crate::api::socials::{Connection, Provider};

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
    token: Entity<InputState>,
}

impl Form {
    fn new(provider: Provider, window: &mut Window, cx: &mut Context<SocialsView>) -> Self {
        Self {
            provider,
            replacement: None,
            token: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("Paste the bot token")
            }),
        }
    }
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
        view.form = setup.map(|(_, provider)| Form::new(provider, window, cx));
        view.refresh(window, cx);
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
        if let Some(form) = &self.form {
            form.token
                .update(cx, |input, cx| input.set_value("", window, cx));
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
                    Ok(detail) => Receipt::Ready(Box::new(detail.connection)),
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
        self.clear(window, cx);
        self.form = Some(Form::new(provider, window, cx));
        self.error = None;
        self.notice = None;
        cx.notify();
    }

    fn replace(&mut self, connection: Connection, window: &mut Window, cx: &mut Context<Self>) {
        self.add(connection.provider, window, cx);
        if let Some(form) = &mut self.form {
            form.replacement = Some(connection);
        }
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        let token = form.token.read(cx).value().trim().to_string();
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
                    Ok(()) => client.social_connection(id).await.map(|d| d.connection),
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
                            view.complete(connection, window, cx);
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

    fn complete(&mut self, connection: Connection, window: &mut Window, cx: &mut Context<Self>) {
        let Some(setup) = self.setup else {
            return;
        };
        let revision = self.begin(window, cx);
        let client = self.client.clone();
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.complete_social_setup(setup, connection.id).await;
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
        let active = connection.state != "active";
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
            self.form = None;
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
