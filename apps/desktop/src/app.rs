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
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::*;
use uuid::Uuid;

use crate::api::{
    Bot, BotList, Capabilities, Category, Client, ClientError, Live, LiveCommand, LiveEvent,
    ProfileChoice, Sent, StreamMessage,
};
use crate::model::{Applied, Conversation, Snapshot};
use crate::profile_picker::ProfilePicker;

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
        LoadOlder,
        NewCategory,
        MoveBot
    ]
);

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
        KeyBinding::new("ctrl-shift-up", LoadOlder, Some(APP_CONTEXT)),
        KeyBinding::new("ctrl-shift-g", NewCategory, Some(APP_CONTEXT)),
        KeyBinding::new("ctrl-shift-m", MoveBot, Some(APP_CONTEXT)),
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
    /// Set when the server definitely refused it. The text stays here until
    /// the owner restores or dismisses it, so it is never lost.
    rejected: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RowKey {
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
    /// The exact payload of a request whose outcome is unknown. Retries
    /// resend it unchanged under the same request ID, whatever the fields
    /// now say, until the server gives a definite answer.
    in_doubt: Option<(String, String, Option<ProfileChoice>)>,
    name: Entity<InputState>,
    role: Entity<TextareaState>,
    /// The new bot's provider and model, starting from the defaults.
    profile: Option<Entity<ProfilePicker>>,
    error: Option<String>,
    busy: bool,
    /// The category the new bot goes into (POC-031).
    category: Option<Uuid>,
}

/// Creating or renaming a category.
struct CategoryForm {
    /// `None` creates; `Some` renames that category.
    renaming: Option<Uuid>,
    request_id: Uuid,
    /// A create whose outcome is unknown is retried unchanged.
    in_doubt: Option<String>,
    name: Entity<InputState>,
    error: Option<String>,
    busy: bool,
}

pub struct CategoryFormHolder {
    form: CategoryForm,
}

/// The Move dialog's pending choice for one bot.
pub struct MoveHolder {
    bot: Uuid,
    current: Option<Uuid>,
    choice: Option<Uuid>,
}

pub struct ElseApp {
    client: Option<Arc<Client>>,
    connection: Connection,
    caps: Option<Capabilities>,
    bots: Vec<Bot>,
    categories: Vec<Category>,
    default_bot_id: Option<Uuid>,
    selected: Option<Uuid>,
    view: Option<ConversationView>,
    composer: Entity<TextareaState>,
    drafts: HashMap<Uuid, String>,
    pending: HashMap<Uuid, Vec<PendingSend>>,
    stopping: HashSet<Uuid>,
    notice: Option<String>,
    create: Option<Entity<CreateFormHolder>>,
    _create_enter: Option<Subscription>,
    category_form: Option<Entity<CategoryFormHolder>>,
    _category_enter: Option<Subscription>,
    /// Bots with a move in flight: one at a time per bot, so an earlier
    /// choice can never land after a later one.
    moving: HashSet<Uuid>,
    /// The last category operation's failure, shown in the sidebar.
    category_notice: Option<String>,
    sidebar_focus: FocusHandle,
    live: Option<tokio::sync::mpsc::UnboundedSender<LiveCommand>>,
    _live: Option<Task<()>>,
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
            categories: Vec::new(),
            default_bot_id: None,
            selected: None,
            view: None,
            composer,
            drafts: HashMap::new(),
            pending: HashMap::new(),
            stopping: HashSet::new(),
            notice: None,
            create: None,
            _create_enter: None,
            category_form: None,
            _category_enter: None,
            moving: HashSet::new(),
            category_notice: None,
            sidebar_focus,
            live: None,
            _live: None,
            _subscriptions: vec![subscription],
        };
        app.start_live(window, cx);
        app
    }

    // ----- Bot list ---------------------------------------------------------

    /// Open the live connection. The `bots` channel keeps the sidebar
    /// current; `bot:<id>` channels stream the open conversation.
    /// Capabilities are fetched over HTTP each time the connection opens.
    fn start_live(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Live {
            commands,
            mut events,
        } = client.clone().live();
        self.live = Some(commands);
        self._live = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events.recv().await {
                let connected = matches!(event, LiveEvent::Connected);
                if this
                    .update_in(cx, |app, window, cx| app.apply_live(event, window, cx))
                    .is_err()
                {
                    return;
                }
                if connected {
                    let caps = client.clone().capabilities().await;
                    let _ = this.update_in(cx, |app, _, cx| app.apply_caps(caps, cx));
                }
            }
        }));
    }

    fn live_command(&self, command: LiveCommand) {
        if let Some(live) = &self.live {
            let _ = live.send(command);
        }
    }

    fn apply_live(&mut self, event: LiveEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            LiveEvent::Connected => {
                self.connection = Connection::Connected;
                if let Some(view) = self.view.as_mut() {
                    view.stream_problem = None;
                    if view.conversation.is_none() {
                        self.load_conversation(window, cx);
                    }
                }
                // Resolve sends whose outcome became unknown while offline.
                self.resolve_unknown_sends(window, cx);
            }
            LiveEvent::Disconnected(reason) => {
                // Keep whatever was loaded; show the problem and keep retrying.
                self.connection = Connection::Disconnected(reason.clone());
                if let Some(view) = self.view.as_mut() {
                    view.stream_problem = Some(format!("Reconnecting… {reason}"));
                }
            }
            LiveEvent::Unauthorized => {
                self.connection =
                    Connection::InvalidCredentials(ClientError::Unauthorized.to_string());
            }
            LiveEvent::Bots(list) => self.apply_bots(list, window, cx),
            LiveEvent::Bot { bot, message } => self.apply_stream(bot, message, window, cx),
        }
        cx.notify();
    }

    fn apply_caps(&mut self, caps: Result<Capabilities, ClientError>, cx: &mut Context<Self>) {
        match caps {
            Ok(caps) => self.caps = Some(caps),
            Err(ClientError::Unauthorized) => {
                self.connection =
                    Connection::InvalidCredentials(ClientError::Unauthorized.to_string());
            }
            Err(_) => {}
        }
        cx.notify();
    }

    /// Fetch the bot list once over HTTP, right after this window changed
    /// it, without waiting for the `bots` channel.
    fn refresh_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(list) = client.bots().await {
                let _ = this.update_in(cx, |app, window, cx| app.apply_bots(list, window, cx));
            }
        })
        .detach();
    }

    fn apply_bots(&mut self, list: BotList, window: &mut Window, cx: &mut Context<Self>) {
        self.default_bot_id = list.default_bot_id;
        self.bots = list.bots;
        self.categories = list.categories;
        self.stopping.retain(|id| {
            self.bots
                .iter()
                .any(|b| b.id == *id && b.activity.as_ref().is_some_and(|a| a.phase != "idle"))
        });
        self.reconcile_selection(window, cx);
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
        // Leaving a conversation unsubscribes from its channel and drops its
        // fetch tasks; the bot's server work is unaffected.
        if let Some(old) = self.view.as_ref() {
            self.live_command(LiveCommand::Unsubscribe { bot: old.bot_id });
        }
        self.view = id.map(ConversationView::new);
        if id.is_some() {
            // Pending sends show at once, even before history loads.
            self.sync_rows(window, cx);
            self.load_conversation(window, cx);
        }
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        // The order the sidebar shows.
        let ids: Vec<Uuid> = crate::sidebar::order(&self.categories, &self.bots);
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

    /// Load a snapshot, then subscribe to the bot's channel from its cursor.
    /// Any reset repeats this; reconnects resume from the last applied cursor.
    fn load_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(view)) = (self.client.clone(), self.view.as_mut()) else {
            return;
        };
        let bot = view.bot_id;
        view._load = Some(cx.spawn_in(window, async move |this, cx| {
            let snapshot = client.snapshot(bot, None).await;
            let _ = this.update_in(cx, |app, window, cx| {
                if let Some(cursor) = app.apply_snapshot(bot, snapshot, window, cx) {
                    app.live_command(LiveCommand::Subscribe { bot, cursor });
                }
            });
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

    fn apply_stream(
        &mut self,
        bot: Uuid,
        message: StreamMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Events for a conversation no longer open are ignored.
        let Some(view) = self.view.as_mut().filter(|v| v.bot_id == bot) else {
            return;
        };
        match message {
            StreamMessage::Frame(frame) => {
                let Some(conversation) = view.conversation.as_mut() else {
                    return;
                };
                match conversation.apply(&frame) {
                    Ok(Applied::Duplicate) => {}
                    Ok(Applied::Changed) => {
                        // The channel's activity is fresher than the list.
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
                    }
                    Err(_) => {
                        // Drop this subscription and reload a snapshot.
                        self.live_command(LiveCommand::Unsubscribe { bot });
                        self.load_conversation(window, cx);
                    }
                }
            }
            StreamMessage::Reset => self.load_conversation(window, cx),
            StreamMessage::Deleted => self.refresh_now(window, cx),
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
        // Pending sends are shown even before history has loaded.
        let conversation = view.conversation.as_ref();
        // A pending send disappears once its canonical item is present.
        if let (Some(pending), Some(conversation)) = (self.pending.get_mut(&bot), conversation) {
            pending.retain(|p| !conversation.has_request(p.request_id));
        }
        let mut rows = Vec::new();
        let mut texts: Vec<(RowKey, String)> = Vec::new();
        for item in conversation.into_iter().flat_map(|c| c.items()) {
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
            // The bot's own provider and model, chosen by the API.
            provider_id: None,
            unknown: None,
            rejected: None,
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
                // Definitely rejected: nothing was accepted. Keep the text as a
                // recoverable entry instead of touching the composer or drafts.
                if let Some(p) = self
                    .pending
                    .get_mut(&bot)
                    .and_then(|list| list.iter_mut().find(|p| p.request_id == pending.request_id))
                {
                    p.unknown = None;
                    p.rejected = Some(err.to_string());
                }
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

    /// Remove a refused send; optionally put its text back in the composer,
    /// after whatever is already there.
    fn take_rejected(
        &mut self,
        request_id: Uuid,
        restore: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bot) = self.selected else { return };
        let Some(list) = self.pending.get_mut(&bot) else {
            return;
        };
        let Some(ix) = list
            .iter()
            .position(|p| p.request_id == request_id && p.rejected.is_some())
        else {
            return;
        };
        let pending = list.remove(ix);
        if restore {
            let current = self.composer.read(cx).value().to_string();
            let text = if current.trim().is_empty() {
                pending.text
            } else {
                format!("{current}\n\n{}", pending.text)
            };
            self.composer
                .update(cx, |state, cx| state.set_value(text, window, cx));
            self.composer.read(cx).focus_handle(cx).focus(window, cx);
        }
        self.sync_rows(window, cx);
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
        let profile = self
            .caps
            .clone()
            .map(|caps| cx.new(|cx| ProfilePicker::new(&caps, caps.defaults.as_ref(), window, cx)));
        let holder = cx.new(|_| CreateFormHolder {
            form: CreateForm {
                request_id: Uuid::new_v4(),
                in_doubt: None,
                name: name.clone(),
                role,
                profile,
                error: None,
                busy: false,
                category: None,
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
            if let Some(profile) = &form.profile {
                body = body.child(profile.clone());
            }
            // Which category the new bot goes into: a radio group (Tab to
            // one and Space to choose; Enter still creates).
            let categories = app
                .upgrade()
                .map(|app| app.read(cx).categories.clone())
                .unwrap_or_default();
            if !categories.is_empty() {
                let options: Vec<(Option<Uuid>, String)> =
                    std::iter::once((None, "None".to_owned()))
                        .chain(categories.iter().map(|c| (Some(c.id), c.name.clone())))
                        .collect();
                let selected = options.iter().position(|(id, _)| *id == form.category);
                let ids: Vec<Option<Uuid>> = options.iter().map(|(id, _)| *id).collect();
                let choose = holder.clone();
                body = body.child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("Category"),
                        )
                        .child(
                            RadioGroup::horizontal("create-category")
                                .children(options.into_iter().map(|(_, label)| label))
                                .selected_index(selected)
                                .on_change(move |ix, window, cx| {
                                    let pick = ids[*ix];
                                    choose.update(cx, |h, cx| {
                                        h.form.category = pick;
                                        cx.notify();
                                    });
                                    window.refresh();
                                }),
                        ),
                );
            }
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
            let app_close = app.clone();
            let mine = holder.clone();
            dialog
                .title("Create bot")
                .w(px(520.))
                // Dismissed: a late result must not close another dialog.
                .on_close(move |_, _, cx| {
                    let _ = app_close.update(cx, |app, _| {
                        if app.create.as_ref() == Some(&mine) {
                            app.create = None;
                        }
                    });
                })
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
        let category = holder.read(cx).form.category;
        let (request_id, name, role, profile) = {
            let form = &holder.read(cx).form;
            let (name, role, profile) = form.in_doubt.clone().unwrap_or_else(|| {
                (
                    form.name.read(cx).value().to_string(),
                    form.role.read(cx).value().to_string(),
                    form.profile.as_ref().and_then(|p| p.read(cx).choice(cx)),
                )
            });
            (form.request_id, name, role, profile)
        };
        holder.update(cx, |h, cx| {
            h.form.busy = true;
            h.form.error = None;
            cx.notify();
        });
        window.refresh();
        let payload = (name.clone(), role.clone(), profile.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = client.create_bot(request_id, name, role, profile).await;
            let _ = this.update_in(cx, |app, window, cx| match result {
                Ok(bot) => {
                    // Close only the dialog this request came from.
                    if app.create.as_ref() == Some(&holder) {
                        app.create = None;
                        window.close_dialog(cx);
                    }
                    if category.is_some() {
                        app.assign_category(bot.id, category, window, cx);
                    }
                    if app.bots.iter().any(|b| b.id == bot.id) {
                        return;
                    }
                    app.bots.push(bot.clone());
                    app.select(Some(bot.id), window, cx);
                    app.composer.read(cx).focus_handle(cx).focus(window, cx);
                    app.refresh_now(window, cx);
                }
                Err(err) => {
                    holder.update(cx, |h, cx| {
                        h.form.busy = false;
                        // A definite rejection lets the owner edit and send a
                        // new request. An unknown outcome (or a conflict,
                        // which proves nothing about the original) keeps the
                        // same ID and payload so a retry cannot duplicate.
                        if err.outcome_unknown() || err.is_request_conflict() {
                            h.form.in_doubt = Some(payload.clone());
                            h.form.error = Some(format!(
                                "{err} Create will retry the original request unchanged."
                            ));
                        } else {
                            h.form.request_id = Uuid::new_v4();
                            h.form.in_doubt = None;
                            h.form.error = Some(err.to_string());
                        }
                        cx.notify();
                    });
                    window.refresh();
                }
            });
        })
        .detach();
    }

    // ----- Categories (POC-031) ---------------------------------------------

    /// Assign (or with `None` unassign) a bot. One move per bot at a time.
    /// The list itself changes only from the server's `bots` snapshots (the
    /// one ordered source), never from this response or an extra fetch.
    fn assign_category(
        &mut self,
        bot: Uuid,
        category: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if !self.moving.insert(bot) {
            return;
        }
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.set_category(bot, category).await;
            let _ = this.update_in(cx, |app, _, cx| {
                app.moving.remove(&bot);
                match result {
                    Ok(_) => app.category_notice = None,
                    Err(err) => app.category_notice = Some(format!("Moving the bot failed: {err}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The dialog for a new category, or (with `renaming`) a new name.
    fn open_category_dialog(
        &mut self,
        renaming: Option<Category>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = renaming
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let name = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("Category name, e.g. Family");
            state.set_value(current, window, cx);
            state
        });
        let holder = cx.new(|_| CategoryFormHolder {
            form: CategoryForm {
                renaming: renaming.as_ref().map(|c| c.id),
                request_id: Uuid::new_v4(),
                in_doubt: None,
                name: name.clone(),
                error: None,
                busy: false,
            },
        });
        self.category_form = Some(holder.clone());
        self._category_enter =
            Some(
                cx.subscribe_in(&name, window, |app, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        app.submit_category(window, cx);
                    }
                }),
            );
        let app = cx.entity().downgrade();
        let title = if renaming.is_some() {
            "Rename category"
        } else {
            "New category"
        };
        let action = if renaming.is_some() {
            "Rename"
        } else {
            "Create"
        };
        let focus_once = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_once.replace(false) {
                name.read(cx).focus_handle(cx).focus(window, cx);
            }
            let form = &holder.read(cx).form;
            let mut body = v_flex().gap_3().child(Input::new(&form.name));
            if let Some(error) = &form.error {
                body = body.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error.clone()),
                );
            }
            let busy = form.busy;
            let app_ok = app.clone();
            let app_submit = app.clone();
            let app_close = app.clone();
            let mine = holder.clone();
            dialog
                .title(title)
                .w(px(420.))
                .on_ok(move |_, window, cx| {
                    let _ = app_ok.update(cx, |app, cx| app.submit_category(window, cx));
                    false
                })
                // Dismissed: a late result must not close another dialog.
                .on_close(move |_, _, cx| {
                    let _ = app_close.update(cx, |app, _| {
                        if app.category_form.as_ref() == Some(&mine) {
                            app.category_form = None;
                        }
                    });
                })
                .child(body)
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("category-cancel")
                                .label("Cancel")
                                .ghost()
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("category-submit")
                                .label(action)
                                .primary()
                                .loading(busy)
                                .disabled(busy)
                                .on_click(move |_, window, cx| {
                                    let _ = app_submit
                                        .update(cx, |app, cx| app.submit_category(window, cx));
                                }),
                        ),
                )
        });
    }

    fn submit_category(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(holder)) = (self.client.clone(), self.category_form.clone()) else {
            return;
        };
        if holder.read(cx).form.busy {
            return;
        }
        let (renaming, request_id, name) = {
            let form = &holder.read(cx).form;
            let name = form
                .in_doubt
                .clone()
                .unwrap_or_else(|| form.name.read(cx).value().to_string());
            (form.renaming, form.request_id, name)
        };
        holder.update(cx, |h, cx| {
            h.form.busy = true;
            h.form.error = None;
            cx.notify();
        });
        window.refresh();
        cx.spawn_in(window, async move |this, cx| {
            let result = match renaming {
                Some(id) => client.rename_category(id, name.clone()).await,
                None => client.create_category(request_id, name.clone()).await,
            };
            let _ = this.update_in(cx, |app, window, cx| match result {
                Ok(category) => {
                    if app.category_form.as_ref() == Some(&holder) {
                        app.category_form = None;
                        window.close_dialog(cx);
                    }
                    // The `bots` snapshot shows it.
                    let _ = category;
                    app.category_notice = None;
                    cx.notify();
                }
                Err(err) => {
                    holder.update(cx, |h, cx| {
                        h.form.busy = false;
                        // An unknown create outcome keeps the same request and
                        // name, so a retry cannot make a second category.
                        if renaming.is_none()
                            && (err.outcome_unknown() || err.is_request_conflict())
                        {
                            h.form.in_doubt = Some(name.clone());
                            h.form.error = Some(format!(
                                "{err} Create will retry the original request unchanged."
                            ));
                        } else {
                            h.form.request_id = Uuid::new_v4();
                            h.form.in_doubt = None;
                            h.form.error = Some(err.to_string());
                        }
                        cx.notify();
                    });
                    window.refresh();
                }
            });
        })
        .detach();
    }

    fn open_delete_category_dialog(
        &mut self,
        category: Category,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self
            .bots
            .iter()
            .filter(|b| b.is_active() && b.category_id == Some(category.id))
            .count();
        let app = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let text = format!(
                "Delete the category {}? {} Only the grouping is removed: no bot, \
                 conversation, memory, or file is deleted.",
                category.name,
                match count {
                    0 => "It has no bots.".to_owned(),
                    1 => "Its bot is kept and moves to Unassigned.".to_owned(),
                    n => format!("Its {n} bots are kept and move to Unassigned."),
                }
            );
            let id = category.id;
            let app_ok = app.clone();
            let app_confirm = app.clone();
            dialog
                .title("Delete category")
                .w(px(460.))
                .on_ok(move |_, window, cx| {
                    let _ = app_ok.update(cx, |app, cx| app.delete_category(id, window, cx));
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
                            Button::new("category-delete-cancel")
                                .label("Cancel")
                                .ghost()
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("category-delete-confirm")
                                .label("Delete category")
                                .danger()
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ = app_confirm
                                        .update(cx, |app, cx| app.delete_category(id, window, cx));
                                }),
                        ),
                )
        });
    }

    fn delete_category(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = client.delete_category(id).await;
            let _ = this.update_in(cx, |app, _, cx| {
                // The `bots` snapshot shows the result; a failure is shown
                // in the sidebar, where it is visible with or without a
                // selected bot.
                app.category_notice = match result {
                    Ok(()) => None,
                    Err(err) => Some(format!("Deleting the category failed: {err}")),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Move the selected bot: a radio group of every category and
    /// Unassigned (Tab and Space, or a click, choose; Enter or Move applies).
    fn open_move_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bot) = self.selected_bot().filter(|b| b.is_active()).cloned() else {
            return;
        };
        if self.moving.contains(&bot.id) {
            return;
        }
        let categories = self.categories.clone();
        let current = bot
            .category_id
            .filter(|c| categories.iter().any(|k| k.id == *c));
        let holder = cx.new(|_| MoveHolder {
            bot: bot.id,
            current,
            choice: current,
        });
        let app = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let options: Vec<(Option<Uuid>, String)> = categories
                .iter()
                .map(|c| (Some(c.id), c.name.clone()))
                .chain(std::iter::once((None, "Unassigned".to_owned())))
                .collect();
            let state = holder.read(cx);
            let selected = options.iter().position(|(id, _)| *id == state.choice);
            let unchanged = state.choice == state.current;
            let ids: Vec<Option<Uuid>> = options.iter().map(|(id, _)| *id).collect();
            let choose = holder.clone();
            let group = RadioGroup::vertical("move-choice")
                .children(options.iter().map(|(id, label)| {
                    if *id == state.current {
                        format!("{label} (current)")
                    } else {
                        label.clone()
                    }
                }))
                .selected_index(selected)
                .on_change(move |ix, window, cx| {
                    let pick = ids[*ix];
                    choose.update(cx, |h, cx| {
                        h.choice = pick;
                        cx.notify();
                    });
                    window.refresh();
                });
            let apply = {
                let app = app.clone();
                let holder = holder.clone();
                move |window: &mut Window, cx: &mut App| {
                    let (bot, choice, current) = {
                        let h = holder.read(cx);
                        (h.bot, h.choice, h.current)
                    };
                    if choice != current {
                        let _ =
                            app.update(cx, |app, cx| app.assign_category(bot, choice, window, cx));
                    }
                }
            };
            let apply_ok = apply.clone();
            dialog
                .title(format!("Move {}", bot.name))
                .w(px(380.))
                .on_ok(move |_, window, cx| {
                    apply_ok(window, cx);
                    true
                })
                .child(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(
                                    "Only its place in the sidebar changes. Tab to a category, \
                                     Space to choose, Enter moves, Esc closes.",
                                ),
                        )
                        .child(group),
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("move-cancel")
                                .label("Cancel")
                                .ghost()
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("move-apply")
                                .label("Move")
                                .primary()
                                .disabled(unchanged)
                                .on_click(move |_, window, cx| {
                                    apply(window, cx);
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn open_delete_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bot) = self.selected_bot().cloned() else {
            return;
        };
        // The main bot is the entry point and is never deleted (the API
        // refuses it too).
        if self.default_bot_id == Some(bot.id) {
            self.notice = Some(format!(
                "{} is your main bot and cannot be deleted.",
                bot.name
            ));
            cx.notify();
            return;
        }
        let app = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let text = format!(
                "Delete {}? This stops its work and removes it from messaging. Its history, \
                 memory, and files are kept.",
                bot.name
            );
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
        let theme = cx.theme().clone();
        let (status, is_error) = self.status_text();
        let mut list = v_flex()
            .id("bot-list")
            .gap_1()
            .p_2()
            .flex_1()
            .overflow_y_scroll();
        let grouped = !self.categories.is_empty();
        let groups = crate::sidebar::groups(&self.categories, &self.bots);
        let can_edit = self.connection == Connection::Connected;
        for group in groups {
            if grouped {
                // A header per category (empty ones stay), then Unassigned.
                let mut header = h_flex().mt_2().px_2().gap_1().items_center().child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(match group.category {
                            Some(c) => format!("{} · {}", c.name, group.bots.len()),
                            None => format!("Unassigned · {}", group.bots.len()),
                        }),
                );
                if let Some(category) = group.category {
                    let rename = category.clone();
                    let delete = category.clone();
                    header = header
                        .child(
                            Button::new(SharedString::from(format!("rename-{}", category.id)))
                                .label("Rename")
                                .xsmall()
                                .ghost()
                                .disabled(!can_edit)
                                .on_click(cx.listener(move |app, _, window, cx| {
                                    app.open_category_dialog(Some(rename.clone()), window, cx)
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("delete-cat-{}", category.id)))
                                .label("Delete")
                                .xsmall()
                                .ghost()
                                .disabled(!can_edit)
                                .on_click(cx.listener(move |app, _, window, cx| {
                                    app.open_delete_category_dialog(delete.clone(), window, cx)
                                })),
                        );
                }
                list = list.child(header);
                if group.bots.is_empty() {
                    list = list.child(
                        div()
                            .px_2()
                            .text_xs()
                            .italic()
                            .text_color(theme.muted_foreground)
                            .child("No bots"),
                    );
                }
            }
            for bot in group.bots {
                list = list.child(self.render_bot_row(bot, cx));
            }
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
                    )
                    .children(
                        self.category_notice
                            .clone()
                            .map(|notice| div().text_xs().text_color(theme.danger).child(notice)),
                    ),
            )
            .child(list)
            .child(
                div()
                    .p_2()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("new-bot")
                                    .label("New bot")
                                    .flex_1()
                                    .disabled(!can_create)
                                    .on_click(cx.listener(|app, _, window, cx| {
                                        app.open_create_dialog(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("new-category")
                                    .label("New category")
                                    .ghost()
                                    .disabled(!can_create)
                                    .on_click(cx.listener(|app, _, window, cx| {
                                        app.open_category_dialog(None, window, cx)
                                    })),
                            ),
                    ),
            )
    }

    fn render_bot_row(&self, bot: &Bot, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
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
                .on_click(cx.listener(move |app, _, window, cx| app.select(Some(id), window, cx)));
        }
        if selected {
            row = row.bg(theme.list_active);
        }
        row
    }

    fn render_header(&self, bot: &Bot, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let activity = self
            .view
            .as_ref()
            .and_then(|v| v.conversation.as_ref())
            .and_then(|c| c.activity.clone())
            .or(bot.activity.clone());
        let busy = activity.as_ref().is_some_and(|a| a.phase != "idle");
        let stopping = self.stopping.contains(&bot.id)
            || activity.as_ref().is_some_and(|a| a.phase == "cancelling");
        let mut actions = h_flex().gap_2();
        if busy || stopping {
            actions = actions.child(
                Button::new("stop-work")
                    .label(if stopping { "Stopping…" } else { "Stop work" })
                    .small()
                    .disabled(stopping)
                    .on_click(cx.listener(|app, _, window, cx| app.stop_work(window, cx))),
            );
        }
        let category = bot
            .category_id
            .and_then(|id| self.categories.iter().find(|c| c.id == id))
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "Unassigned".to_owned());
        actions = actions.child(
            Button::new("move-bot")
                .label(if self.moving.contains(&bot.id) {
                    "Moving…".to_owned()
                } else {
                    format!("Category: {category}")
                })
                .small()
                .ghost()
                .disabled(self.connection != Connection::Connected || self.moving.contains(&bot.id))
                .on_click(cx.listener(|app, _, window, cx| app.open_move_dialog(window, cx))),
        );
        if self.default_bot_id != Some(bot.id) {
            actions = actions.child(
                Button::new("delete-bot")
                    .label("Delete")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|app, _, window, cx| app.open_delete_dialog(window, cx))),
            );
        }
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
                if let Some(reason) = &pending.rejected {
                    return column
                        .child(
                            h_flex()
                                .w_full()
                                .justify_end()
                                .gap_2()
                                .child(note(format!("Not sent. {reason}"), true))
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "restore-{request_id}"
                                    )))
                                    .label("Restore to composer")
                                    .xsmall()
                                    .on_click(cx.listener(move |app, _, window, cx| {
                                        app.take_rejected(request_id, true, window, cx)
                                    })),
                                )
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "dismiss-{request_id}"
                                    )))
                                    .label("Dismiss")
                                    .xsmall()
                                    .ghost()
                                    .on_click(cx.listener(move |app, _, window, cx| {
                                        app.take_rejected(request_id, false, window, cx)
                                    })),
                                ),
                        )
                        .into_any_element();
                }
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
            Some(view) if view.conversation.is_some() || !view.rows.is_empty() => {
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
                    let older = view
                        .conversation
                        .as_ref()
                        .is_some_and(|c| c.older_before_seq.is_some());
                    // Outside the list, so prepending history shifts the
                    // anchored message instead of the button.
                    let mut column = v_flex().flex_1().min_h_0();
                    if older {
                        let loading = view.loading_older;
                        column =
                            column.child(
                                h_flex().justify_center().py_1().child(
                                    Button::new("load-older")
                                        .label(if loading {
                                            "Loading…"
                                        } else {
                                            "Load older messages"
                                        })
                                        .xsmall()
                                        .ghost()
                                        .disabled(loading)
                                        .on_click(cx.listener(|app, _, window, cx| {
                                            app.load_older(window, cx)
                                        })),
                                ),
                            );
                    }
                    column
                        .child(
                            list(
                                view.list.clone(),
                                cx.processor(|app, ix, window, cx| app.render_row(ix, window, cx)),
                            )
                            .flex_1()
                            .py_2(),
                        )
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
        // The main bot handed part of the request to another bot.
        "waiting" => ("Waiting for another bot…".into(), false),
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
            .on_action(cx.listener(|app, _: &LoadOlder, window, cx| app.load_older(window, cx)))
            .on_action(cx.listener(|app, _: &NewCategory, window, cx| {
                if app.connection == Connection::Connected {
                    app.open_category_dialog(None, window, cx);
                }
            }))
            .on_action(cx.listener(|app, _: &MoveBot, window, cx| {
                if app.connection == Connection::Connected {
                    app.open_move_dialog(window, cx);
                }
            }))
            .size_full()
            .text_color(theme.foreground)
            .child(self.render_sidebar(cx))
            .child(self.render_main(cx))
    }
}
