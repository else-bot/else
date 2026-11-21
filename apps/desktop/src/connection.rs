//! The window's root: which server the desktop uses, and the setup screens
//! that choose it. Saved servers own immutable clients and messaging sessions.
//! Switching selects a session without rebinding requests or discarding drafts.

mod backups;
mod computers;
mod connect;
mod icon_editor;
mod loaded;
mod mcp;
mod pane;
mod server_keys;
mod server_rail;
mod server_views;
mod servers;
mod settings;
mod shared_icons;
mod skills;
mod switcher;
mod switching;
mod updates;
mod usage;
mod vault;
mod view;
pub(crate) use switcher::bind_keys;
pub(crate) use view::brand_mark;

use connect::{Check, ConnectForm, Page, RemoteForm, verify};
use settings::{ServerPage, Settings, SettingsSection};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, TitleBar, WindowExt as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::{Client, ClientError};
use crate::app::ElseApp;
use crate::config::{Origin, Profile, ServerRegistry};
use crate::launch::{
    KeyState, Launch, Limits, LocalServer, NoLocal, Phase, RemoteSeed, Step, this_machine,
};

const STORY_MIN_WIDTH: f32 = 760.;

pub(crate) const SHELL_CONTEXT: &str = "AppShell";

actions!(
    else_desktop,
    [
        OpenAppSettings,
        OpenEmailSettings,
        OpenSkillsSettings,
        OpenUsageSettings
    ]
);

struct Active {
    id: Uuid,
    profile: Profile,
    client: Arc<Client>,
    app: Entity<ElseApp>,
    /// Cached so a background session redraws the rail only when its
    /// badges change.
    attention: Option<crate::attention::Attention>,
    /// Tracked on the element that holds the app, so the shell can tell
    /// whether focus is inside this session.
    pane: FocusHandle,
    /// Where focus was when the session went to the background. A hidden
    /// session's elements are not drawn, so focus left there would stop
    /// key and modifier events from reaching the shell.
    last_focus: Option<FocusHandle>,
    _app: Subscription,
}

/// What to open once a server switch lands.
struct Arrival {
    server: Uuid,
    open: Open,
}

enum Open {
    /// A system notification's bot, found by its session scope.
    Notification {
        scope: Uuid,
        bot: Uuid,
    },
    Bot(Uuid),
    /// The server itself: focus its bot list.
    Server,
}

/// What the window shows in front of the server sessions.
enum Front {
    /// No usable server yet: the full-window setup.
    Onboarding(Box<ConnectForm>),
    /// The active server's chat.
    Session,
    /// The settings sheet over the active server's chat.
    Settings(Box<Settings>),
}

impl Front {
    fn form(&self) -> Option<&ConnectForm> {
        match self {
            Self::Onboarding(form) => Some(form),
            Self::Settings(settings) => settings.servers().form(),
            Self::Session => None,
        }
    }

    fn form_mut(&mut self) -> Option<&mut ConnectForm> {
        match self {
            Self::Onboarding(form) => Some(form),
            Self::Settings(settings) => settings.form_mut(),
            Self::Session => None,
        }
    }

    fn settings(&self) -> Option<&Settings> {
        match self {
            Self::Settings(settings) => Some(settings),
            _ => None,
        }
    }

    fn settings_mut(&mut self) -> Option<&mut Settings> {
        match self {
            Self::Settings(settings) => Some(settings),
            _ => None,
        }
    }
}

pub struct Shell {
    launch: Launch,
    key: Option<KeyState>,
    runtime: tokio::runtime::Handle,
    active: Option<Active>,
    registry: ServerRegistry,
    inactive: HashMap<Uuid, Active>,
    servers: servers::Connections,
    rail: server_rail::Rail,
    hold: server_keys::Hold,
    _server_keys: [Subscription; 3],
    switch: switching::Switch<Task<()>>,
    arrival: Option<Arrival>,
    /// Opens a hovered server's session so a click lands warm.
    _prefetch: Option<(Uuid, Task<()>)>,
    connection_error: Option<String>,
    names: servers::Writes<servers::NameWrite>,
    icon_writes: servers::Writes<shared_icons::IconWrite>,
    /// Servers whose picture is downloading.
    icon_fetches: HashSet<Uuid>,
    server_updates: updates::View,
    remote_updates: HashMap<Uuid, updates::View>,
    front: Front,
    settings_focus: FocusHandle,
    updates: updates::View,
    switcher: Option<switcher::Switcher>,
    recall: switcher::Recall,
}

impl Shell {
    /// Connect with the saved profile; without one (the first launch) or
    /// with an unusable one, open the welcome screen.
    pub fn new(
        launch: Launch,
        runtime: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut launch = launch;
        let loaded = launch.store.clone().and_then(|store| store.load());
        if let Err(error) = &loaded {
            launch.store = Err(error.clone());
        }
        let registry = loaded.as_ref().cloned().unwrap_or_default();
        let selected = registry
            .selected
            .and_then(|id| registry.find(id))
            .map(|saved| saved.profile.clone());
        // The platform keeps one handler for the whole process.
        let this = cx.weak_entity();
        let handle = window.window_handle();
        crate::system_notifications::on_response(cx, move |response, cx| {
            let this = this.clone();
            let _ = handle.update(cx, |_, window, cx| {
                let Some((scope, bot)) = crate::inbox::parse_tag(&response.tag) else {
                    return;
                };
                let _ = this.update(cx, |shell, cx| {
                    shell.open_notification(scope, bot, window, cx)
                });
            });
        });
        let key = launch
            .local
            .as_ref()
            .ok()
            .map(|local| LocalServer::key_state(&local.key()));
        let updates = updates::View::new(&launch, &runtime, window, cx);
        let server_updates = updates::View::new_server(&launch, &runtime, window, cx);
        let servers = servers::Connections::new(&registry, &runtime, window, cx);
        let (rail, rail_error) = server_rail::Rail::load(&launch.store);
        let this = cx.weak_entity();
        let shell_window = window.window_handle();
        let settings_focus = cx.focus_handle();
        let server_keys = [
            cx.intercept_keystrokes(move |event, window, cx| {
                if window.window_handle() == shell_window {
                    let _ = this.update(cx, |shell, cx| {
                        shell.server_key(&event.keystroke, window, cx)
                    });
                }
            }),
            cx.observe_window_activation(window, |shell, window, cx| {
                if !window.is_window_active() {
                    shell.move_hold(server_keys::Hold::blurred, cx);
                }
            }),
            cx.on_focus_in(&settings_focus, window, |shell, window, cx| {
                let modifiers = window.modifiers();
                shell.move_hold(|hold| hold.refocused(modifiers), cx);
            }),
        ];
        let recall = switcher::Recall::load(&launch.store);
        let mut shell = Self {
            launch,
            key,
            runtime,
            active: None,
            registry,
            inactive: HashMap::new(),
            servers,
            rail,
            hold: server_keys::Hold::Idle,
            _server_keys: server_keys,
            switch: switching::Switch::Settled,
            arrival: None,
            _prefetch: None,
            connection_error: loaded.err().or(rail_error),
            server_updates,
            remote_updates: HashMap::new(),
            names: Default::default(),
            icon_writes: Default::default(),
            icon_fetches: HashSet::new(),
            front: Front::Session,
            settings_focus,
            updates,
            switcher: None,
            recall,
        };
        shell.sync_server_updates(window, cx);
        match selected {
            Some(profile)
                if shell
                    .launch
                    .local
                    .as_ref()
                    .is_ok_and(|local| local.owns(&profile)) =>
            {
                shell.open_setup(None, window, cx);
                shell.start_local(window, cx);
            }
            Some(profile) => {
                match Client::new(
                    profile.origin.clone(),
                    &profile.token,
                    shell.runtime.clone(),
                ) {
                    Ok(client) => shell.activate(profile, Arc::new(client), window, cx),
                    Err(err) => {
                        shell.open_setup(Some(err), window, cx);
                        shell.open_remote(Some(&profile), window, cx);
                    }
                }
            }
            None => shell.open_first(shell.connection_error.clone(), window, cx),
        }
        shell
    }

    fn open_first(&mut self, error: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.open_setup(error, window, cx);
        if self.launch.local.is_err() && self.launch.remote_seed.is_some() {
            self.open_remote(None, window, cx);
        }
    }

    fn open_setup(&mut self, error: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let form = ConnectForm::new(error, focus, window, cx);
        self.set_front(Front::Onboarding(Box::new(form)), window, cx);
        cx.notify();
    }

    /// Replaces what the window shows. Leaving settings closes its panes,
    /// so the ones holding typed secrets wipe them.
    fn set_front(&mut self, front: Front, window: &mut Window, cx: &mut App) {
        match std::mem::replace(&mut self.front, front) {
            Front::Settings(mut settings) => settings.clear(window, cx),
            Front::Onboarding(form) => form.close(window, cx),
            Front::Session => {}
        }
    }

    fn activate(
        &mut self,
        profile: Profile,
        client: Arc<Client>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self.registry.servers.iter().find(|s| s.profile == profile) else {
            return;
        };
        let id = saved.id;
        let widths = self
            .active
            .as_ref()
            .map(|active| active.app.read(cx).panel_widths());
        if let Some(settings) = self.front.settings_mut() {
            settings.clear(window, cx);
        }
        if let Some(mut previous) = self.active.take() {
            previous.last_focus = window
                .focused(cx)
                .filter(|focused| previous.pane.contains(focused, window));
            previous.app.update(cx, |app, cx| {
                app.dismiss_notifications(cx);
                app.set_hidden(true, window, cx);
            });
            self.inactive.insert(previous.id, previous);
        }
        if let Some(mut cached) = self.inactive.remove(&id).filter(|s| s.profile == profile) {
            let hidden = self.settings_section().is_some();
            cached.app.update(cx, |app, cx| {
                if let Some(widths) = widths {
                    app.set_panel_widths(widths, cx);
                }
                app.set_hidden(hidden, window, cx);
            });
            let last_focus = cached.last_focus.take();
            if !hidden {
                last_focus
                    .unwrap_or_else(|| cached.app.read(cx).sidebar_focus_handle())
                    .focus(window, cx);
            }
            self.active = Some(cached);
            cx.notify();
            return;
        }
        let session = self.open_session(id, profile, client, window, cx);
        if let Some(widths) = widths {
            session
                .app
                .update(cx, |app, cx| app.set_panel_widths(widths, cx));
        }
        self.active = Some(session);
        cx.notify();
    }

    /// Opens a server's session: its app connects and loads on its own.
    fn open_session(
        &mut self,
        id: Uuid,
        profile: Profile,
        client: Arc<Client>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Active {
        let preferences = self
            .launch
            .store
            .as_ref()
            .map(|store| {
                crate::preferences::Preferences::load_for_server(
                    store,
                    id,
                    self.registry.legacy_server == Some(id),
                )
            })
            .unwrap_or_default();
        let saved = self.launch.store.as_ref().ok().map(|store| {
            crate::saved_conversations::SavedConversations::new(
                &store.saved_conversations_dir(id),
                profile.origin.as_str(),
            )
        });
        let app = cx.new(|cx| ElseApp::new(client.clone(), preferences, saved, window, cx));
        let observer = cx.observe_in(&app, window, move |shell, app, window, cx| {
            let attention = app.read(cx).attention();
            let session = match shell.active.as_mut() {
                Some(active) if active.id == id => Some(active),
                _ => shell.inactive.get_mut(&id),
            };
            if let Some(session) = session
                && session.attention != attention
            {
                session.attention = attention;
                cx.notify();
            }
            shell.cache_session_bots(id, &app, cx);
            if shell.active.as_ref().is_some_and(|active| active.id == id) {
                if app.read(cx).connected() && shell.switch.recheck(id) {
                    shell.retry_switch(window, cx);
                }
                shell.reclaim_switcher_focus(window, cx);
                if let Some(bot) = app.read(cx).selected_bot_id() {
                    shell.remember(crate::quick_switcher::Target::Bot { server: id, bot }, cx);
                }
                shell.connection_changed(app.read(cx).connected(), window, cx);
                if let Some(pane::Pane::Socials(socials)) =
                    shell.front.settings().map(Settings::pane)
                {
                    let available = app.read(cx).social_network_available();
                    socials.update(cx, |view, cx| view.set_available(available, window, cx));
                }
                cx.notify();
            }
        });
        Active {
            id,
            profile,
            client,
            app,
            attention: None,
            pane: cx.focus_handle(),
            last_focus: None,
            _app: observer,
        }
    }

    /// Opens a saved server's session in the background, so switching to
    /// it shows a loaded list. A server this desktop starts waits for a
    /// click, which starts it.
    fn prefetch(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if self.active.as_ref().is_some_and(|active| active.id == id)
            || self.inactive.contains_key(&id)
        {
            return;
        }
        let Some(saved) = self.registry.find(id).cloned() else {
            return;
        };
        if self.managed_server(&saved.profile) {
            return;
        }
        let Ok(client) = Client::new(
            saved.profile.origin.clone(),
            &saved.profile.token,
            self.runtime.clone(),
        ) else {
            return;
        };
        let focused = window.focused(cx);
        let session = self.open_session(id, saved.profile, Arc::new(client), window, cx);
        session
            .app
            .update(cx, |app, cx| app.set_hidden(true, window, cx));
        // A new app takes focus, and a hidden one would swallow keys.
        focused
            .unwrap_or_else(|| self.settings_focus.clone())
            .focus(window, cx);
        self.inactive.insert(id, session);
    }

    fn open_notification(
        &mut self,
        scope: uuid::Uuid,
        bot: uuid::Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .active
            .as_ref()
            .is_none_or(|active| active.app.read(cx).notification_scope() != scope)
        {
            if let Some(id) = self
                .inactive
                .values()
                .find(|session| session.app.read(cx).notification_scope() == scope)
                .map(|session| session.id)
            {
                self.arrival = Some(Arrival {
                    server: id,
                    open: Open::Notification { scope, bot },
                });
                self.select_server(id, window, cx);
            }
            return;
        }
        let Some(active) = &self.active else { return };
        let app = active.app.clone();
        if !app.update(cx, |app, cx| app.open_notification(scope, bot, window, cx)) {
            return;
        }
        cx.activate(true);
        window.activate_window();
        if !matches!(self.front, Front::Session) {
            if self.settings_section().is_some() {
                window.close_dialog(cx);
            }
            self.cancel(window, cx);
        }
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(settings) = self.front.settings_mut() {
            settings.show(pane::Pane::None, window, cx);
        }
        if let Some(active) = &self.active {
            let app = active.app.clone();
            self.updates.dismiss();
            self.server_updates.dismiss();
            self.set_front(Front::Session, window, cx);
            app.update(cx, |app, cx| app.set_hidden(false, window, cx));
            cx.notify();
        }
    }
}

/// Leaves the settings sheet through [`Shell::cancel`], which wipes what
/// its panes typed. macOS hides the window instead of closing it, so a
/// sheet left open would come back with its secrets still typed.
pub(crate) fn close_settings(window: &mut Window, cx: &mut App) {
    let Some(shell) = window
        .root::<gpui_kit::base::Root>()
        .flatten()
        .and_then(|root| root.read(cx).view().clone().downcast::<Shell>().ok())
    else {
        return;
    };
    shell.update(cx, |shell, cx| {
        if shell.settings_section().is_some() {
            window.close_all_dialogs(cx);
            shell.cancel(window, cx);
        }
    });
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let root =
            v_flex()
                .key_context(SHELL_CONTEXT)
                .on_modifiers_changed(cx.listener(|shell, event: &ModifiersChangedEvent, _, cx| {
                    shell.modifiers_changed(event.modifiers, cx)
                }))
                .relative()
                .on_action(
                    cx.listener(|shell, _: &switcher::ToggleQuickSwitcher, window, cx| {
                        shell.toggle_switcher(window, cx)
                    }),
                )
                .on_action(cx.listener(|shell, _: &OpenAppSettings, window, cx| {
                    shell.open_settings(window, cx)
                }))
                .on_action(cx.listener(|shell, _: &OpenSkillsSettings, window, cx| {
                    if window.has_active_dialog(cx) {
                        return;
                    }
                    shell.open_settings(window, cx);
                    shell.select_settings(SettingsSection::Skills, window, cx);
                }))
                .on_action(cx.listener(|shell, _: &OpenUsageSettings, window, cx| {
                    if window.has_active_dialog(cx) {
                        return;
                    }
                    let bot = shell
                        .active
                        .as_ref()
                        .and_then(|active| active.app.read(cx).selected_bot_id());
                    shell.open_settings(window, cx);
                    shell.select_settings(SettingsSection::Usage, window, cx);
                    shell.focus_usage(bot, cx);
                }))
                .on_action(cx.listener(|shell, _: &OpenEmailSettings, window, cx| {
                    if window.has_active_dialog(cx) {
                        return;
                    }
                    shell.open_settings(window, cx);
                    shell.open_email_from_chat(window, cx);
                }))
                .size_full()
                .bg(theme.background)
                .text_color(theme.foreground);
        if let Front::Onboarding(setup) = &self.front {
            let wide = window.viewport_size().width >= px(STORY_MIN_WIDTH);
            let start_on_enter = matches!(setup.page, Page::Welcome) && self.launch.local.is_ok();
            // macOS still needs a title bar to drag the window by.
            return root
                .when(cfg!(target_os = "macos"), |root| {
                    root.child(TitleBar::new())
                })
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .track_focus(&setup.focus)
                        .when(start_on_enter, |panel| {
                            panel.on_key_down(cx.listener(
                                |shell, event: &KeyDownEvent, window, cx| {
                                    if event.keystroke.key == "enter" {
                                        shell.start_local(window, cx);
                                    }
                                },
                            ))
                        })
                        .child(self.render_setup(setup, wide, cx)),
                );
        }
        let Some(active) = &self.active else {
            return root;
        };
        // The quick switcher owns the keys while open.
        let commands = self.switcher.is_none() && active.app.read(cx).takes_commands(cx);
        // macOS draws its traffic lights over a title bar the app owns.
        root.when(commands, |root| crate::app::on_commands(root, &active.app))
            .track_focus(&self.settings_focus)
            .when(cfg!(target_os = "macos"), |root| {
                root.child(TitleBar::new())
            })
            .when_some(
                self.connection_error
                    .clone()
                    .filter(|_| self.settings_section().is_none()),
                |root, error| {
                    root.child(
                        div()
                            .px_3()
                            .py_2()
                            .text_xs()
                            .text_color(theme.danger)
                            .child(error),
                    )
                },
            )
            .children(
                self.render_switch_failure(cx)
                    .filter(|_| self.settings_section().is_none()),
            )
            .child(self.render_with_rail(active.app.clone().into(), &active.pane, window, cx))
            .children(self.render_switcher(cx))
    }
}
