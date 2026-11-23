mod identity;
pub(super) mod model;
pub(super) mod routines;
mod triggers;
mod watchers;

use super::computer::{ComputerView, DetailsTab, LayoutChanged};
use super::panel::{hairline, list_rows, muted, section};
use super::rules::{RulesSummary, RulesTab};
use super::view::{HEADER_END_PADDING, HEADER_HEIGHT};
use super::{Connection, ElseApp};
use crate::api::Client;
use crate::controls::{segment, segment_track};
use else_client::request::Request;
use else_wire::bots::Bot;
use else_wire::conversation::BotExtras;
use else_wire::routines::Routine;
use else_wire::settings::Capabilities;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, FocusableExt as _, h_flex, v_flex};
use gpui_kit::*;
use identity::Details;
use model::ModelState;
use routines::RoutineChange;
use std::collections::HashMap;
use std::sync::Arc;
use triggers::TriggerChange;
use uuid::Uuid;
use watchers::WatcherStop;

/// One bot's details: its identity, model and automations. It keeps its
/// requests while the bot is active, so switching away and back keeps a
/// save in flight. `ElseApp` pushes what it shares through [`PanelInputs`]
/// and handles what reaches past the panel through [`PanelEvent`].
pub(super) struct BotPanel {
    bot: Uuid,
    client: Arc<Client>,
    /// For a rejected owner token, which outlives the panel's bot.
    app: WeakEntity<ElseApp>,
    inputs: PanelInputs,
    details: Details,
    /// The bot's `metadata_revision` when its profile was last loaded: a
    /// bot can change another bot's model, which publishes `bot.updated`.
    revision: Option<i64>,
    model: ModelState,
    /// The one-click switch to the newer model of the bot's family.
    model_upgrade: Request<()>,
    routines: HashMap<Uuid, Request<RoutineChange>>,
    /// Trigger changes by `TriggerSource::key`.
    triggers: HashMap<Uuid, Request<TriggerChange>>,
    watcher_stops: HashMap<Uuid, WatcherStop>,
    open_watcher_log: Option<Uuid>,
    /// Routines whose banner shows its whole reason, with the reason it was
    /// opened on, so a new reason starts folded.
    expanded_routine_problems: HashMap<Uuid, String>,
    watcher_clock: Option<Task<()>>,
    /// The panel's width inside its border.
    width: f32,
}

/// What the panel reads from the window.
#[derive(Clone, Default, PartialEq)]
pub(super) struct PanelInputs {
    pub(super) connected: bool,
    pub(super) caps: Option<Arc<Capabilities>>,
    /// The bot's routines, watchers and triggers, while its conversation
    /// is open.
    pub(super) automations: Option<Arc<BotExtras>>,
    pub(super) rules: RulesSummary,
    /// The tab on screen, while the panel shows this bot.
    pub(super) shown: Option<DetailsTab>,
    pub(super) computer: Option<Entity<ComputerView>>,
    /// The panel's width inside its border, once the window was laid out.
    pub(super) width: Option<f32>,
}

/// What the panel asks of the window.
pub(super) enum PanelEvent {
    /// A change response, for the bot's conversation.
    RoutineSaved(Box<Routine>),
    ChangeModel,
    ChangeRoutineModel(Uuid),
    OpenRules,
    OpenUsage,
}

impl EventEmitter<PanelEvent> for BotPanel {}

/// A bot's panel and its event subscription.
pub(super) struct PanelSlot {
    pub(super) panel: Entity<BotPanel>,
    _events: Subscription,
}

/// The bot details panel. Its tab outlives closing it; the computer view
/// exists only while the panel is open on the Computer tab.
pub(super) enum DetailsPanel {
    Closed {
        tab: DetailsTab,
    },
    Open {
        tab: DetailsTab,
        computer: Option<ComputerPane>,
    },
}

pub(super) struct ComputerPane {
    pub(super) view: Entity<ComputerView>,
    _layout: Subscription,
}

impl Default for DetailsPanel {
    fn default() -> Self {
        Self::Closed {
            tab: DetailsTab::default(),
        }
    }
}

impl DetailsPanel {
    pub(super) fn open(&self) -> bool {
        matches!(self, Self::Open { .. })
    }

    pub(super) fn tab(&self) -> DetailsTab {
        match self {
            Self::Closed { tab } | Self::Open { tab, .. } => *tab,
        }
    }

    pub(super) fn computer(&self) -> Option<&Entity<ComputerView>> {
        match self {
            Self::Open {
                computer: Some(pane),
                ..
            } => Some(&pane.view),
            _ => None,
        }
    }

    /// Drops the computer view; showing the tab again builds it anew.
    pub(super) fn drop_computer(&mut self) {
        if let Self::Open { computer, .. } = self {
            *computer = None;
        }
    }

    fn toggle(&mut self) {
        let tab = self.tab();
        *self = match self {
            Self::Closed { .. } => Self::Open {
                tab,
                computer: None,
            },
            Self::Open { .. } => Self::Closed { tab },
        };
    }

    pub(super) fn select(&mut self, next: DetailsTab) {
        match self {
            Self::Closed { tab } => *tab = next,
            Self::Open { tab, computer } => {
                *tab = next;
                if next != DetailsTab::Computer {
                    *computer = None;
                }
            }
        }
    }
}

impl BotPanel {
    fn new(
        bot: &Bot,
        client: Arc<Client>,
        app: WeakEntity<ElseApp>,
        inputs: PanelInputs,
        width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self {
            bot: bot.id,
            client,
            app,
            inputs: PanelInputs::default(),
            details: Details::new(bot, window, cx),
            revision: bot.metadata_revision,
            model: ModelState::Loading(Uuid::nil()),
            model_upgrade: Request::Idle,
            routines: HashMap::new(),
            triggers: HashMap::new(),
            watcher_stops: HashMap::new(),
            open_watcher_log: None,
            expanded_routine_problems: HashMap::new(),
            watcher_clock: None,
            width,
        };
        panel.load_profile(window, cx);
        panel.set_inputs(inputs, cx);
        panel
    }

    fn set_inputs(&mut self, inputs: PanelInputs, cx: &mut Context<Self>) {
        if let Some(automations) = &inputs.automations {
            self.sync_watchers(&automations.watchers);
            self.sync_triggers(automations);
        }
        if let Some(width) = inputs.width {
            self.width = width;
        }
        self.inputs = inputs;
        self.sync_watcher_clock(cx);
        cx.notify();
    }

    /// A newer bot list: the name and avatar, and the profile when another
    /// bot changed it.
    fn sync_bot(&mut self, bot: &Bot, window: &mut Window, cx: &mut Context<Self>) {
        self.details.sync_metadata(bot);
        if self.revision != bot.metadata_revision {
            self.revision = bot.metadata_revision;
            self.load_profile(window, cx);
        }
        cx.notify();
    }

    /// Opening the panel reads the profile again, unless a read is running.
    fn reopened(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.pending() {
            self.load_profile(window, cx);
        }
    }

    fn connected(&self) -> bool {
        self.inputs.connected
    }

    fn caps(&self) -> Option<&Capabilities> {
        self.inputs.caps.as_deref()
    }

    fn automations(&self) -> Option<&BotExtras> {
        self.inputs.automations.as_deref()
    }

    pub(super) fn rules(&self) -> &RulesSummary {
        &self.inputs.rules
    }
}

impl Render for BotPanel {
    /// The bot's identity over the shown tab's body.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // The padding keeps the first and last rows clear of the edge fades
        // at rest.
        let mut body = v_flex()
            .id("bot-settings-body")
            .flex_1()
            .min_h_0()
            .pt_2()
            .pb_6()
            .overflow_y_scroll();
        if let Some(computer) = &self.inputs.computer {
            body = body.child(v_flex().flex_1().p_4().child(computer.clone()));
        }
        if self.inputs.shown == Some(DetailsTab::Details) {
            body = body
                .child(self.render_bot_usage(cx))
                .child(self.render_model(cx))
                .child(self.render_rules_section(cx))
                .child(self.render_routines(window, cx))
                .child(self.render_reminders(window, cx))
                .children(self.render_watchers(cx))
                .child(self.render_triggers(cx));
        }
        // Both tabs open on the bot itself: its avatar and name, where both
        // are edited, so the Computer tab is anchored the same way.
        v_flex().size_full().child(self.render_identity(cx)).child(
            // Rows dissolve under the tabs and the footer instead of
            // being sliced at the rule.
            div()
                .flex_1()
                .min_h_0()
                .child(crate::panel_kit::faded(body, theme.sidebar, px(0.))),
        )
    }
}

impl ElseApp {
    fn apply_routine(&mut self, bot: Uuid, saved: Routine, cx: &mut App) {
        if let Some(view) = self.view_of(bot, cx) {
            view.update(cx, |view, cx| view.apply_routine(saved, cx));
        }
    }

    pub(super) fn panel(&self, bot: Uuid) -> Option<&Entity<BotPanel>> {
        self.panels.get(&bot).map(|slot| &slot.panel)
    }

    pub(super) fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.details.toggle();
        if self.details.open() {
            if let Some(panel) = self.selected.and_then(|id| self.panel(id)).cloned() {
                panel.update(cx, |panel, cx| panel.reopened(window, cx));
            }
        } else {
            self.composer.read(cx).focus_handle(cx).focus(window, cx);
        }
        self.ensure_settings(window, cx);
        cx.notify();
    }

    pub(super) fn settings_shown(&self) -> bool {
        self.details.open() && self.selected_bot().is_some_and(|b| b.is_active())
    }

    pub(super) fn ensure_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bot) = self.selected_bot().filter(|b| b.is_active()).cloned() else {
            return;
        };
        if let DetailsPanel::Open {
            tab: DetailsTab::Computer,
            computer: computer @ None,
        } = &mut self.details
        {
            let client = self.client.clone();
            let view = cx.new(|cx| ComputerView::new(client, &bot, window, cx));
            let layout = cx.subscribe_in(&view, window, |app, _, _: &LayoutChanged, window, cx| {
                app.check_reading(window, cx)
            });
            *computer = Some(ComputerPane {
                view,
                _layout: layout,
            });
        }
        if self.panels.contains_key(&bot.id) {
            return;
        }
        let client = self.client.clone();
        let inputs = self.panel_inputs(bot.id, cx);
        let width = self.details_panel_width(window) - 1.;
        let app = cx.entity().downgrade();
        let panel = cx.new(|cx| BotPanel::new(&bot, client, app, inputs, width, window, cx));
        let id = bot.id;
        let events = cx.subscribe_in(&panel, window, move |app, _, event, window, cx| {
            app.on_panel_event(id, event, window, cx)
        });
        self.panels.insert(
            bot.id,
            PanelSlot {
                panel,
                _events: events,
            },
        );
    }

    fn on_panel_event(
        &mut self,
        bot: Uuid,
        event: &PanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PanelEvent::RoutineSaved(saved) => {
                self.apply_routine(bot, (**saved).clone(), cx);
                cx.notify();
            }
            PanelEvent::ChangeModel => self.open_model_dialog(bot, window, cx),
            PanelEvent::ChangeRoutineModel(routine) => {
                self.open_routine_model_dialog(bot, *routine, window, cx)
            }
            PanelEvent::OpenRules => self.open_rules(bot, None, RulesTab::Rules, None, window, cx),
            PanelEvent::OpenUsage => {
                self.sidebar_focus.focus(window, cx);
                window.dispatch_action(Box::new(crate::connection::OpenUsageSettings), cx);
            }
        }
    }

    pub(super) fn sync_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let bots = &self.bots;
        self.panels
            .retain(|id, _| bots.iter().any(|b| b.id == *id && b.is_active()));
        for bot in self.bots.iter() {
            if let Some(slot) = self.panels.get(&bot.id) {
                slot.panel
                    .update(cx, |panel, cx| panel.sync_bot(bot, window, cx));
            }
        }
        self.ensure_settings(window, cx);
    }

    /// The panel's width inside its 1 px border, once the window was laid
    /// out.
    fn details_body_width(&self) -> Option<f32> {
        self.width
            .map(|viewport| self.details_panel_width_in(viewport) - 1.)
    }

    fn panel_inputs(&self, bot: Uuid, cx: &App) -> PanelInputs {
        let shown =
            (self.selected == Some(bot) && self.settings_shown()).then(|| self.details.tab());
        let automations = self
            .shown(cx)
            .filter(|view| view.bot_id == bot)
            .and_then(|view| view.conversation.as_ref())
            .map(|conversation| conversation.extras().clone());
        PanelInputs {
            connected: self.connection == Connection::Connected,
            caps: self.caps.clone(),
            automations,
            rules: self.rules_summary(bot),
            shown,
            computer: shown.and(self.details.computer().cloned()),
            width: self.details_body_width(),
        }
    }

    /// Pushes what changed to each panel. It runs after each notify of the
    /// window, so a change to what the panels read must notify it; a notify
    /// during drawing calls it directly.
    pub(super) fn sync_panel_inputs(&mut self, cx: &mut Context<Self>) {
        for (&bot, slot) in &self.panels {
            let inputs = self.panel_inputs(bot, cx);
            if slot.panel.read(cx).inputs != inputs {
                slot.panel
                    .update(cx, |panel, cx| panel.set_inputs(inputs, cx));
            }
        }
    }

    pub(super) fn render_settings(
        &self,
        bot: &Bot,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let line = hairline(cx);
        let mut panel = v_flex()
            .id("bot-settings")
            .relative()
            .w(px(self.details_panel_width(window)))
            .on_drag_move(cx.listener(Self::resize_details))
            .flex_none()
            .h_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_l_1()
            .border_color(line)
            .child(
                // The tabs are the panel's header: same height and rule as the
                // conversation header, so the two dividers meet as one line.
                h_flex()
                    .flex_none()
                    .h(HEADER_HEIGHT)
                    .pl_4()
                    .pr(HEADER_END_PADDING)
                    .items_center()
                    .border_b_1()
                    .border_color(line)
                    .gap_2()
                    .child(
                        segment_track(cx)
                            .child(
                                crate::panel_kit::hugging(segment(
                                    "details-tab",
                                    "Details",
                                    self.details.tab() == DetailsTab::Details,
                                    false,
                                    cx,
                                ))
                                .on_click(cx.listener(
                                    |app, _, _, cx| {
                                        app.details.select(DetailsTab::Details);
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                crate::panel_kit::hugging(segment(
                                    "computer-tab",
                                    "Computer",
                                    self.details.tab() == DetailsTab::Computer,
                                    false,
                                    cx,
                                ))
                                .on_click(cx.listener(
                                    |app, _, window, cx| {
                                        app.details.select(DetailsTab::Computer);
                                        app.ensure_settings(window, cx);
                                        cx.notify();
                                    },
                                )),
                            ),
                    )
                    .child(div().flex_1())
                    .child(self.header_actions(cx)),
            );
        panel = match self.panel(bot.id) {
            Some(details) => panel.child(div().flex_1().min_h_0().child(details.clone())),
            None => panel.child(div().flex_1().min_h_0().child(muted("Loading…", cx))),
        };
        panel = panel.child(self.sidebar_handle(super::layout::SidebarEdge::Right, cx));
        if self.default_bot_id != Some(bot.id) {
            let id = bot.id;
            panel = panel.child(
                div()
                    .flex_none()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(line)
                    .child(
                        Button::new("delete-bot")
                            .focus_ring(false)
                            .cursor_pointer()
                            .icon(gpui_kit::assets::IconName::Trash)
                            .label("Delete bot")
                            .custom(
                                ButtonCustomVariant::new(cx)
                                    .foreground(theme.danger)
                                    .hover(theme.danger.opacity(0.08))
                                    .active(theme.danger.opacity(0.14)),
                            )
                            .w_full()
                            .disabled(
                                self.connection != Connection::Connected
                                    || self.deleting.contains(&id),
                            )
                            .on_click(cx.listener(move |app, _, window, cx| {
                                app.open_delete_dialog(id, window, cx)
                            })),
                    ),
            );
        }
        panel.into_any_element()
    }
}

fn automation_section(
    title: &'static str,
    empty: &'static str,
    rows: Option<Vec<AnyElement>>,
    cx: &App,
) -> AnyElement {
    let content = match rows {
        None => muted("Loading…", cx),
        Some(rows) if rows.is_empty() => muted(empty, cx),
        Some(rows) => list_rows(rows),
    };
    section(title, cx).child(content).into_any_element()
}
