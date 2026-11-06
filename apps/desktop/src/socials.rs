#[cfg(test)]
mod tests;
mod view;

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::*;
use uuid::Uuid;

use crate::api::socials::{Binding, BindingInput, Connection, Permissions, Provider, WakePolicy};
use crate::api::{Bot, Client};

const DEFAULT_INSTRUCTIONS: &str =
    "Summarize messages relevant to me. Draft useful replies and ask me before posting.";

struct Access {
    connection: Connection,
    bindings: HashMap<Uuid, Binding>,
    destinations: Vec<crate::api::socials::Destination>,
    bot: Option<Uuid>,
    draft: BindingInput,
}

impl Access {
    fn from_detail(detail: crate::api::socials::ConnectionDetail, bot: Option<Uuid>) -> Self {
        let mut access = Self {
            connection: detail.connection,
            bindings: detail.bindings.into_iter().map(|b| (b.bot_id, b)).collect(),
            destinations: Vec::new(),
            bot: None,
            draft: Self::default_draft(),
        };
        if let Some(bot) = bot {
            access.choose_bot(bot);
        }
        access
    }

    fn default_draft() -> BindingInput {
        BindingInput {
            destinations: Vec::new(),
            permissions: Permissions {
                read: true,
                ..Default::default()
            },
            wake_policy: WakePolicy::Never,
            instructions: DEFAULT_INSTRUCTIONS.into(),
        }
    }

    fn choose_bot(&mut self, bot: Uuid) {
        self.bot = Some(bot);
        self.draft = self
            .bindings
            .get(&bot)
            .map(|b| b.access.clone())
            .unwrap_or_else(Self::default_draft);
    }

    fn discovered(
        &mut self,
        result: Result<Vec<crate::api::socials::Destination>, crate::api::ClientError>,
    ) -> Result<(), crate::api::ClientError> {
        self.destinations = result?;
        Ok(())
    }
}

enum Editor {
    Credential { replacement: Option<Connection> },
    Loading(Connection),
    Failed(Connection),
    Access(Access),
}

impl Editor {
    fn connection(&self) -> Option<&Connection> {
        match self {
            Self::Credential { replacement } => replacement.as_ref(),
            Self::Loading(connection) | Self::Failed(connection) => Some(connection),
            Self::Access(access) => Some(&access.connection),
        }
    }

    fn access(&self) -> Option<&Access> {
        match self {
            Self::Access(access) => Some(access),
            _ => None,
        }
    }

    fn access_mut(&mut self) -> Option<&mut Access> {
        match self {
            Self::Access(access) => Some(access),
            _ => None,
        }
    }
}

enum Receipt {
    Loading {
        connection: Uuid,
        provider: Provider,
    },
    Ready {
        connection: Connection,
        access: BindingInput,
        destinations: String,
    },
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
            Self::Ready { connection, .. } => (connection.id, connection.provider),
        }
    }
}

struct Form {
    provider: Provider,
    editor: Editor,
    token: Entity<InputState>,
    instructions: Entity<TextareaState>,
    confirm_disconnect: bool,
    _instructions: Subscription,
}

impl Form {
    fn new(provider: Provider, window: &mut Window, cx: &mut Context<SocialsView>) -> Self {
        let instructions = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 5)
                .default_value(DEFAULT_INSTRUCTIONS)
        });
        let subscription =
            cx.subscribe_in(&instructions, window, |_, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            });
        Self {
            provider,
            editor: Editor::Credential { replacement: None },
            token: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("Paste the bot token")
            }),
            instructions,
            confirm_disconnect: false,
            _instructions: subscription,
        }
    }

    fn choose_bot(&mut self, bot: Uuid, window: &mut Window, cx: &mut App) {
        let Some(access) = self.editor.access_mut() else {
            return;
        };
        access.choose_bot(bot);
        self.instructions.update(cx, |input, cx| {
            input.set_value(access.draft.instructions.clone(), window, cx)
        });
    }
}

pub(crate) struct SocialsView {
    client: Arc<Client>,
    bots: Vec<Bot>,
    setup: Option<(Uuid, Uuid)>,
    connections: Vec<Connection>,
    form: Option<Form>,
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
        bots: Vec<Bot>,
        setup: Option<(Uuid, Provider, Uuid)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::empty(client, bots, setup.map(|(id, _, bot)| (id, bot)));
        view.form = setup.map(|(_, provider, _)| Form::new(provider, window, cx));
        view.refresh(window, cx);
        view
    }

    pub(crate) fn new_receipt(
        client: Arc<Client>,
        bots: Vec<Bot>,
        setup: (Uuid, Provider, Uuid),
        connection: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::empty(client, bots, Some((setup.0, setup.2)));
        view.show_receipt(connection, setup.1, window, cx);
        view
    }

    fn empty(client: Arc<Client>, bots: Vec<Bot>, setup: Option<(Uuid, Uuid)>) -> Self {
        Self {
            client,
            bots,
            setup,
            connections: Vec::new(),
            form: None,
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
        let known = self
            .form
            .as_ref()
            .and_then(|form| form.editor.access())
            .map(|access| access.destinations.clone())
            .unwrap_or_default();
        self.form = None;
        self.receipt = Some(Receipt::Loading {
            connection,
            provider,
        });
        let client = self.client.clone();
        let bot = self.setup.map(|(_, bot)| bot);
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.social_connection(connection).await;
            let _ = this.update_in(cx, |view, _, cx| {
                if view.revision != revision {
                    return;
                }
                view.busy = false;
                view.receipt = Some(match result {
                    Ok(detail) => {
                        let access = detail
                            .bindings
                            .into_iter()
                            .find(|b| Some(b.bot_id) == bot)
                            .map(|b| b.access)
                            .unwrap_or(BindingInput {
                                destinations: Vec::new(),
                                permissions: Permissions::default(),
                                wake_policy: WakePolicy::Never,
                                instructions:
                                    "No instructions are currently assigned to this assistant."
                                        .into(),
                            });
                        let destinations = access
                            .destinations
                            .iter()
                            .map(|id| {
                                known
                                    .iter()
                                    .find(|destination| &destination.id == id)
                                    .map(|destination| destination.name.as_str())
                                    .unwrap_or(id)
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        Receipt::Ready {
                            connection: detail.connection,
                            access,
                            destinations,
                        }
                    }
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

    fn open(&mut self, connection: Connection, window: &mut Window, cx: &mut Context<Self>) {
        let revision = self.begin(window, cx);
        self.receipt = None;
        let id = connection.id;
        let bot = self
            .setup
            .map(|(_, bot)| bot)
            .or_else(|| self.bots.first().map(|b| b.id));
        let mut form = Form::new(connection.provider, window, cx);
        form.editor = Editor::Loading(connection);
        self.form = Some(form);
        let client = self.client.clone();
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let (detail, destinations) = tokio::join!(
                client.clone().social_connection(id),
                client.social_destinations(id),
            );
            let _ = this.update_in(cx, |view, window, cx| {
                if view.revision != revision {
                    return;
                }
                view.busy = false;
                if let Some(form) = &mut view.form {
                    match detail {
                        Ok(detail) => {
                            form.editor = Editor::Access(Access::from_detail(detail, bot));
                            if let Some(bot) = bot {
                                form.choose_bot(bot, window, cx);
                            }
                        }
                        Err(error) => {
                            if let Some(connection) = form.editor.connection().cloned() {
                                form.editor = Editor::Failed(connection);
                            }
                            view.error = Some(error.to_string());
                        }
                    }
                    if let Some(access) = form.editor.access_mut()
                        && let Err(error) = access.discovered(destinations)
                    {
                        view.error = Some(error.to_string());
                    }
                }
                cx.notify();
            });
        }));
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
        let replacing = match &form.editor {
            Editor::Credential { replacement } => replacement.as_ref().map(|c| c.id),
            _ => return,
        };
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
                        view.open(connection, window, cx);
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn refresh_destinations(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self
            .form
            .as_ref()
            .and_then(|f| f.editor.connection())
            .map(|c| c.id)
        else {
            return;
        };
        let revision = self.begin(window, cx);
        let client = self.client.clone();
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.social_destinations(id).await;
            let _ = this.update_in(cx, |view, _, cx| {
                if view.revision != revision {
                    return;
                }
                view.busy = false;
                if let Some(access) = view.form.as_mut().and_then(|form| form.editor.access_mut())
                    && let Err(error) = access.discovered(result)
                {
                    view.error = Some(error.to_string());
                }
                cx.notify();
            });
        }));
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        let Some(access) = form.editor.access() else {
            return;
        };
        let Some(bot) = access.bot else { return };
        let connection = &access.connection;
        if self.setup.is_some() && !access.draft.permissions.read {
            self.error = Some("Enable reading to complete this connection.".into());
            cx.notify();
            return;
        }
        if access.draft.permissions.read && access.draft.destinations.is_empty() {
            self.error = Some("Choose at least one destination.".into());
            cx.notify();
            return;
        }
        let id = connection.id;
        let mut access = access.draft.clone();
        access.destinations.sort();
        access.instructions = form.instructions.read(cx).value().to_string();
        let setup = self.setup.map(|(id, _)| id);
        let revision = self.begin(window, cx);
        let client = self.client.clone();
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match setup {
                Some(setup) => client.complete_social_setup(setup, id, access).await.map(|()| None),
                None => client.bind_social(id, bot, access).await.map(Some),
            };
            let _ = this.update_in(cx, |view, window, cx| {
                if view.revision != revision { return; }
                view.busy = false;
                match result {
                    Ok(binding) => {
                        view.notice = Some("Access saved. Your instructions guide the assistant within these permissions.".into());
                        if let (Some(form), Some(binding)) = (&mut view.form, binding)
                            && let Some(access) = form.editor.access_mut()
                        { access.bindings.insert(binding.bot_id, binding); }
                        if view.setup.is_some()
                            && let Some(connection) = view.form.as_ref().and_then(|f| f.editor.connection()).cloned()
                        {
                            view.show_receipt(connection.id, connection.provider, window, cx);
                        }
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn toggle_permission(&mut self, permission: &'static str, cx: &mut Context<Self>) {
        let Some(access) = self.form.as_mut().and_then(|form| form.editor.access_mut()) else {
            return;
        };
        let p = &mut access.draft.permissions;
        match permission {
            "read" => {
                p.read = !p.read;
                if !p.read {
                    p.write = false;
                    p.send = false;
                    p.wake = false;
                    access.draft.wake_policy = WakePolicy::Never;
                }
            }
            "write" => {
                p.write = !p.write;
                if p.write {
                    p.read = true;
                } else {
                    p.send = false;
                }
            }
            "send" => {
                p.send = !p.send;
                if p.send {
                    p.read = true;
                    p.write = true;
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self
            .form
            .as_ref()
            .and_then(|f| f.editor.connection().cloned())
        else {
            return;
        };
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
                        if let Some(form) = &mut view.form {
                            if let Some(access) = form.editor.access_mut() {
                                access.connection = connection.clone();
                            }
                        }
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

    fn disconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self
            .form
            .as_ref()
            .and_then(|f| f.editor.connection())
            .map(|c| c.id)
        else {
            return;
        };
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
                        view.form = None;
                        view.notice = Some(
                            "Disconnected. The assistant no longer has access to this account."
                                .into(),
                        );
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear(window, cx);
        let Some((id, _)) = self.setup else {
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
