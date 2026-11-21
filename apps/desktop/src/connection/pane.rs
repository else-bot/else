//! Server settings panes as entities: each owns its client and poll, and
//! asks the shell for what only the shell can do through [`PaneEvent`].

use super::*;

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
        section: settings::SettingsSection,
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
        section: settings::SettingsSection,
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
