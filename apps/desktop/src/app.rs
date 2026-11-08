mod attachments;
mod bot_activity;
mod bot_exchange;
mod bot_mention;
mod bot_menu;
mod categories;
mod computer;
mod conversation;
mod create_bot;
mod delete_dialog;
mod dialogs;
mod draft_card;
mod handoff;
mod images;
mod layout;
mod link_target;
mod mentions;
mod message_menu;
mod message_row;
mod question_card;
mod reading;
mod reply;
mod row_text;
mod secret_card;
mod settings;
mod social_card;
mod task_reply;
mod view;

use categories::CategoryFormHolder;
use conversation::{ConversationView, RowKey};
use create_bot::CreateFormHolder;
use dialogs::Welcome;
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
        ToggleSettings,
        Quit,
        CloseWindow,
        Hide,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom,
        About
    ]
);

const SIDEBAR_CONTEXT: &str = "Sidebar";
const COMPOSER_CONTEXT: &str = "Composer";
const APP_CONTEXT: &str = "ElseApp";

/// The platform's command modifier: Cmd on macOS, Ctrl elsewhere.
/// `KeyBinding::new` needs the literal modifier.
macro_rules! primary {
    ($keys:literal) => {
        if cfg!(target_os = "macos") {
            concat!("cmd-", $keys)
        } else {
            concat!("ctrl-", $keys)
        }
    };
}

pub fn bind_keys(cx: &mut App) {
    computer::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, Some(SIDEBAR_CONTEXT)),
        KeyBinding::new("up", SelectPrev, Some(SIDEBAR_CONTEXT)),
        KeyBinding::new("ctrl-n", SelectNext, Some(SIDEBAR_CONTEXT)),
        KeyBinding::new("ctrl-p", SelectPrev, Some(SIDEBAR_CONTEXT)),
        KeyBinding::new("enter", FocusComposer, Some(SIDEBAR_CONTEXT)),
        KeyBinding::new("escape", FocusSidebar, Some(COMPOSER_CONTEXT)),
        KeyBinding::new(primary!("shift-n"), NewBot, Some(APP_CONTEXT)),
        KeyBinding::new(primary!("shift-x"), StopWork, Some(APP_CONTEXT)),
        KeyBinding::new(primary!("shift-d"), DeleteBot, Some(APP_CONTEXT)),
        KeyBinding::new(primary!("shift-up"), LoadOlder, Some(APP_CONTEXT)),
        KeyBinding::new(primary!("shift-g"), NewCategory, Some(APP_CONTEXT)),
        KeyBinding::new(primary!("shift-m"), MoveBot, Some(APP_CONTEXT)),
        KeyBinding::new(primary!("shift-b"), ToggleSettings, Some(APP_CONTEXT)),
        KeyBinding::new(
            primary!(","),
            crate::connection::OpenAppSettings,
            Some(crate::connection::SHELL_CONTEXT),
        ),
    ]);
    if cfg!(target_os = "macos") {
        cx.bind_keys([
            KeyBinding::new("cmd-w", CloseWindow, None),
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-h", Hide, None),
            KeyBinding::new("alt-cmd-h", HideOthers, None),
            KeyBinding::new("cmd-m", Minimize, None),
        ]);
    }
}

/// The macOS menu bar. Edit commands reach the focused text field through
/// the input's own actions.
pub fn install_menus(cx: &mut App) {
    if !cfg!(target_os = "macos") {
        return;
    }
    use gpui_kit::base::input;
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &CloseWindow, cx| cx.hide());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &Minimize, cx| with_active_window(cx, |window, _| window.minimize_window()));
    cx.on_action(|_: &Zoom, cx| with_active_window(cx, |window, _| window.zoom_window()));
    cx.on_action(|_: &About, cx| {
        if let Some(window) = cx.windows().into_iter().next() {
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, cx| open_about(window, cx));
            });
        }
    });
    cx.set_menus(vec![
        Menu::new("else.bot").items([
            MenuItem::action("About else.bot", About),
            MenuItem::separator(),
            MenuItem::action("Settings…", crate::connection::OpenAppSettings),
            MenuItem::action("Bot details", ToggleSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide else.bot", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit else.bot", Quit),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", input::Undo, OsAction::Undo),
            MenuItem::os_action("Redo", input::Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", input::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", input::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", input::Paste, OsAction::Paste),
            MenuItem::os_action("Select All", input::SelectAll, OsAction::SelectAll),
        ]),
        Menu::new("Bot").items([
            MenuItem::action("New Bot", NewBot),
            MenuItem::action("New Category", NewCategory),
            MenuItem::action("Move to Category…", MoveBot),
            MenuItem::separator(),
            MenuItem::action("Load Older Messages", LoadOlder),
            MenuItem::action("Stop Work", StopWork),
            MenuItem::separator(),
            MenuItem::action("Delete Bot…", DeleteBot),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
            MenuItem::separator(),
            MenuItem::action("Close Window", CloseWindow),
        ]),
    ]);
}

/// Deferred because a key binding dispatches while its window is updating.
fn with_active_window(cx: &mut App, f: fn(&mut Window, &mut App)) {
    if let Some(window) = cx.active_window() {
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| f(window, cx));
        });
    }
}

fn open_about(window: &mut Window, cx: &mut App) {
    use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
    cx.activate(true);
    window.activate_window();
    if window.has_active_dialog(cx) {
        return;
    }
    window.open_dialog(cx, |dialog, _, cx| {
        let theme = cx.theme();
        dialog.w(px(300.)).close_button(false).child(
            v_flex()
                .items_center()
                .gap_3()
                .py_4()
                .child(crate::connection::brand_mark(theme))
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("else.bot"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                ),
        )
    });
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

#[derive(Debug, Clone)]
struct PendingSend {
    request_id: Uuid,
    text: String,
    parts: Vec<crate::attachments::MessagePart>,
    sent_at: DateTime<Utc>,
    provider_id: Option<String>,
    reaction: Option<Reaction>,
    open_draft: Option<crate::model::DraftAttachment>,
    unknown: Option<String>,
    rejected: Option<String>,
}

pub struct ElseApp {
    client: Arc<Client>,
    connection: Connection,
    caps: Option<Capabilities>,
    bots: Vec<Bot>,
    mention_bots: Arc<bot_mention::MentionRegistry>,
    categories: Vec<Category>,
    default_bot_id: Option<Uuid>,
    selected: Option<Uuid>,
    view: Option<ConversationView>,
    composer: Entity<TextareaState>,
    clipboard_read: Option<Task<()>>,
    mentions: mentions::MentionMenu,
    drafts: HashMap<Uuid, Draft>,
    pending: HashMap<Uuid, Vec<PendingSend>>,
    stopping: HashSet<Uuid>,
    inbox: crate::inbox::Inbox,
    scope: Uuid,
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
    moving: HashSet<Uuid>,
    category_notice: Option<String>,
    preferences: crate::preferences::Preferences,
    _save_preferences: Option<Task<()>>,
    settings_open: bool,
    details_tab: computer::DetailsTab,
    computer: Option<Entity<computer::ComputerView>>,
    image_viewer: Option<Entity<images::ImageViewer>>,
    _computer_layout: Option<Subscription>,
    settings: HashMap<Uuid, BotSettings>,
    sidebar_focus: FocusHandle,
    live: Option<crate::api::LiveCommands>,
    _live: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ElseApp {
    pub(crate) fn social_network_available(&self) -> bool {
        self.connection == Connection::Connected
    }

    pub(crate) fn social_context(&self) -> (Arc<Client>, Vec<Bot>) {
        (
            self.client.clone(),
            self.bots
                .iter()
                .filter(|b| b.is_active())
                .cloned()
                .collect(),
        )
    }

    pub fn new(
        client: Arc<Client>,
        preferences: crate::preferences::Preferences,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sidebar_focus = cx.focus_handle();
        sidebar_focus.focus(window, cx);
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Message…")
                .auto_grow(1, 8)
                .submit_on_enter(true)
        });
        let subscription = cx.subscribe_in(
            &composer,
            window,
            |app, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => app.composer_changed(cx),
                InputEvent::PressEnter { shift: false, .. } => app.send(window, cx),
                InputEvent::Focus | InputEvent::Blur => app.sync_mentions(window, cx),
                _ => {}
            },
        );
        let activation = cx.observe_window_activation(window, |app, window, cx| {
            if window.is_window_active() && app.view.is_some() {
                app.select(app.selected, window, cx);
            }
            app.check_reading(window, cx);
        });
        let composer_observer = cx.observe_in(&composer, window, |app, _, window, cx| {
            app.sync_mentions(window, cx);
        });
        let visibility = cx
            .observe_window_visibility(window, |app, _, window, cx| app.check_reading(window, cx));
        let bounds =
            cx.observe_window_bounds(window, |app, window, cx| app.check_reading(window, cx));
        let mut app = Self {
            client,
            connection: Connection::Connecting,
            caps: None,
            bots: Vec::new(),
            mention_bots: Default::default(),
            categories: Vec::new(),
            default_bot_id: None,
            selected: None,
            view: None,
            composer,
            clipboard_read: None,
            mentions: Default::default(),
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
            preferences,
            _save_preferences: None,
            settings_open: false,
            details_tab: Default::default(),
            computer: None,
            image_viewer: None,
            _computer_layout: None,
            settings: HashMap::new(),
            sidebar_focus,
            live: None,
            _live: None,
            _subscriptions: vec![
                subscription,
                composer_observer,
                activation,
                visibility,
                bounds,
            ],
        };
        app.start_live(window, cx);
        app
    }

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
                self.sync_social_forms(window, cx);
                if let Some(view) = self.view.as_mut() {
                    view.stream_problem = None;
                    if view.conversation.is_none() {
                        self.load_conversation(window, cx);
                    }
                }
                self.resolve_unknown_sends(window, cx);
                self.resume_reads(cx);
            }
            LiveEvent::Disconnected(reason) => {
                self.connection = Connection::Disconnected(reason.clone());
                if let Some(view) = self.view.as_mut() {
                    view.stream_problem = Some(format!("Reconnecting… {reason}"));
                }
                self.clear_secret_forms(window, cx);
                self.clear_social_forms(window, cx);
                self.sync_secret_forms(window, cx);
            }
            LiveEvent::Unauthorized => {
                self.connection =
                    Connection::InvalidCredentials(ClientError::Unauthorized.to_string());
                self.clear_secret_forms(window, cx);
                self.clear_social_forms(window, cx);
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
                self.clear_social_forms(window, cx);
                self.sync_secret_forms(window, cx);
            }
            Err(_) => {}
        }
        cx.notify();
    }

    pub(super) fn refresh_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let mention_bots = bot_mention::MentionRegistry::new(&self.bots);
        if *self.mention_bots != mention_bots {
            self.mention_bots = Arc::new(mention_bots);
        }
        self.categories = list.categories;
        self.stopping.retain(|id| {
            self.bots
                .iter()
                .any(|b| b.id == *id && b.activity.as_ref().is_some_and(|a| a.phase != "idle"))
        });
        self.observe_replies(window, cx);
        self.reconcile_selection(window, cx);
        self.sync_settings(window, cx);
        self.sync_mentions(window, cx);
        self.sync_rows(window, cx);
        cx.notify();
    }

    fn active_bots(&self) -> impl Iterator<Item = &Bot> {
        self.bots.iter().filter(|bot| bot.is_active())
    }

    fn reconcile_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = |id: Option<Uuid>| id.filter(|id| self.active_bots().any(|b| b.id == *id));
        let next = active(self.selected)
            .or(active(self.default_bot_id))
            .or_else(|| self.active_bots().next().map(|b| b.id));
        if next != self.selected || (next.is_some() && self.view.is_none()) {
            self.select(next, window, cx);
        }
    }

    pub(crate) fn open_bot(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.inbox.open(id);
        self.select(Some(id), window, cx);
        self.check_reading(window, cx);
    }

    pub(crate) fn selected_bot_id(&self) -> Option<Uuid> {
        self.selected
    }

    fn select(&mut self, id: Option<Uuid>, window: &mut Window, cx: &mut Context<Self>) {
        let unread_since = id.and_then(|id| {
            self.bots
                .iter()
                .find(|bot| bot.id == id && self.inbox.is_unread(bot))
                .map(|bot| self.inbox.seen_through(bot))
        });
        if id == self.selected
            && let Some(view) = self.view.as_mut()
        {
            if view.unread_since.is_none() && unread_since.is_some() {
                view.unread_since = unread_since;
                self.sync_rows(window, cx);
            }
            return;
        }
        self.image_viewer = None;
        if let Some(previous) = self.selected {
            self.drafts.entry(previous).or_default().content = self.composer.read(cx).content();
        }
        self.computer = None;
        self._computer_layout = None;
        self.selected = id;
        self.mentions = Default::default();
        let draft = id
            .and_then(|id| self.drafts.get(&id))
            .map(|draft| draft.content.clone())
            .unwrap_or_else(|| gpui_kit::component::input::InputContent::new(""));
        let placeholder = match self.selected_bot() {
            Some(bot) => format!("Message {}…", bot.name),
            None => "Message…".to_owned(),
        };
        self.composer.update(cx, |state, cx| {
            state.set_value(draft, window, cx);
            state.set_placeholder(placeholder, window, cx);
        });
        self.notice = None;
        if let Some(old) = self.view.as_ref() {
            self.live_command(LiveCommand::Unsubscribe { bot: old.bot_id });
        }
        self.clear_secret_forms(window, cx);
        self.clear_social_forms(window, cx);
        self.view = id.map(|id| {
            let mut view = ConversationView::new(id);
            view.unread_since = unread_since;
            view
        });
        if let Some(view) = &self.view {
            let this = cx.entity().downgrade();
            view.list.set_scroll_handler(move |_, window, cx| {
                let _ = this.update(cx, |app, cx| app.check_reading(window, cx));
            });
        }
        self.ensure_settings(window, cx);
        if id.is_some() {
            self.sync_rows(window, cx);
            self.load_conversation(window, cx);
        }
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self
            .sidebar_layout()
            .visible_order(&self.preferences.collapsed);
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

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bot) = self.selected else { return };
        let draft = self.drafts.entry(bot).or_default();
        draft.content = self.composer.read(cx).content();
        let Some(message) = draft.message() else {
            return;
        };
        let mut parts = match attachments::parts(&draft.content, &draft.files) {
            Ok(parts) => parts,
            Err(error) => {
                self.notice = Some(error);
                cx.notify();
                return;
            }
        };
        if !parts.is_empty() {
            let prefix = message.len() - draft.content.text().len();
            if prefix > 0 {
                parts.insert(
                    0,
                    crate::attachments::MessagePart::Text {
                        text: message[..prefix].to_owned(),
                    },
                );
            }
        }
        if !self.enqueue(bot, message, parts, None, window, cx) {
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
        parts: Vec<crate::attachments::MessagePart>,
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
            parts,
            sent_at: Utc::now(),
            provider_id: None,
            reaction,
            open_draft: if reaction.is_none() {
                self.outgoing_attachment(cx)
            } else {
                None
            },
            unknown: None,
            rejected: None,
        };
        self.notice = None;
        self.pending.entry(bot).or_default().push(pending.clone());
        self.sync_rows(window, cx);
        self.submit(bot, pending, window, cx);
        true
    }

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
                    crate::api::SendRequest {
                        request_id: pending.request_id,
                        text: pending.text.clone(),
                        provider_id: pending.provider_id.clone(),
                        reaction: pending.reaction,
                        parts: pending.parts.clone(),
                        open_draft: pending.open_draft.clone(),
                    },
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

    fn resolve_unknown_sends(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        for (bot, list) in &self.pending {
            for pending in list.iter().filter(|p| p.unknown.is_some()) {
                let (client, bot, request_id) = (client.clone(), *bot, pending.request_id);
                cx.spawn_in(window, async move |this, cx| {
                    let found = client.by_request(bot, request_id).await;
                    let _ = this.update_in(cx, |app, window, cx| match found {
                        Ok(Some(sent)) => app.accept_sent(bot, sent, window, cx),
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
            let current = self.composer.read(cx).content();
            let restored = crate::attachments::input_content(&pending.text, &pending.parts);
            for part in &pending.parts {
                if let crate::attachments::MessagePart::File { file } = part {
                    self.drafts
                        .entry(bot)
                        .or_default()
                        .files
                        .insert(file.id, attachments::DraftFile::ready(file.clone()));
                }
            }
            let content = attachments::append(current, restored);
            self.composer
                .update(cx, |state, cx| state.set_value(content, window, cx));
            self.composer.read(cx).focus_handle(cx).focus(window, cx);
        }
        self.sync_rows(window, cx);
    }

    fn stop_work(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (client, Some(bot)) = (self.client.clone(), self.selected) else {
            return;
        };
        self.stopping.insert(bot);
        cx.notify();
        let request_id = Uuid::new_v4();
        cx.spawn_in(window, async move |this, cx| {
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
}
