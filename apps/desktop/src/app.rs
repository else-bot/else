//! The messaging window: bot sidebar, selected conversation, and composer.
//!
//! All network work runs on the API client's runtime; GPUI tasks only await
//! results and apply them to this view. The conversation model accepts only
//! contiguous committed events, and any inconsistency reloads a snapshot.

mod bot_menu;
mod categories;
mod conversation;
mod dialogs;
mod link_target;
mod message_menu;
mod message_row;
mod reading;
mod reply;
mod row_text;
mod secret_card;
mod settings;
mod view;

use categories::CategoryFormHolder;
use conversation::{ConversationView, RowKey};
use dialogs::{CreateFormHolder, Welcome};
use reply::Draft;
use settings::{BotSettings, ModelDialog};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use gpui_kit::component::input::{InputEvent, TextareaState};
use gpui_kit::*;
use uuid::Uuid;

use crate::api::{
    Bot, BotList, Capabilities, Category, Client, ClientError, Live, LiveCommand, LiveEvent, Sent,
};
use crate::model::Reaction;

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
        MoveBot,
        ToggleSettings
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
        KeyBinding::new("ctrl-shift-b", ToggleSettings, Some(APP_CONTEXT)),
    ]);
}

/// What the window knows about its API connection.
#[derive(Debug, Clone, PartialEq)]
pub enum Connection {
    Connecting,
    Connected,
    /// Shown with any history already loaded; the live connection keeps retrying.
    Disconnected(String),
    /// Retrying will not help until the token is fixed.
    InvalidCredentials(String),
}

/// An owner message not yet confirmed by the server.
#[derive(Debug, Clone)]
struct PendingSend {
    request_id: Uuid,
    text: String,
    sent_at: DateTime<Utc>,
    provider_id: Option<String>,
    reaction: Option<Reaction>,
    /// `None` while sending; otherwise why the outcome is unknown.
    unknown: Option<String>,
    /// Set when the server definitely refused it. The text stays here until
    /// the owner restores or dismisses it, so it is never lost.
    rejected: Option<String>,
}

pub struct ElseApp {
    client: Arc<Client>,
    connection: Connection,
    caps: Option<Capabilities>,
    bots: Vec<Bot>,
    categories: Vec<Category>,
    default_bot_id: Option<Uuid>,
    selected: Option<Uuid>,
    view: Option<ConversationView>,
    composer: Entity<TextareaState>,
    drafts: HashMap<Uuid, Draft>,
    pending: HashMap<Uuid, Vec<PendingSend>>,
    stopping: HashSet<Uuid>,
    inbox: crate::inbox::Inbox,
    /// Names this connection in notification tags.
    scope: Uuid,
    /// Covered by the connection editor.
    hidden: bool,
    _alerts: Option<Task<()>>,
    deleting: HashSet<Uuid>,
    notice: Option<String>,
    create: Option<Entity<CreateFormHolder>>,
    _create_enter: Option<Subscription>,
    welcome: Option<Welcome>,
    _welcome_enter: Option<Subscription>,
    category_form: Option<Entity<CategoryFormHolder>>,
    model_dialog: Option<Entity<ModelDialog>>,
    _category_enter: Option<Subscription>,
    /// Bots with a move in flight: one at a time per bot, so an earlier
    /// choice can never land after a later one.
    moving: HashSet<Uuid>,
    /// The last category operation's failure, shown in the sidebar.
    category_notice: Option<String>,
    settings_open: bool,
    settings: HashMap<Uuid, BotSettings>,
    sidebar_focus: FocusHandle,
    live: Option<crate::api::LiveCommands>,
    _live: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ElseApp {
    pub fn new(client: Arc<Client>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar_focus = cx.focus_handle();
        sidebar_focus.focus(window, cx);
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
        let activation =
            cx.observe_window_activation(window, |app, window, cx| app.check_reading(window, cx));
        let visibility = cx
            .observe_window_visibility(window, |app, _, window, cx| app.check_reading(window, cx));
        let bounds =
            cx.observe_window_bounds(window, |app, window, cx| app.check_reading(window, cx));
        let mut app = Self {
            client,
            connection: Connection::Connecting,
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
            inbox: Default::default(),
            scope: Uuid::new_v4(),
            hidden: false,
            _alerts: None,
            deleting: HashSet::new(),
            notice: None,
            create: None,
            _create_enter: None,
            welcome: None,
            _welcome_enter: None,
            category_form: None,
            model_dialog: None,
            _category_enter: None,
            moving: HashSet::new(),
            category_notice: None,
            settings_open: true,
            settings: HashMap::new(),
            sidebar_focus,
            live: None,
            _live: None,
            _subscriptions: vec![subscription, activation, visibility, bounds],
        };
        app.start_live(window, cx);
        app
    }

    // ----- Bot list ---------------------------------------------------------

    /// Open the live connection. The `bots` channel keeps the sidebar
    /// current; `bot:<id>` channels stream the open conversation.
    /// Capabilities are fetched over HTTP each time the connection opens.
    fn start_live(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
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
                    let _ = this.update_in(cx, |app, window, cx| app.apply_caps(caps, window, cx));
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
                self.resume_reads(cx);
            }
            LiveEvent::Disconnected(reason) => {
                // Keep whatever was loaded; show the problem and keep retrying.
                self.connection = Connection::Disconnected(reason.clone());
                if let Some(view) = self.view.as_mut() {
                    view.stream_problem = Some(format!("Reconnecting… {reason}"));
                }
                // Values typed into a secret form do not outlive the connection.
                self.clear_secret_forms(window, cx);
                self.sync_secret_forms(window, cx);
            }
            LiveEvent::Unauthorized => {
                self.connection =
                    Connection::InvalidCredentials(ClientError::Unauthorized.to_string());
                self.clear_secret_forms(window, cx);
                self.sync_secret_forms(window, cx);
            }
            LiveEvent::Bots(list) => self.apply_bots(list, window, cx),
            LiveEvent::Bot { bot, message } => self.apply_stream(bot, message, window, cx),
        }
        cx.notify();
    }

    fn apply_caps(
        &mut self,
        caps: Result<Capabilities, ClientError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match caps {
            Ok(caps) => {
                if !caps.onboarding_required {
                    self.welcome = None;
                } else if self.welcome.is_none() {
                    self.open_welcome(&caps, window, cx);
                }
                self.caps = Some(caps);
            }
            Err(ClientError::Unauthorized) => {
                self.connection =
                    Connection::InvalidCredentials(ClientError::Unauthorized.to_string());
                self.clear_secret_forms(window, cx);
                self.sync_secret_forms(window, cx);
            }
            Err(_) => {}
        }
        cx.notify();
    }

    /// Fetch the bot list once over HTTP, right after this window changed
    /// it, without waiting for the `bots` channel.
    fn refresh_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
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
        self.observe_replies(window, cx);
        self.reconcile_selection(window, cx);
        self.sync_settings(window, cx);
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

    /// The owner chose this bot. Selection alone never reads it; the
    /// replies it shows are read once they are on screen.
    fn open_bot(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.inbox.open(id);
        self.select(Some(id), window, cx);
        self.check_reading(window, cx);
    }

    fn select(&mut self, id: Option<Uuid>, window: &mut Window, cx: &mut Context<Self>) {
        if id == self.selected && self.view.is_some() {
            return;
        }
        // Keep the draft of the bot being left.
        if let Some(previous) = self.selected {
            self.drafts.entry(previous).or_default().text =
                self.composer.read(cx).value().to_string();
        }
        self.selected = id;
        let draft = id
            .and_then(|id| self.drafts.get(&id))
            .map(|draft| draft.text.clone())
            .unwrap_or_default();
        self.composer
            .update(cx, |state, cx| state.set_value(draft, window, cx));
        self.notice = None;
        // Leaving a conversation unsubscribes from its channel and drops its
        // fetch tasks; the bot's server work is unaffected.
        if let Some(old) = self.view.as_ref() {
            self.live_command(LiveCommand::Unsubscribe { bot: old.bot_id });
        }
        self.clear_secret_forms(window, cx);
        self.view = id.map(ConversationView::new);
        if let Some(view) = &self.view {
            let this = cx.entity().downgrade();
            view.list.set_scroll_handler(move |_, window, cx| {
                let _ = this.update(cx, |app, cx| app.check_reading(window, cx));
            });
        }
        self.ensure_settings(window, cx);
        if id.is_some() {
            // Pending sends show at once, even before history loads.
            self.sync_rows(window, cx);
            self.load_conversation(window, cx);
        }
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        // The order the sidebar shows.
        let ids =
            crate::sidebar::Layout::new(&self.categories, &self.bots, self.default_bot_id).order();
        if ids.is_empty() {
            return;
        }
        let current = self
            .selected
            .and_then(|id| ids.iter().position(|b| *b == id))
            .unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, ids.len() as isize - 1) as usize;
        self.open_bot(ids[next], window, cx);
    }

    fn selected_bot(&self) -> Option<&Bot> {
        self.selected
            .and_then(|id| self.bots.iter().find(|b| b.id == id))
    }

    // ----- Conversation and stream -----------------------------------------

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bot) = self.selected else { return };
        let draft = Draft {
            text: self.composer.read(cx).value().to_string(),
            reply_to: self.drafts.get(&bot).and_then(|d| d.reply_to.clone()),
        };
        let Some(message) = draft.message() else {
            return;
        };
        if !self.enqueue(bot, message, None, window, cx) {
            return;
        }
        self.composer
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.drafts.remove(&bot);
    }

    fn enqueue(
        &mut self,
        bot: Uuid,
        text: String,
        reaction: Option<Reaction>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.connection != Connection::Connected {
            return false;
        }
        let limit = self
            .caps
            .as_ref()
            .map_or(32 * 1024, |c| c.limits.max_text_bytes);
        if let Some(notice) = reply::too_long(&text, limit) {
            self.notice = Some(notice);
            cx.notify();
            return false;
        }
        let pending = PendingSend {
            request_id: Uuid::new_v4(),
            text,
            sent_at: Utc::now(),
            // The bot's own provider and model, chosen by the API.
            provider_id: None,
            reaction,
            unknown: None,
            rejected: None,
        };
        self.notice = None;
        self.pending.entry(bot).or_default().push(pending.clone());
        self.sync_rows(window, cx);
        self.submit(bot, pending, window, cx);
        true
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
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client
                .send(
                    bot,
                    pending.request_id,
                    pending.text.clone(),
                    pending.provider_id.clone(),
                    pending.reaction,
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
        let client = self.client.clone();
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
        let (client, Some(bot)) = (self.client.clone(), self.selected) else {
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

    // ----- Welcome ----------------------------------------------------------
}
