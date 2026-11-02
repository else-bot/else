//! The messaging window: bot sidebar, selected conversation, and composer.
//!
//! All network work runs on the API client's runtime; GPUI tasks only await
//! results and apply them to this view. The conversation model accepts only
//! contiguous committed events, and any inconsistency reloads a snapshot.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::*;
use uuid::Uuid;

use crate::api::{Bot, BotList, Capabilities, Client, ClientError, Sent, StreamMessage};
use crate::model::{Applied, Conversation, Snapshot};

actions!(
    else_desktop,
    [
        SelectNext,
        SelectPrev,
        FocusComposer,
        FocusSidebar,
        NewBot,
        StopWork,
        DeleteBot,
        NextProvider,
        LoadOlder
    ]
);

const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Retrying a rejected token cannot succeed; poll rarely until it changes.
const UNAUTHORIZED_POLL_INTERVAL: Duration = Duration::from_secs(30);
const SIDEBAR_CONTEXT: &str = "Sidebar";
const COMPOSER_CONTEXT: &str = "Composer";
const APP_CONTEXT: &str = "ElseApp";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, Some(SIDEBAR_CONTEXT)),
        KeyBinding::new("up", SelectPrev, Some(SIDEBAR_CONTEXT)),
        KeyBinding::new("ctrl-n", SelectNext, Some(SIDEBAR_CONTEXT)),
        KeyBinding::new("ctrl-p", SelectPrev, Some(SIDEBAR_CONTEXT)),
        KeyBinding::new("enter", FocusComposer, Some(SIDEBAR_CONTEXT)),
        KeyBinding::new("escape", FocusSidebar, Some(COMPOSER_CONTEXT)),
        KeyBinding::new("ctrl-shift-n", NewBot, Some(APP_CONTEXT)),
        KeyBinding::new("ctrl-shift-x", StopWork, Some(APP_CONTEXT)),
        KeyBinding::new("ctrl-shift-d", DeleteBot, Some(APP_CONTEXT)),
        KeyBinding::new("ctrl-shift-p", NextProvider, Some(APP_CONTEXT)),
        KeyBinding::new("ctrl-shift-up", LoadOlder, Some(APP_CONTEXT)),
    ]);
}

/// What the window knows about its API connection.
#[derive(Debug, Clone, PartialEq)]
pub enum Connection {
    Connecting,
    Connected,
    /// Shown with any history already loaded; polling keeps retrying.
    Disconnected(String),
    /// Retrying will not help until the token is fixed.
    InvalidCredentials(String),
    /// Configuration could not be loaded at all.
    Misconfigured(String),
}

/// An owner message not yet confirmed by the server.
#[derive(Debug, Clone)]
struct PendingSend {
    request_id: Uuid,
    text: String,
    provider_id: Option<String>,
    /// `None` while sending; otherwise why the outcome is unknown.
    unknown: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RowKey {
    Older,
    Item(Uuid),
    Pending(Uuid),
}

/// The selected bot's transcript and its live subscription.
struct ConversationView {
    bot_id: Uuid,
    conversation: Option<Conversation>,
    stream_problem: Option<String>,
    loading_older: bool,
    rows: Vec<RowKey>,
    list: ListState,
    texts: HashMap<RowKey, (Entity<TextareaState>, String)>,
    _load: Option<Task<()>>,
    _older: Option<Task<()>>,
}

impl ConversationView {
    fn new(bot_id: Uuid) -> Self {
        let list = ListState::new(0, ListAlignment::Bottom, px(400.));
        list.set_follow_mode(FollowMode::Tail);
        Self {
            bot_id,
            conversation: None,
            stream_problem: None,
            loading_older: false,
            rows: Vec::new(),
            list,
            texts: HashMap::new(),
            _load: None,
            _older: None,
        }
    }
}

struct CreateForm {
    request_id: Uuid,
    name: Entity<InputState>,
    role: Entity<TextareaState>,
    error: Option<String>,
    busy: bool,
}

pub struct ElseApp {
    client: Option<Arc<Client>>,
    connection: Connection,
    caps: Option<Capabilities>,
    bots: Vec<Bot>,
    default_bot_id: Option<Uuid>,
    selected: Option<Uuid>,
    view: Option<ConversationView>,
    composer: Entity<TextareaState>,
    drafts: HashMap<Uuid, String>,
    pending: HashMap<Uuid, Vec<PendingSend>>,
    provider_choice: HashMap<Uuid, String>,
    stopping: HashSet<Uuid>,
    notice: Option<String>,
    create: Option<Entity<CreateFormHolder>>,
    _create_enter: Option<Subscription>,
    sidebar_focus: FocusHandle,
    _poll: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// Holds the create-bot form so the dialog builder can re-render it.
pub struct CreateFormHolder {
    form: CreateForm,
}

impl ElseApp {
    pub fn new(
        client: Result<Arc<Client>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sidebar_focus = cx.focus_handle();
        sidebar_focus.focus(window, cx);
        let (client, connection) = match client {
            Ok(client) => (Some(client), Connection::Connecting),
            Err(message) => (None, Connection::Misconfigured(message)),
        };
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Message… (Enter to send, Shift+Enter for a new line)")
                .auto_grow(1, 8)
                .submit_on_enter(true)
        });
        let subscription = cx.subscribe_in(
            &composer,
            window,
            |app, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    app.send(window, cx);
                }
            },
        );
        let mut app = Self {
            client,
            connection,
            caps: None,
            bots: Vec::new(),
            default_bot_id: None,
            selected: None,
            view: None,
            composer,
            drafts: HashMap::new(),
            pending: HashMap::new(),
            provider_choice: HashMap::new(),
            stopping: HashSet::new(),
            notice: None,
            create: None,
            _create_enter: None,
            sidebar_focus,
            _poll: None,
            _subscriptions: vec![subscription],
        };
        app.start_polling(window, cx);
        app
    }

    // ----- Bot list ---------------------------------------------------------

    /// Refresh capabilities and bot summaries now and every few seconds.
    fn start_polling(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self._poll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let caps = client.clone().capabilities().await;
                let bots = match &caps {
                    Ok(_) => Some(client.clone().bots().await),
                    Err(_) => None,
                };
                let Ok(interval) = this.update_in(cx, |app, window, cx| {
                    app.apply_refresh(caps, bots, window, cx);
                    match app.connection {
                        Connection::InvalidCredentials(_) => UNAUTHORIZED_POLL_INTERVAL,
                        _ => POLL_INTERVAL,
                    }
                }) else {
                    return;
                };
                cx.background_executor().timer(interval).await;
            }
        }));
    }

    fn refresh_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_polling(window, cx);
    }

    fn apply_refresh(
        &mut self,
        caps: Result<Capabilities, ClientError>,
        bots: Option<Result<BotList, ClientError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = caps.and_then(|caps| {
            bots.expect("bots fetched with capabilities")
                .map(|b| (caps, b))
        });
        match result {
            Ok((caps, list)) => {
                let was_connected = self.connection == Connection::Connected;
                self.caps = Some(caps);
                self.default_bot_id = list.default_bot_id;
                self.bots = list.bots;
                self.connection = Connection::Connected;
                self.stopping.retain(|id| {
                    self.bots.iter().any(|b| {
                        b.id == *id && b.activity.as_ref().is_some_and(|a| a.phase != "idle")
                    })
                });
                self.reconcile_selection(window, cx);
                if !was_connected {
                    // Resolve sends whose outcome became unknown while offline.
                    self.resolve_unknown_sends(window, cx);
                }
            }
            Err(ClientError::Unauthorized) => {
                self.connection =
                    Connection::InvalidCredentials(ClientError::Unauthorized.to_string());
            }
            // Keep whatever was loaded; show the problem and keep retrying.
            Err(err) => self.connection = Connection::Disconnected(err.to_string()),
        }
        cx.notify();
    }

    fn active_bots(&self) -> impl Iterator<Item = &Bot> {
        self.bots.iter().filter(|bot| bot.is_active())
    }

    /// Prefer the current selection, then the saved default, then the oldest
    /// active bot. Navigation only: this never changes the saved default.
    fn reconcile_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = |id: Option<Uuid>| id.filter(|id| self.active_bots().any(|b| b.id == *id));
        let next = active(self.selected)
            .or(active(self.default_bot_id))
            .or_else(|| self.active_bots().next().map(|b| b.id));
        if next != self.selected || (next.is_some() && self.view.is_none()) {
            self.select(next, window, cx);
        }
    }

    fn select(&mut self, id: Option<Uuid>, window: &mut Window, cx: &mut Context<Self>) {
        if id == self.selected && self.view.is_some() {
            return;
        }
        // Keep the draft of the bot being left.
        if let Some(previous) = self.selected {
            let draft = self.composer.read(cx).value().to_string();
            self.drafts.insert(previous, draft);
        }
        self.selected = id;
        let draft = id
            .and_then(|id| self.drafts.get(&id).cloned())
            .unwrap_or_default();
        self.composer
            .update(cx, |state, cx| state.set_value(draft, window, cx));
        self.notice = None;
        // Dropping the old view drops its subscription and fetch tasks; the
        // bot's server work is unaffected.
        self.view = id.map(ConversationView::new);
        if id.is_some() {
            self.load_conversation(window, cx);
        }
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<Uuid> = self.active_bots().map(|b| b.id).collect();
        if ids.is_empty() {
            return;
        }
        let current = self
            .selected
            .and_then(|id| ids.iter().position(|b| *b == id))
            .unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, ids.len() as isize - 1) as usize;
        self.select(Some(ids[next]), window, cx);
    }

    fn selected_bot(&self) -> Option<&Bot> {
        self.selected
            .and_then(|id| self.bots.iter().find(|b| b.id == id))
    }

    // ----- Conversation and stream -----------------------------------------

    /// Load a snapshot, then follow the stream from its cursor. Any reset
    /// repeats this; reconnects resume from the last applied cursor.
    fn load_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(view)) = (self.client.clone(), self.view.as_mut()) else {
            return;
        };
        let bot = view.bot_id;
        view._load = Some(cx.spawn_in(window, async move |this, cx| {
            let snapshot = client.clone().snapshot(bot, None).await;
            let cursor = this
                .update_in(cx, |app, window, cx| {
                    app.apply_snapshot(bot, snapshot, window, cx)
                })
                .ok()
                .flatten();
            let Some(cursor) = cursor else { return };
            let mut messages = client.clone().subscribe(bot, cursor);
            while let Some(message) = messages.recv().await {
                let keep = this
                    .update_in(cx, |app, window, cx| {
                        app.apply_stream(bot, message, window, cx)
                    })
                    .unwrap_or(false);
                if !keep {
                    return;
                }
            }
        }));
    }

    /// Returns the cursor to subscribe from.
    fn apply_snapshot(
        &mut self,
        bot: Uuid,
        snapshot: Result<Snapshot, ClientError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let view = self.view.as_mut().filter(|v| v.bot_id == bot)?;
        match snapshot.map(Conversation::from_snapshot) {
            Ok(Ok(conversation)) => {
                let cursor = conversation.cursor();
                view.conversation = Some(conversation);
                view.stream_problem = None;
                self.sync_rows(window, cx);
                Some(cursor)
            }
            Ok(Err(_)) => {
                view.stream_problem = Some("The server sent an invalid snapshot.".into());
                cx.notify();
                None
            }
            Err(ClientError::Api { status: 410, .. }) => {
                self.refresh_now(window, cx);
                None
            }
            Err(err) => {
                view.stream_problem = Some(format!("Could not load history: {err}"));
                cx.notify();
                // Retry shortly; the poll loop also keeps the list current.
                let retry = cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    let _ = this.update_in(cx, |app, window, cx| {
                        if app.view.as_ref().is_some_and(|v| v.bot_id == bot) {
                            app.load_conversation(window, cx);
                        }
                    });
                });
                retry.detach();
                None
            }
        }
    }

    /// Returns false to end the subscription loop.
    fn apply_stream(
        &mut self,
        bot: Uuid,
        message: StreamMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(view) = self.view.as_mut().filter(|v| v.bot_id == bot) else {
            return false;
        };
        match message {
            StreamMessage::Connected => {
                view.stream_problem = None;
                cx.notify();
                true
            }
            StreamMessage::Frame(frame) => {
                let Some(conversation) = view.conversation.as_mut() else {
                    return false;
                };
                match conversation.apply(&frame) {
                    Ok(Applied::Duplicate) => true,
                    Ok(Applied::Changed) => {
                        // The stream's activity is fresher than the last poll.
                        let activity = conversation.activity.clone();
                        if activity.as_ref().is_some_and(|a| a.phase == "idle") {
                            self.stopping.remove(&bot);
                        }
                        if let Some(entry) = self.bots.iter_mut().find(|b| b.id == bot) {
                            if activity.is_some() {
                                entry.activity = activity;
                            }
                        }
                        self.sync_rows(window, cx);
                        true
                    }
                    Err(_) => {
                        // Discard this subscription and reload a snapshot.
                        self.load_conversation(window, cx);
                        false
                    }
                }
            }
            StreamMessage::Disconnected(reason) => {
                view.stream_problem = Some(format!("Reconnecting… {reason}"));
                cx.notify();
                true
            }
            StreamMessage::Reset => {
                self.load_conversation(window, cx);
                false
            }
            StreamMessage::Deleted => {
                self.refresh_now(window, cx);
                false
            }
            StreamMessage::Unauthorized => {
                self.connection =
                    Connection::InvalidCredentials(ClientError::Unauthorized.to_string());
                cx.notify();
                false
            }
        }
    }

    fn load_older(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let Some(before) = view.conversation.as_ref().and_then(|c| c.older_before_seq) else {
            return;
        };
        if view.loading_older {
            return;
        }
        view.loading_older = true;
        let bot = view.bot_id;
        view._older = Some(cx.spawn_in(window, async move |this, cx| {
            let page = client.snapshot(bot, Some(before)).await;
            let _ = this.update_in(cx, |app, window, cx| {
                let Some(view) = app.view.as_mut().filter(|v| v.bot_id == bot) else {
                    return;
                };
                view.loading_older = false;
                match page {
                    Ok(page) => {
                        if let Some(conversation) = view.conversation.as_mut() {
                            conversation.merge_older(page);
                        }
                        app.sync_rows(window, cx);
                    }
                    Err(err) => {
                        app.notice = Some(format!("Could not load older messages: {err}"));
                        cx.notify();
                    }
                }
            });
        }));
        cx.notify();
    }

    /// Rebuild the row list from the model and pending sends, splicing only
    /// what changed so scroll position and measurements are kept.
    fn sync_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let bot = view.bot_id;
        let Some(conversation) = view.conversation.as_ref() else {
            return;
        };
        // A pending send disappears once its canonical item is present.
        if let Some(pending) = self.pending.get_mut(&bot) {
            pending.retain(|p| !conversation.has_request(p.request_id));
        }
        let mut rows = Vec::new();
        if conversation.older_before_seq.is_some() {
            rows.push(RowKey::Older);
        }
        let mut texts: Vec<(RowKey, String)> = Vec::new();
        for item in conversation.items() {
            rows.push(RowKey::Item(item.id));
            texts.push((RowKey::Item(item.id), item.text.clone()));
        }
        for pending in self.pending.get(&bot).into_iter().flatten() {
            rows.push(RowKey::Pending(pending.request_id));
            texts.push((RowKey::Pending(pending.request_id), pending.text.clone()));
        }
        let old = std::mem::take(&mut view.rows);
        let prefix = old.iter().zip(&rows).take_while(|(a, b)| a == b).count();
        let max_suffix = old.len().min(rows.len()) - prefix;
        let suffix = old
            .iter()
            .rev()
            .zip(rows.iter().rev())
            .take(max_suffix)
            .take_while(|(a, b)| a == b)
            .count();
        if prefix + suffix != old.len() || old.len() != rows.len() {
            view.list
                .splice(prefix..old.len() - suffix, rows.len() - prefix - suffix);
        }
        // Create or update the selectable text of each row.
        let mut changed = Vec::new();
        let mut created = false;
        for (key, text) in texts {
            match view.texts.get_mut(&key) {
                Some((state, current)) if *current != text => {
                    state.update(cx, |state, cx| state.set_value(text.clone(), window, cx));
                    *current = text;
                    changed.push(key);
                }
                Some(_) => {}
                None => {
                    let state = cx.new(|cx| {
                        TextareaState::new(window, cx)
                            .auto_grow(1, 100_000)
                            .default_value(text.clone())
                    });
                    view.texts.insert(key, (state, text));
                    created = true;
                }
            }
        }
        view.texts.retain(|key, _| rows.contains(key));
        if created {
            // New text views learn their wrapped height during their first
            // layout; measure the rows again once that has happened.
            let list = view.list.clone();
            window.on_next_frame(move |window, _| {
                let list = list.clone();
                window.on_next_frame(move |window, _| {
                    list.remeasure();
                    window.refresh();
                });
            });
        }
        for key in changed {
            if let Some(ix) = rows.iter().position(|r| *r == key) {
                view.list.remeasure_items(ix..ix + 1);
            }
        }
        view.rows = rows;
        cx.notify();
    }

    // ----- Sending ----------------------------------------------------------

    fn provider_for(&self, bot: Uuid) -> Option<String> {
        self.provider_choice
            .get(&bot)
            .cloned()
            .or_else(|| {
                self.view
                    .as_ref()
                    .filter(|v| v.bot_id == bot)
                    .and_then(|v| v.conversation.as_ref())
                    .and_then(|c| c.selected_provider_id.clone())
            })
            .filter(|id| {
                self.caps
                    .as_ref()
                    .is_some_and(|caps| caps.providers.iter().any(|p| p.id == *id && p.available))
            })
            .or_else(|| {
                self.caps
                    .as_ref()
                    .and_then(|c| c.default_provider_id.clone())
            })
    }

    /// Choose the next available provider for the selected bot's next message.
    fn cycle_provider(&mut self, cx: &mut Context<Self>) {
        let (Some(bot), Some(caps)) = (self.selected, self.caps.as_ref()) else {
            return;
        };
        let available: Vec<&str> = caps
            .providers
            .iter()
            .filter(|p| p.available)
            .map(|p| p.id.as_str())
            .collect();
        if available.is_empty() {
            return;
        }
        let current = self.provider_for(bot);
        let ix = current
            .and_then(|c| available.iter().position(|p| *p == c))
            .map_or(0, |ix| (ix + 1) % available.len());
        self.provider_choice.insert(bot, available[ix].to_owned());
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bot) = self.selected else { return };
        let text = self.composer.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        let limit = self
            .caps
            .as_ref()
            .map_or(32 * 1024, |c| c.limits.max_text_bytes);
        if text.len() > limit {
            self.notice = Some(format!(
                "The message is too long ({} KiB maximum).",
                limit / 1024
            ));
            cx.notify();
            return;
        }
        let pending = PendingSend {
            request_id: Uuid::new_v4(),
            text,
            provider_id: self.provider_for(bot),
            unknown: None,
        };
        self.composer
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.drafts.remove(&bot);
        self.notice = None;
        self.pending.entry(bot).or_default().push(pending.clone());
        self.sync_rows(window, cx);
        self.submit(bot, pending, window, cx);
    }

    /// POST a pending send. Retrying reuses its request ID and text, so the
    /// server deduplicates it.
    fn submit(
        &mut self,
        bot: Uuid,
        pending: PendingSend,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = client
                .send(
                    bot,
                    pending.request_id,
                    pending.text.clone(),
                    pending.provider_id.clone(),
                )
                .await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.apply_send(bot, pending, result, window, cx)
            });
        })
        .detach();
    }

    fn apply_send(
        &mut self,
        bot: Uuid,
        pending: PendingSend,
        result: Result<Sent, ClientError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(sent) => self.accept_sent(bot, sent, window, cx),
            Err(err) if err.outcome_unknown() => {
                if let Some(p) = self
                    .pending
                    .get_mut(&bot)
                    .and_then(|list| list.iter_mut().find(|p| p.request_id == pending.request_id))
                {
                    p.unknown = Some(err.to_string());
                }
                cx.notify();
            }
            Err(err) => {
                // Definitely rejected: nothing was accepted. Give the text back.
                if let Some(list) = self.pending.get_mut(&bot) {
                    list.retain(|p| p.request_id != pending.request_id);
                }
                if self.selected == Some(bot) && self.composer.read(cx).value().is_empty() {
                    self.composer.update(cx, |state, cx| {
                        state.set_value(pending.text.clone(), window, cx)
                    });
                } else if self.drafts.get(&bot).is_none_or(|d| d.is_empty()) {
                    self.drafts.insert(bot, pending.text.clone());
                }
                self.notice = Some(err.to_string());
                self.sync_rows(window, cx);
            }
        }
    }

    fn accept_sent(&mut self, bot: Uuid, sent: Sent, window: &mut Window, cx: &mut Context<Self>) {
        let Sent { item, turn } = sent;
        if let Some(conversation) = self
            .view
            .as_mut()
            .filter(|v| v.bot_id == bot)
            .and_then(|v| v.conversation.as_mut())
        {
            conversation.insert_item(item);
            conversation.insert_turn(turn);
        } else if let Some(list) = self.pending.get_mut(&bot) {
            list.retain(|p| Some(p.request_id) != item.request_id);
        }
        self.sync_rows(window, cx);
    }

    /// After reconnecting, ask the server about sends whose outcome is
    /// unknown. Found ones are shown; missing ones stay retryable.
    fn resolve_unknown_sends(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        for (bot, list) in &self.pending {
            for pending in list.iter().filter(|p| p.unknown.is_some()) {
                let (client, bot, request_id) = (client.clone(), *bot, pending.request_id);
                cx.spawn_in(window, async move |this, cx| {
                    let found = client.by_request(bot, request_id).await;
                    let _ = this.update_in(cx, |app, window, cx| match found {
                        Ok(Some(sent)) => app.accept_sent(bot, sent, window, cx),
                        // Not received: resending the same request ID is safe
                        // because the server deduplicates it.
                        Ok(None) => app.retry_pending_for(bot, request_id, window, cx),
                        Err(_) => {}
                    });
                })
                .detach();
            }
        }
    }

    fn retry_pending(&mut self, request_id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bot) = self.selected {
            self.retry_pending_for(bot, request_id, window, cx);
        }
    }

    fn retry_pending_for(
        &mut self,
        bot: Uuid,
        request_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self
            .pending
            .get_mut(&bot)
            .and_then(|list| list.iter_mut().find(|p| p.request_id == request_id))
        else {
            return;
        };
        pending.unknown = None;
        let pending = pending.clone();
        cx.notify();
        self.submit(bot, pending, window, cx);
    }

    // ----- Stop, create, delete ---------------------------------------------

    fn stop_work(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(bot)) = (self.client.clone(), self.selected) else {
            return;
        };
        self.stopping.insert(bot);
        cx.notify();
        let request_id = Uuid::new_v4();
        cx.spawn_in(window, async move |this, cx| {
            // A lost response is retried with the same request ID.
            let mut attempts = 0;
            let result = loop {
                attempts += 1;
                match client.clone().cancel(bot, request_id).await {
                    Err(err) if err.outcome_unknown() && attempts < 5 => {
                        cx.background_executor().timer(Duration::from_secs(1)).await;
                    }
                    other => break other,
                }
            };
            let _ = this.update_in(cx, |app, window, cx| {
                if let Err(err) = result {
                    app.stopping.remove(&bot);
                    app.notice = Some(format!("Stop work failed: {err}"));
                }
                app.refresh_now(window, cx);
            });
        })
        .detach();
    }

    fn open_create_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Name, e.g. Work"));
        let role = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Role: what this bot helps with")
                .auto_grow(3, 8)
        });
        let holder = cx.new(|_| CreateFormHolder {
            form: CreateForm {
                request_id: Uuid::new_v4(),
                name: name.clone(),
                role,
                error: None,
                busy: false,
            },
        });
        self.create = Some(holder.clone());
        self._create_enter =
            Some(
                cx.subscribe_in(&name, window, |app, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        app.submit_create(window, cx);
                    }
                }),
            );
        let app = cx.entity().downgrade();
        let focus_once = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_once.replace(false) {
                name.read(cx).focus_handle(cx).focus(window, cx);
            }
            let form = &holder.read(cx).form;
            let mut body = v_flex()
                .gap_3()
                .child(Input::new(&form.name))
                .child(Textarea::new(&form.role));
            if let Some(error) = &form.error {
                body = body.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error.clone()),
                );
            }
            let app_create = app.clone();
            let busy = form.busy;
            let app_ok = app.clone();
            dialog
                .title("Create bot")
                .w(px(460.))
                .on_ok(move |_, window, cx| {
                    let _ = app_ok.update(cx, |app, cx| app.submit_create(window, cx));
                    false
                })
                .child(body)
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("create-cancel")
                                .label("Cancel")
                                .ghost()
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("create-submit")
                                .label("Create")
                                .primary()
                                .loading(busy)
                                .disabled(busy)
                                .on_click(move |_, window, cx| {
                                    let _ = app_create
                                        .update(cx, |app, cx| app.submit_create(window, cx));
                                }),
                        ),
                )
        });
    }

    fn submit_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(holder)) = (self.client.clone(), self.create.clone()) else {
            return;
        };
        if holder.read(cx).form.busy {
            return;
        }
        let (request_id, name, role) = {
            let form = &holder.read(cx).form;
            (
                form.request_id,
                form.name.read(cx).value().to_string(),
                form.role.read(cx).value().to_string(),
            )
        };
        holder.update(cx, |h, cx| {
            h.form.busy = true;
            h.form.error = None;
            cx.notify();
        });
        window.refresh();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.create_bot(request_id, name, role).await;
            let _ = this.update_in(cx, |app, window, cx| match result {
                Ok(bot) => {
                    app.create = None;
                    window.close_dialog(cx);
                    app.bots.push(bot.clone());
                    app.select(Some(bot.id), window, cx);
                    app.composer.read(cx).focus_handle(cx).focus(window, cx);
                    app.refresh_now(window, cx);
                }
                Err(err) => {
                    holder.update(cx, |h, cx| {
                        h.form.busy = false;
                        // A definite rejection lets the owner edit and retry
                        // as a new request; an unknown outcome keeps the same
                        // request ID so a retry cannot create a duplicate.
                        if !err.outcome_unknown() {
                            h.form.request_id = Uuid::new_v4();
                        }
                        h.form.error = Some(err.to_string());
                        cx.notify();
                    });
                    window.refresh();
                }
            });
        })
        .detach();
    }

    fn open_delete_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bot) = self.selected_bot().cloned() else {
            return;
        };
        let is_default = self.default_bot_id == Some(bot.id);
        let app = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let mut text = format!(
                "Delete {}? This stops its work and removes it from messaging. Its history, \
                 memory, and files are kept.",
                bot.name
            );
            if is_default {
                text.push_str(" This removes your default assistant.");
            }
            let app = app.clone();
            let id = bot.id;
            let app_ok = app.clone();
            dialog
                .title("Delete bot")
                .w(px(460.))
                .on_ok(move |_, window, cx| {
                    let _ = app_ok.update(cx, |app, cx| app.delete_bot(id, window, cx));
                    true
                })
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(text),
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("delete-cancel")
                                .label("Cancel")
                                .ghost()
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("delete-confirm")
                                .label("Delete bot")
                                .danger()
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ =
                                        app.update(cx, |app, cx| app.delete_bot(id, window, cx));
                                }),
                        ),
                )
        });
    }

    fn delete_bot(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = client.delete_bot(id).await;
            let _ = this.update_in(cx, |app, window, cx| {
                match result {
                    Ok(receipt) => {
                        if let Some(bot) = app.bots.iter_mut().find(|b| b.id == id) {
                            *bot = receipt;
                        }
                        if app.default_bot_id == Some(id) {
                            app.default_bot_id = None;
                        }
                        app.reconcile_selection(window, cx);
                    }
                    Err(err) => app.notice = Some(format!("Delete failed: {err}")),
                }
                app.refresh_now(window, cx);
            });
        })
        .detach();
    }

    // ----- Rendering --------------------------------------------------------

    fn status_text(&self) -> (SharedString, bool) {
        match &self.connection {
            Connection::Connecting => ("Connecting…".into(), false),
            Connection::Connected => ("Connected".into(), false),
            Connection::Disconnected(message) => {
                (format!("Disconnected — retrying. {message}").into(), true)
            }
            Connection::InvalidCredentials(message) => (message.clone().into(), true),
            Connection::Misconfigured(message) => (message.clone().into(), true),
        }
    }

    fn provider_label(&self, id: &str) -> String {
        self.caps
            .as_ref()
            .and_then(|caps| caps.providers.iter().find(|p| p.id == id))
            .map(|p| p.label.clone())
            .unwrap_or_else(|| id.to_owned())
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (status, is_error) = self.status_text();
        let mut list = v_flex()
            .id("bot-list")
            .gap_1()
            .p_2()
            .flex_1()
            .overflow_y_scroll();
        for bot in &self.bots {
            let id = bot.id;
            let selected = self.selected == Some(id);
            let pending = !bot.is_active();
            let activity = if pending {
                "Deleting…".to_owned()
            } else {
                activity_label(bot)
            };
            let mut name_row = h_flex()
                .gap_2()
                .child(div().text_sm().truncate().child(bot.name.clone()));
            if self.default_bot_id == Some(id) {
                name_row = name_row.child(
                    div()
                        .text_xs()
                        .px_1()
                        .rounded_sm()
                        .bg(theme.secondary)
                        .text_color(theme.secondary_foreground)
                        .child("Main"),
                );
            }
            let mut row = v_flex()
                .id(SharedString::from(format!("bot-{id}")))
                .px_2()
                .py_1p5()
                .rounded_md()
                .child(name_row)
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(activity),
                );
            if pending {
                row = row.opacity(0.5);
            } else {
                row = row
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.list_hover))
                    .on_click(
                        cx.listener(move |app, _, window, cx| app.select(Some(id), window, cx)),
                    );
            }
            if selected {
                row = row.bg(theme.list_active);
            }
            list = list.child(row);
        }
        let can_create = self.connection == Connection::Connected;
        v_flex()
            .id("sidebar")
            .key_context(SIDEBAR_CONTEXT)
            .track_focus(&self.sidebar_focus)
            .on_action(
                cx.listener(|app, _: &SelectNext, window, cx| app.move_selection(1, window, cx)),
            )
            .on_action(
                cx.listener(|app, _: &SelectPrev, window, cx| app.move_selection(-1, window, cx)),
            )
            .w(px(260.))
            .flex_none()
            .h_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                v_flex()
                    .p_3()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("else.bot"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(if is_error {
                                theme.danger
                            } else {
                                theme.muted_foreground
                            })
                            .child(status),
                    ),
            )
            .child(list)
            .child(
                div()
                    .p_2()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        Button::new("new-bot")
                            .label("New bot")
                            .w_full()
                            .disabled(!can_create)
                            .on_click(
                                cx.listener(|app, _, window, cx| {
                                    app.open_create_dialog(window, cx)
                                }),
                            ),
                    ),
            )
    }

    fn render_header(&self, bot: &Bot, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let provider = self.provider_for(bot.id);
        let mut picker = h_flex().gap_1();
        if let Some(caps) = &self.caps {
            for info in &caps.providers {
                let id = info.id.clone();
                let selected = provider.as_deref() == Some(info.id.as_str());
                let mut button = Button::new(SharedString::from(format!("provider-{}", info.id)))
                    .label(info.label.clone())
                    .xsmall()
                    .disabled(!info.available);
                button = if selected {
                    button.primary()
                } else {
                    button.ghost()
                };
                if let Some(reason) = &info.unavailable_reason {
                    button = button.tooltip(reason.clone());
                }
                let bot_id = bot.id;
                picker = picker.child(button.on_click(cx.listener(move |app, _, _, cx| {
                    app.provider_choice.insert(bot_id, id.clone());
                    cx.notify();
                })));
            }
        }
        let activity = self
            .view
            .as_ref()
            .and_then(|v| v.conversation.as_ref())
            .and_then(|c| c.activity.clone())
            .or(bot.activity.clone());
        let busy = activity.as_ref().is_some_and(|a| a.phase != "idle");
        let stopping = self.stopping.contains(&bot.id)
            || activity.as_ref().is_some_and(|a| a.phase == "cancelling");
        let mut actions = h_flex().gap_2().child(picker);
        if busy || stopping {
            actions = actions.child(
                Button::new("stop-work")
                    .label(if stopping { "Stopping…" } else { "Stop work" })
                    .small()
                    .disabled(stopping)
                    .on_click(cx.listener(|app, _, window, cx| app.stop_work(window, cx))),
            );
        }
        actions = actions.child(
            Button::new("delete-bot")
                .label("Delete")
                .small()
                .ghost()
                .on_click(cx.listener(|app, _, window, cx| app.open_delete_dialog(window, cx))),
        );
        h_flex()
            .px_4()
            .py_2()
            .gap_4()
            .justify_between()
            .border_b_1()
            .border_color(theme.border)
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(bot.name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(bot.role.clone().unwrap_or_default()),
                    ),
            )
            .child(actions.flex_none())
    }

    fn render_row(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(view) = self.view.as_ref() else {
            return div().into_any_element();
        };
        let Some(key) = view.rows.get(ix).copied() else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let bubble = |state: &Entity<TextareaState>, user: bool| {
            // Wrapped text needs a definite width, so bubbles span the row
            // and are offset to their side.
            let side = div().w_full();
            let side = if user {
                side.pl(px(96.))
            } else {
                side.pr(px(96.))
            };
            side.child(
                div()
                    .w_full()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(if user {
                        theme.secondary
                    } else {
                        theme.background
                    })
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        Textarea::new(state)
                            .readonly(true)
                            .appearance(false)
                            .bordered(false),
                    ),
            )
        };
        let note = |text: String, error: bool| {
            div()
                .text_xs()
                .text_color(if error {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(text)
        };
        match key {
            RowKey::Older => {
                let loading = view.loading_older;
                div()
                    .flex()
                    .justify_center()
                    .py_2()
                    .child(
                        Button::new("load-older")
                            .label(if loading {
                                "Loading…"
                            } else {
                                "Load older messages"
                            })
                            .xsmall()
                            .ghost()
                            .disabled(loading)
                            .on_click(cx.listener(|app, _, window, cx| app.load_older(window, cx))),
                    )
                    .into_any_element()
            }
            RowKey::Item(id) => {
                let Some(conversation) = view.conversation.as_ref() else {
                    return div().into_any_element();
                };
                let Some(item) = conversation.item(id) else {
                    return div().into_any_element();
                };
                let Some((state, _)) = view.texts.get(&key) else {
                    return div().into_any_element();
                };
                let user = item.kind == "user";
                let turn = conversation.turn(item.turn_id);
                // Children stretch so bubbles get a definite width; notes
                // are aligned to the bubble's side.
                let mut column = v_flex().w_full().gap_1().px_4().py_1();
                if !user {
                    if let Some(provider) = &item.provider_id {
                        column = column.child(note(self.provider_label(provider), false));
                    }
                }
                column = column.child(bubble(state, user));
                if let Some(turn) = turn {
                    let show_on_user = user && !turn.is_finished()
                        || user
                            && turn.status != "completed"
                            && !conversation
                                .items()
                                .any(|i| i.turn_id == turn.id && i.kind == "assistant");
                    let show_on_assistant = !user && turn.status != "completed";
                    if show_on_user || show_on_assistant {
                        let (label, error) = turn_label(turn);
                        if !label.is_empty() {
                            let row = h_flex().w_full().child(note(label, error));
                            column = column.child(if user { row.justify_end() } else { row });
                        }
                    }
                }
                column.into_any_element()
            }
            RowKey::Pending(request_id) => {
                let Some(pending) = self
                    .selected
                    .and_then(|bot| self.pending.get(&bot))
                    .and_then(|list| list.iter().find(|p| p.request_id == request_id))
                else {
                    return div().into_any_element();
                };
                let Some((state, _)) = view.texts.get(&key) else {
                    return div().into_any_element();
                };
                let mut column = v_flex()
                    .w_full()
                    .gap_1()
                    .px_4()
                    .py_1()
                    .child(bubble(state, true));
                column = match &pending.unknown {
                    None => column.child(
                        h_flex()
                            .w_full()
                            .justify_end()
                            .child(note("Sending…".into(), false)),
                    ),
                    Some(reason) => column.child(
                        h_flex()
                            .w_full()
                            .justify_end()
                            .gap_2()
                            .child(note(format!("Not confirmed. {reason}"), true))
                            .child(
                                Button::new(SharedString::from(format!("retry-{request_id}")))
                                    .label("Retry")
                                    .xsmall()
                                    .on_click(cx.listener(move |app, _, window, cx| {
                                        app.retry_pending(request_id, window, cx)
                                    })),
                            ),
                    ),
                };
                column.into_any_element()
            }
        }
    }

    fn render_conversation(&self, bot: &Bot, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let view = self.view.as_ref().filter(|v| v.bot_id == bot.id);
        let transcript: AnyElement = match view {
            Some(view) if view.conversation.is_some() => {
                if view.rows.is_empty() {
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(theme.muted_foreground)
                        .child("No messages yet. Say hello.")
                        .into_any_element()
                } else {
                    list(
                        view.list.clone(),
                        cx.processor(|app, ix, window, cx| app.render_row(ix, window, cx)),
                    )
                    .flex_1()
                    .py_2()
                    .into_any_element()
                }
            }
            _ => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child("Loading conversation…")
                .into_any_element(),
        };
        let mut footer = v_flex()
            .gap_1()
            .px_4()
            .py_3()
            .border_t_1()
            .border_color(theme.border);
        if let Some(problem) = view.and_then(|v| v.stream_problem.clone()) {
            footer = footer.child(div().text_xs().text_color(theme.warning).child(problem));
        }
        if let Some(notice) = &self.notice {
            footer = footer.child(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(notice.clone()),
            );
        }
        let can_send = self.connection == Connection::Connected;
        footer = footer.child(
            h_flex()
                .gap_2()
                .items_end()
                .child(
                    div()
                        .key_context(COMPOSER_CONTEXT)
                        .flex_1()
                        .min_w_0()
                        .child(Textarea::new(&self.composer)),
                )
                .child(
                    Button::new("send")
                        .label("Send")
                        .primary()
                        .disabled(!can_send)
                        .on_click(cx.listener(|app, _, window, cx| app.send(window, cx))),
                ),
        );
        v_flex()
            .size_full()
            .min_w_0()
            .child(self.render_header(bot, cx))
            .child(transcript)
            .child(footer)
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        if let Some(bot) = self.selected_bot().filter(|b| b.is_active()).cloned() {
            return v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .overflow_hidden()
                .bg(theme.background)
                .child(self.render_conversation(&bot, cx))
                .into_any_element();
        }
        let message = match self.connection {
            Connection::Connected => "Create a bot to start a conversation.",
            Connection::InvalidCredentials(_) | Connection::Misconfigured(_) => {
                "Fix the desktop configuration shown in the sidebar, then restart the desktop."
            }
            _ => "Waiting for the API…",
        };
        let mut body = v_flex()
            .flex_1()
            .h_full()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(theme.background)
            .child(div().text_color(theme.muted_foreground).child(message));
        if self.connection == Connection::Connected {
            body = body.child(
                Button::new("empty-create")
                    .label("Create bot")
                    .primary()
                    .on_click(cx.listener(|app, _, window, cx| app.open_create_dialog(window, cx))),
            );
        }
        body.into_any_element()
    }
}

fn activity_label(bot: &Bot) -> String {
    let Some(activity) = &bot.activity else {
        return String::new();
    };
    match activity.phase.as_str() {
        "working" if activity.queued_count > 0 => {
            format!("Working · {} queued", activity.queued_count)
        }
        "working" => "Working".into(),
        "queued" => format!("{} queued", activity.queued_count),
        "cancelling" => "Stopping…".into(),
        "unavailable" => "Unavailable".into(),
        _ => "Idle".into(),
    }
}

/// A short public description of a turn's state. Returns (text, is_error).
fn turn_label(turn: &crate::model::Turn) -> (String, bool) {
    let detail = turn
        .error
        .as_ref()
        .and_then(|e| e.message.clone())
        .unwrap_or_default();
    match turn.status.as_str() {
        "queued" => ("Queued".into(), false),
        "preparing" | "sending" | "running" => ("Working…".into(), false),
        "cancelling" => ("Stopping…".into(), false),
        "interrupted" => (
            if turn.error.as_ref().is_some_and(|e| e.code == "cancelled") {
                "Stopped before it ran".into()
            } else {
                "Stopped".into()
            },
            false,
        ),
        "failed" => (format!("Failed. {detail}"), true),
        "uncertain" => (format!("Outcome unknown. {detail}"), true),
        _ => (String::new(), false),
    }
}

impl Render for ElseApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .key_context(APP_CONTEXT)
            .on_action(cx.listener(|app, _: &FocusComposer, window, cx| {
                if app.selected.is_some() {
                    app.composer.read(cx).focus_handle(cx).focus(window, cx);
                }
            }))
            .on_action(cx.listener(|app, _: &FocusSidebar, window, cx| {
                app.sidebar_focus.focus(window, cx);
            }))
            .on_action(cx.listener(|app, _: &NewBot, window, cx| {
                if app.connection == Connection::Connected {
                    app.open_create_dialog(window, cx);
                }
            }))
            .on_action(cx.listener(|app, _: &StopWork, window, cx| {
                if app.selected_bot().is_some_and(|b| b.is_active()) {
                    app.stop_work(window, cx);
                }
            }))
            .on_action(cx.listener(|app, _: &DeleteBot, window, cx| {
                app.open_delete_dialog(window, cx);
            }))
            .on_action(cx.listener(|app, _: &NextProvider, _, cx| app.cycle_provider(cx)))
            .on_action(cx.listener(|app, _: &LoadOlder, window, cx| app.load_older(window, cx)))
            .size_full()
            .text_color(theme.foreground)
            .child(self.render_sidebar(cx))
            .child(self.render_main(cx))
    }
}
