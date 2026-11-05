mod view;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::*;
use uuid::Uuid;

use crate::api::socials::{Binding, BindingInput, Connection, Permissions, Provider, WakePolicy};
use crate::api::{Bot, Client};

const DEFAULT_INSTRUCTIONS: &str =
    "Summarize messages relevant to me. Draft useful replies and ask me before posting.";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Credential,
    Access,
    Connected,
}

struct Form {
    provider: Provider,
    connection: Option<Connection>,
    token: Entity<InputState>,
    instructions: Entity<TextareaState>,
    phase: Phase,
    replacing: bool,
    selected_bot: Option<Uuid>,
    destinations: Vec<crate::api::socials::Destination>,
    selected: HashSet<String>,
    permissions: Permissions,
    wake: WakePolicy,
    bindings: HashMap<Uuid, Binding>,
    confirm_disconnect: bool,
    _instructions: Subscription,
}

impl Form {
    fn new(
        provider: Provider,
        bot: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<SocialsView>,
    ) -> Self {
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
            connection: None,
            token: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("Paste the bot token")
            }),
            instructions,
            phase: Phase::Credential,
            replacing: false,
            selected_bot: bot,
            destinations: Vec::new(),
            selected: HashSet::new(),
            permissions: Permissions {
                read: true,
                ..Default::default()
            },
            wake: WakePolicy::Never,
            bindings: HashMap::new(),
            confirm_disconnect: false,
            _instructions: subscription,
        }
    }

    fn choose_bot(&mut self, bot: Uuid, window: &mut Window, cx: &mut App) {
        self.selected_bot = Some(bot);
        let binding = self.bindings.get(&bot);
        self.selected = binding
            .map(|b| b.access.destinations.iter().cloned().collect())
            .unwrap_or_default();
        self.permissions = binding
            .map(|b| b.access.permissions.clone())
            .unwrap_or(Permissions {
                read: true,
                ..Default::default()
            });
        self.wake = binding.map(|b| b.access.wake_policy).unwrap_or_default();
        let instructions = binding
            .map(|b| b.access.instructions.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_INSTRUCTIONS)
            .to_owned();
        self.instructions
            .update(cx, |input, cx| input.set_value(instructions, window, cx));
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
    receipt_only: bool,
    error: Option<String>,
    notice: Option<String>,
    revision: u64,
    _request: Option<Task<()>>,
}

impl SocialsView {
    pub(crate) fn new(
        client: Arc<Client>,
        bots: Vec<Bot>,
        setup: Option<(Uuid, Provider, Uuid)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let form = setup.map(|(_, provider, bot)| Form::new(provider, Some(bot), window, cx));
        let mut view = Self {
            client,
            bots,
            setup: setup.map(|(id, _, bot)| (id, bot)),
            connections: Vec::new(),
            form,
            busy: false,
            available: true,
            receipt_only: false,
            error: None,
            notice: None,
            revision: 0,
            _request: None,
        };
        view.refresh(window, cx);
        view
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
        let destinations = self
            .form
            .as_ref()
            .map(|f| f.destinations.clone())
            .unwrap_or_default();
        self.receipt_only = true;
        self.load_connection(connection, provider, None, window, cx);
        if let Some(form) = &mut self.form {
            form.destinations = destinations;
        }
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
        let bot = self
            .setup
            .map(|(_, bot)| bot)
            .or_else(|| self.bots.first().map(|b| b.id));
        self.form = Some(Form::new(provider, bot, window, cx));
        self.error = None;
        self.notice = None;
        cx.notify();
    }

    fn open(&mut self, connection: Connection, window: &mut Window, cx: &mut Context<Self>) {
        self.receipt_only = false;
        self.load_connection(
            connection.id,
            connection.provider,
            Some(connection),
            window,
            cx,
        );
    }

    fn load_connection(
        &mut self,
        id: Uuid,
        provider: Provider,
        connection: Option<Connection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let revision = self.begin(window, cx);
        let bot = self
            .setup
            .map(|(_, bot)| bot)
            .or_else(|| self.bots.first().map(|b| b.id));
        let mut form = Form::new(provider, bot, window, cx);
        form.connection = connection;
        form.phase = if self.receipt_only {
            Phase::Connected
        } else {
            Phase::Access
        };
        self.form = Some(form);
        let client = self.client.clone();
        let receipt_only = self.receipt_only;
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let (detail, destinations) = tokio::join!(
                client.clone().social_connection(id),
                async { if receipt_only { Ok(Vec::new()) } else { client.social_destinations(id).await } }
            );
            let _ = this.update_in(cx, |view, window, cx| {
                if view.revision != revision {
                    return;
                }
                view.busy = false;
                match (detail, destinations) {
                    (Ok(detail), Ok(destinations)) => {
                        if let Some(form) = &mut view.form {
                            form.connection = Some(detail.connection);
                            if !view.receipt_only { form.destinations = destinations; }
                            form.bindings =
                                detail.bindings.into_iter().map(|b| (b.bot_id, b)).collect();
                            if let Some(bot) = form.selected_bot {
                                form.choose_bot(bot, window, cx);
                                if view.receipt_only {
                                    for id in &form.selected {
                                        if !form.destinations.iter().any(|d| &d.id == id) {
                                            form.destinations.push(crate::api::socials::Destination { id: id.clone(), name: id.clone(), kind: "conversation".into(), can_send: form.permissions.send });
                                        }
                                    }
                                }
                                if view.receipt_only && !form.bindings.contains_key(&bot) {
                                    form.permissions = Permissions::default();
                                    form.wake = WakePolicy::Never;
                                    form.instructions.update(cx, |input, cx| input.set_value("No instructions are currently assigned to this assistant.", window, cx));
                                }
                            }
                        }
                    }
                    (Err(error), _) | (_, Err(error)) => view.error = Some(error.to_string()),
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
        let replacing = form
            .connection
            .as_ref()
            .filter(|_| form.replacing)
            .map(|c| c.id);
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
            .and_then(|f| f.connection.as_ref())
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
                match result {
                    Ok(destinations) => {
                        if let Some(form) = &mut view.form {
                            form.destinations = destinations;
                        }
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        let (Some(connection), Some(bot)) = (&form.connection, form.selected_bot) else {
            return;
        };
        if form.permissions.read && form.selected.is_empty() {
            self.error = Some("Choose at least one destination.".into());
            cx.notify();
            return;
        }
        let id = connection.id;
        let access = BindingInput {
            destinations: {
                let mut ids: Vec<_> = form.selected.iter().cloned().collect();
                ids.sort();
                ids
            },
            permissions: form.permissions.clone(),
            wake_policy: form.wake,
            instructions: form.instructions.read(cx).value().to_string(),
        };
        let setup = self.setup.map(|(id, _)| id);
        let revision = self.begin(window, cx);
        let client = self.client.clone();
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match client.clone().bind_social(id, bot, access).await {
                Ok(binding) => match setup {
                    Some(setup) => client.complete_social_setup(setup, id).await.map(|()| binding),
                    None => Ok(binding),
                },
                Err(error) => Err(error),
            };
            let _ = this.update_in(cx, |view, _, cx| {
                if view.revision != revision { return; }
                view.busy = false;
                match result {
                    Ok(binding) => {
                        view.notice = Some("Access saved. Your instructions guide the assistant within these permissions.".into());
                        if let Some(form) = &mut view.form { form.bindings.insert(binding.bot_id, binding); }
                        if view.setup.is_some()
                            && let Some(form) = &mut view.form
                        {
                            form.phase = Phase::Connected;
                        }
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn toggle_permission(&mut self, permission: &'static str, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        let p = &mut form.permissions;
        match permission {
            "read" => {
                p.read = !p.read;
                if !p.read {
                    p.write = false;
                    p.send = false;
                    p.wake = false;
                    form.wake = WakePolicy::Never;
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
        let Some(connection) = self.form.as_ref().and_then(|f| f.connection.clone()) else {
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
                            form.connection = Some(connection.clone());
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
            .and_then(|f| f.connection.as_ref())
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
