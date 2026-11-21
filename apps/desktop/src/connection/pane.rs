//! Server settings panes as entities: each owns its client and poll, and
//! asks the shell for what only the shell can do through [`PaneEvent`].

use super::*;
use settings::SettingsSection;

/// What a pane asks of the shell.
pub(super) enum PaneEvent {
    /// Leave settings for the chat.
    Leave,
    /// Leave settings for this bot's chat.
    OpenBot(Uuid),
    /// Replace the pane with a fresh one, as a retry after a failed load.
    Reload,
}

/// A pane and the shell's subscription to its events.
pub(super) struct Mounted<V> {
    pub view: Entity<V>,
    _events: Subscription,
}

impl Shell {
    pub(super) fn mount<V: EventEmitter<PaneEvent>>(
        &self,
        view: Entity<V>,
        section: SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Mounted<V> {
        let owner = self.active.as_ref().map(|active| active.id);
        let events = cx.subscribe_in(&view, window, move |shell, _, event, window, cx| {
            if shell.active.as_ref().map(|active| active.id) == owner {
                shell.pane_event(event, section, window, cx);
            }
        });
        Mounted {
            view,
            _events: events,
        }
    }

    fn pane_event(
        &mut self,
        event: &PaneEvent,
        section: SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PaneEvent::Leave => {
                window.close_dialog(cx);
                self.cancel(window, cx);
            }
            PaneEvent::OpenBot(bot) => {
                let app = self.active.as_ref().map(|active| active.app.clone());
                window.close_dialog(cx);
                self.cancel(window, cx);
                if let Some(app) = app {
                    app.update(cx, |app, cx| app.open_bot(*bot, window, cx));
                }
            }
            PaneEvent::Reload => self.reload_pane(section, window, cx),
        }
    }
}

/// The settings pane that is open besides the two kept warm. Panes that
/// hold typed secrets wipe them in [`Pane::close`], which runs whenever a
/// pane is replaced through [`super::Settings::show`].
#[derive(Default)]
pub(super) enum Pane {
    #[default]
    None,
    Mcp(Mounted<mcp::View>),
    Skills(Mounted<skills::View>),
    Vault(Entity<vault::View>),
    Usage(Mounted<usage::View>),
    Computers(Mounted<computers::View>),
    Socials(Entity<crate::socials::SocialsView>),
}

impl Pane {
    pub(super) fn section(&self) -> Option<SettingsSection> {
        Some(match self {
            Self::None => return None,
            Self::Mcp(_) => SettingsSection::Mcp,
            Self::Skills(_) => SettingsSection::Skills,
            Self::Vault(_) => SettingsSection::Vault,
            Self::Usage(_) => SettingsSection::Usage,
            Self::Computers(_) => SettingsSection::Computers,
            Self::Socials(_) => SettingsSection::Socials,
        })
    }

    pub(super) fn close(self, window: &mut Window, cx: &mut App) {
        match self {
            Self::Skills(pane) => pane.view.update(cx, |view, cx| view.clear_keys(window, cx)),
            Self::Vault(view) => view.update(cx, |view, cx| view.close(window, cx)),
            Self::Socials(view) => view.update(cx, |view, cx| view.clear(window, cx)),
            Self::None | Self::Mcp(_) | Self::Usage(_) | Self::Computers(_) => {}
        }
    }

    pub(super) fn has_unsaved_work(&self, cx: &App) -> bool {
        match self {
            Self::Mcp(pane) => pane.view.read(cx).has_unsaved_work(),
            Self::Skills(pane) => pane.view.read(cx).has_unsaved_work(cx),
            Self::Vault(view) => view.read(cx).has_unsaved_work(),
            Self::Socials(view) => view.read(cx).has_unsaved_work(),
            Self::None | Self::Usage(_) | Self::Computers(_) => false,
        }
    }

    /// The pane's page, which fills the settings column.
    pub(super) fn element(&self) -> AnyElement {
        match self {
            Self::None => div().into_any_element(),
            Self::Mcp(pane) => pane.view.clone().into_any_element(),
            Self::Skills(pane) => pane.view.clone().into_any_element(),
            Self::Usage(pane) => pane.view.clone().into_any_element(),
            Self::Computers(pane) => pane.view.clone().into_any_element(),
            Self::Vault(view) => div().child(view.clone()).into_any_element(),
            Self::Socials(view) => div().child(view.clone()).into_any_element(),
        }
    }
}

impl Shell {
    /// A fresh pane for `section`, or [`Pane::None`] for a section without
    /// one or without a server.
    pub(super) fn new_pane(
        &self,
        section: SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Pane {
        let Some(active) = &self.active else {
            return Pane::None;
        };
        let (client, app) = (active.client.clone(), active.app.clone());
        match section {
            SettingsSection::Mcp => {
                let view = cx.new(|cx| mcp::View::new(client, window, cx));
                Pane::Mcp(self.mount(view, section, window, cx))
            }
            SettingsSection::Skills => {
                let connected = app.read(cx).connected();
                let view = cx.new(|cx| skills::View::new(client, connected, window, cx));
                Pane::Skills(self.mount(view, section, window, cx))
            }
            SettingsSection::Vault => {
                Pane::Vault(cx.new(|cx| vault::View::new(client, window, cx)))
            }
            SettingsSection::Usage => {
                let view = cx.new(|cx| usage::View::new(client, app, window, cx));
                Pane::Usage(self.mount(view, section, window, cx))
            }
            SettingsSection::Computers => {
                let view = cx.new(|cx| computers::View::new(client, app, window, cx));
                Pane::Computers(self.mount(view, section, window, cx))
            }
            SettingsSection::Socials => {
                let (client, available) = {
                    let app = app.read(cx);
                    (app.social_client(), app.social_network_available())
                };
                let view = cx.new(|cx| crate::socials::SocialsView::new(client, None, window, cx));
                if !available {
                    view.update(cx, |view, cx| view.set_available(false, window, cx));
                }
                Pane::Socials(view)
            }
            SettingsSection::Server
            | SettingsSection::Email
            | SettingsSection::Backups
            | SettingsSection::Updates => Pane::None,
        }
    }

    /// Replaces a pane with a fresh one, as its retry after a failed load.
    pub(super) fn reload_pane(
        &mut self,
        section: SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if section == SettingsSection::Backups {
            self.open_backups(window, cx);
            return;
        }
        let pane = self.new_pane(section, window, cx);
        if let Some(settings) = self.front.settings_mut() {
            settings.show(pane, window, cx);
        }
        cx.notify();
    }
}
