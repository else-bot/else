mod model;
mod view;

use super::loaded::Loaded;
use super::pane::PaneEvent;
use super::*;
use crate::controls::{Control as _, Scale, Tier};
use else_wire::computers::Computers;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::h_flex;
use model::{
    Action, action, gb, image_label, ordinal, queue_active, queue_line, summary, upgrade_count,
};
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
    client: Arc<Client>,
    app: Entity<ElseApp>,
    state: Loaded<Availability>,
    pending: Option<Command>,
    error: Option<(Command, String)>,
    _poll: Task<()>,
}

impl EventEmitter<PaneEvent> for View {}

impl View {
    pub(in crate::connection) fn new(
        client: Arc<Client>,
        app: Entity<ElseApp>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let poll_client = client.clone();
        let poll = cx.spawn_in(window, async move |this, cx| {
            while let Ok(attempt) = this.update(cx, |view, _| view.state.begin()) {
                let result = poll_client.clone().computers().await;
                if this
                    .update(cx, |view, cx| view.apply(attempt, result, cx))
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(REFRESH).await;
            }
        });
        Self {
            client,
            app,
            state: Loaded::default(),
            pending: None,
            error: None,
            _poll: poll,
        }
    }

    fn apply(
        &mut self,
        attempt: Uuid,
        result: Result<Computers, ClientError>,
        cx: &mut Context<Self>,
    ) {
        let result = match result {
            Ok(report) => Ok(Availability::Available(report)),
            Err(error)
                if error.code() == Some("computers_unavailable")
                    && !matches!(self.state.value(), Some(Availability::Available(_))) =>
            {
                Ok(Availability::Unavailable)
            }
            Err(error) => Err(error.to_string()),
        };
        if self.state.settle(attempt, result) {
            cx.notify();
        }
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let attempt = self.state.begin();
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.computers().await;
            this.update(cx, |view, cx| view.apply(attempt, result, cx))
                .ok();
        })
        .detach();
    }

    fn command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        let client = self.client.clone();
        self.pending = Some(command);
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = match command {
                Command::Upgrade(bot) => client.upgrade_computer(bot).await,
                Command::UpgradeAll => client.upgrade_all_computers().await.map(|_| ()),
                Command::CancelRemaining => client.cancel_computer_upgrades().await.map(|_| ()),
            };
            this.update_in(cx, |view, window, cx| {
                view.pending = None;
                view.error = result.err().map(|error| (command, error.to_string()));
                view.refresh(window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn computer_name(&self, id: Uuid, cx: &App) -> String {
        self.app
            .read(cx)
            .bots()
            .iter()
            .find(|bot| bot.id == id)
            .map(|bot| bot.name.clone())
            .unwrap_or_else(|| "Unknown bot".into())
    }
}

impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_page(cx)
    }
}
