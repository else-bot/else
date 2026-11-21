mod identity;
pub(super) mod model;
pub(super) mod routines;
mod schedule_text;
mod triggers;
mod watchers;

use super::panel::{hairline, list_rows, muted, section};
use super::view::HEADER_HEIGHT;
use super::{APP_CONTEXT, Connection, ElseApp, ToggleSettings};
use crate::api::Bot;
use crate::controls::{segment, segment_track};
use crate::model::Conversation;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, IconName, Sizable as _, h_flex, v_flex,
};
use gpui_kit::*;
use identity::Details;
use model::ModelState;
use routines::RoutineSave;
use std::collections::HashMap;
use triggers::TriggerSave;
use uuid::Uuid;
use watchers::WatcherStop;

pub(super) struct BotSettings {
    details: Details,
    /// The bot's `metadata_revision` when its profile was last loaded: a
    /// bot can change another bot's model, which publishes `bot.updated`.
    revision: Option<i64>,
    model: ModelState,
    routines: HashMap<Uuid, RoutineSave>,
    /// Trigger changes by `TriggerSource::key`.
    triggers: HashMap<Uuid, TriggerSave>,
    watcher_stops: HashMap<Uuid, WatcherStop>,
    open_watcher_log: Option<Uuid>,
}

impl ElseApp {
    fn automations(&self, bot: Uuid) -> Option<&Conversation> {
        self.view
            .as_ref()
            .filter(|view| view.bot_id == bot)
            .and_then(|view| view.conversation.as_ref())
    }

    fn apply_routine(&mut self, bot: Uuid, saved: crate::model::Routine) {
        if let Some(conversation) = self
            .view
            .as_mut()
            .filter(|view| view.bot_id == bot)
            .and_then(|view| view.conversation.as_mut())
        {
            conversation.apply_routine(saved);
        }
    }

    pub(super) fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open.toggle();
        if self.settings_open {
            if let Some(bot) = self.selected.filter(|id| {
                self.settings
                    .get(id)
                    .is_some_and(|entry| !entry.model.pending())
            }) {
                self.load_profile(bot, window, cx);
            }
        } else {
            self.composer.read(cx).focus_handle(cx).focus(window, cx);
        }
        if !self.settings_open {
            self.computer = None;
            self._computer_layout = None;
        }
        self.ensure_settings(window, cx);
        cx.notify();
    }

    pub(super) fn settings_shown(&self) -> bool {
        self.settings_open && self.selected_bot().is_some_and(|b| b.is_active())
    }

    pub(super) fn ensure_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bot) = self.selected_bot().filter(|b| b.is_active()).cloned() else {
            return;
        };
        if self.settings_open
            && self.details_tab == super::computer::DetailsTab::Computer
            && self.computer.is_none()
        {
            let client = self.client.clone();
            let computer =
                cx.new(|cx| super::computer::ComputerView::new(client, &bot, window, cx));
            self._computer_layout = Some(cx.subscribe_in(
                &computer,
                window,
                |app, _, _: &super::computer::LayoutChanged, window, cx| {
                    app.check_reading(window, cx)
                },
            ));
            self.computer = Some(computer);
        }
        if self.settings.contains_key(&bot.id) {
            return;
        }
        let details = Details::new(&bot, window, cx);
        self.settings.insert(
            bot.id,
            BotSettings {
                details,
                revision: bot.metadata_revision,
                model: ModelState::Loading(Uuid::nil()),
                routines: HashMap::new(),
                triggers: HashMap::new(),
                watcher_stops: HashMap::new(),
                open_watcher_log: None,
            },
        );
        self.load_profile(bot.id, window, cx);
    }

    pub(super) fn sync_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let bots = &self.bots;
        self.settings
            .retain(|id, _| bots.iter().any(|b| b.id == *id && b.is_active()));
        let mut stale = Vec::new();
        for bot in &self.bots {
            if let Some(entry) = self.settings.get_mut(&bot.id) {
                entry.details.sync_metadata(bot);
                if entry.revision != bot.metadata_revision {
                    entry.revision = bot.metadata_revision;
                    stale.push(bot.id);
                }
            }
        }
        for bot in stale {
            self.load_profile(bot, window, cx);
        }
        self.ensure_settings(window, cx);
    }

    pub(super) fn sync_settings_changes(&mut self) {
        if let Some(view) = &self.view
            && let Some(conversation) = &view.conversation
            && let Some(entry) = self.settings.get_mut(&view.bot_id)
        {
            entry.sync_watchers(conversation.watchers());
            entry.sync_triggers(conversation);
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
        let close = Button::new("close-settings")
            .focus_ring(false)
            .cursor_pointer()
            .icon(IconName::PanelRightClose)
            .small()
            .ghost()
            .accessibility_label("Hide bot details")
            .tooltip_with_action("Hide bot details", &ToggleSettings, Some(APP_CONTEXT))
            .on_click(cx.listener(|app, _, window, cx| app.toggle_settings(window, cx)));
        // The padding keeps the first and last rows clear of the edge fades
        // at rest.
        let mut body = v_flex()
            .id("bot-settings-body")
            .flex_1()
            .min_h_0()
            .pt_2()
            .pb_6()
            .overflow_y_scroll();
        if let Some(computer) = &self.computer {
            body = body.child(v_flex().flex_1().p_4().child(computer.clone()));
        }
        if self.details_tab == super::computer::DetailsTab::Details {
            if let Some(entry) = self.settings.get(&bot.id) {
                body = body
                    .child(self.render_bot_usage(cx))
                    .child(self.render_model(bot.id, &entry.model, cx))
                    .child(self.render_rules_section(bot.id, cx))
                    .child(self.render_routines(bot.id, &entry.routines, window, cx))
                    .child(self.render_reminders(bot.id, &entry.routines, window, cx))
                    .children(self.render_watchers(bot.id, entry, cx))
                    .child(self.render_triggers(bot.id, entry, cx));
            } else {
                body = body.child(muted("Loading…", cx));
            }
        }
        // Both tabs open on the bot itself: its avatar and name, where both
        // are edited, so the Computer tab is anchored the same way.
        let identity = self
            .settings
            .get(&bot.id)
            .map(|entry| self.render_identity(bot.id, &entry.details, cx));
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
                    .pr_2()
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
                                    self.details_tab == super::computer::DetailsTab::Details,
                                    false,
                                    cx,
                                ))
                                .on_click(cx.listener(
                                    |app, _, _, cx| {
                                        app.details_tab = super::computer::DetailsTab::Details;
                                        app.computer = None;
                                        app._computer_layout = None;
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                crate::panel_kit::hugging(segment(
                                    "computer-tab",
                                    "Computer",
                                    self.details_tab == super::computer::DetailsTab::Computer,
                                    false,
                                    cx,
                                ))
                                .on_click(cx.listener(
                                    |app, _, window, cx| {
                                        app.details_tab = super::computer::DetailsTab::Computer;
                                        app.ensure_settings(window, cx);
                                        cx.notify();
                                    },
                                )),
                            ),
                    )
                    .child(div().flex_1())
                    .child(div().flex_none().child(close)),
            )
            .children(identity)
            .child(
                // Rows dissolve under the tabs and the footer instead of
                // being sliced at the rule.
                div().flex_1().min_h_0().child(crate::panel_kit::faded(
                    body,
                    theme.sidebar,
                    px(0.),
                )),
            )
            .child(self.sidebar_handle(super::layout::SidebarEdge::Right, cx));
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
