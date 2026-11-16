mod model;
mod view;

use super::*;
use crate::api::{Computer, ComputerDesiredState, ComputerStatus, ComputerUpgrade, Computers};
use crate::controls::{Control as _, Scale, Tier};
use crate::usage_ui::Loaded;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::h_flex;
use model::{Action, action, gb, ordinal, queue_active, queue_line, summary, upgrade_count};
use std::time::Duration;

const REFRESH: Duration = Duration::from_secs(3);

enum Availability {
    Available(Computers),
    Unavailable,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Upgrade(Uuid),
    UpgradeAll,
    CancelRemaining,
}

pub(super) struct View {
    id: Uuid,
    state: Loaded<Availability>,
    pending: Option<Command>,
    error: Option<(Command, String)>,
    _poll: Task<()>,
}

impl Shell {
    pub(super) fn open_computers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = &self.active else { return };
        let client = active.client.clone();
        let id = Uuid::new_v4();
        let poll = cx.spawn_in(window, async move |this, cx| {
            while let Ok(Some(attempt)) = this.update(cx, |shell, _| shell.begin_computers(id)) {
                let result = client.clone().computers().await;
                let open = this.update_in(cx, |shell, _, cx| {
                    shell.apply_computers(id, attempt, result, cx)
                });
                if !matches!(open, Ok(true)) {
                    break;
                }
                cx.background_executor().timer(REFRESH).await;
            }
        });
        self.computers = Some(View {
            id,
            state: Loaded::Loading(Uuid::nil()),
            pending: None,
            error: None,
            _poll: poll,
        });
        cx.notify();
    }

    fn begin_computers(&mut self, id: Uuid) -> Option<Uuid> {
        Some(
            self.computers
                .as_mut()
                .filter(|view| view.id == id)?
                .state
                .begin(),
        )
    }

    fn apply_computers(
        &mut self,
        id: Uuid,
        attempt: Uuid,
        result: Result<Computers, ClientError>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(view) = self.computers.as_mut().filter(|view| view.id == id) else {
            return false;
        };
        let result = match result {
            Ok(report) => Ok(Availability::Available(report)),
            Err(error)
                if error.code() == Some("computers_unavailable")
                    && !matches!(
                        view.state,
                        Loaded::Ready {
                            value: Availability::Available(_),
                            ..
                        }
                    ) =>
            {
                Ok(Availability::Unavailable)
            }
            Err(error) => Err(error.to_string()),
        };
        if view.state.settle(attempt, result) {
            cx.notify();
        }
        true
    }

    fn refresh_computers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(id)) = (
            self.active.as_ref().map(|active| active.client.clone()),
            self.computers.as_ref().map(|view| view.id),
        ) else {
            return;
        };
        let Some(attempt) = self.begin_computers(id) else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = client.computers().await;
            this.update_in(cx, |shell, _, cx| {
                shell.apply_computers(id, attempt, result, cx);
            })
            .ok();
        })
        .detach();
    }

    fn computer_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(active), Some(view)) = (&self.active, &mut self.computers) else {
            return;
        };
        if view.pending.is_some() {
            return;
        }
        let id = view.id;
        let client = active.client.clone();
        view.pending = Some(command);
        view.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = match command {
                Command::Upgrade(bot) => client.upgrade_computer(bot).await,
                Command::UpgradeAll => client.upgrade_all_computers().await.map(|_| ()),
                Command::CancelRemaining => client.cancel_computer_upgrades().await.map(|_| ()),
            };
            this.update_in(cx, |shell, window, cx| {
                let Some(view) = shell.computers.as_mut().filter(|view| view.id == id) else {
                    return;
                };
                view.pending = None;
                view.error = result.err().map(|error| (command, error.to_string()));
                shell.refresh_computers(window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_computer_bot(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let app = self.active.as_ref().map(|active| active.app.clone());
        window.close_dialog(cx);
        self.cancel(window, cx);
        if let Some(app) = app {
            app.update(cx, |app, cx| app.open_bot(bot, window, cx));
        }
    }

    fn computer_name(&self, id: Uuid, cx: &App) -> String {
        self.known_bots(cx)
            .iter()
            .find(|bot| bot.id == id)
            .map(|bot| bot.name.clone())
            .unwrap_or_else(|| "Unknown bot".into())
    }
}
