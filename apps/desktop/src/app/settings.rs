use super::panel::{alert, hairline, item_name, list_rows, meta, meta_line, muted, section};

mod identity;
pub(super) mod model;

use model::ModelState;
mod schedule_text;
mod triggers;
mod watchers;

use triggers::TriggerSave;

use watchers::WatcherStop;

use identity::Details;

use schedule_text::{describe_cron, describe_hours, local_time};

use super::delete_dialog::DeleteTarget;
use super::view::HEADER_HEIGHT;
use super::{APP_CONTEXT, Connection, ElseApp, ToggleSettings};
use crate::controls::{Control as _, Scale, Tier, segment, segment_track, toggle};
use gpui_kit::component::FocusableExt as _;
use std::collections::HashMap;

use crate::api::Bot;
use crate::model::{LastRun, PausedReason, Routine, RunStatus, SkipReason};
use crate::profile_picker::effort_label;
use chrono::Utc;
use gpui_kit::base::HoverCard;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
enum RoutineSave {
    Saving { attempt: Uuid, enabled: bool },
    Quieting { attempt: Uuid, quiet: bool },
    Dismissing { attempt: Uuid },
    Retrying { attempt: Uuid },
    Model { attempt: Uuid },
    Deleting { attempt: Uuid },
    Failed(String),
}

impl RoutineSave {
    fn busy(&self) -> bool {
        matches!(
            self,
            Self::Saving { .. }
                | Self::Quieting { .. }
                | Self::Dismissing { .. }
                | Self::Retrying { .. }
                | Self::Model { .. }
                | Self::Deleting { .. }
        )
    }

    fn attempt(&self) -> Option<Uuid> {
        match self {
            Self::Saving { attempt, .. }
            | Self::Quieting { attempt, .. }
            | Self::Dismissing { attempt }
            | Self::Retrying { attempt }
            | Self::Model { attempt }
            | Self::Deleting { attempt } => Some(*attempt),
            Self::Failed(_) => None,
        }
    }
}

pub(super) struct BotSettings {
    details: Details,
    /// The bot's `metadata_revision` when its profile was last loaded: a
    /// bot can change another bot's model, which publishes `bot.updated`.
    revision: Option<i64>,
    model: ModelState,
    routines: HashMap<Uuid, RoutineSave>,
    /// Trigger changes by source ID.
    triggers: HashMap<Uuid, TriggerSave>,
    watcher_stops: HashMap<Uuid, WatcherStop>,
    open_watcher_log: Option<Uuid>,
}

impl ElseApp {
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

    fn set_routine(
        &mut self,
        bot: Uuid,
        routine: Uuid,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let attempt = Uuid::new_v4();
        let request = self
            .client
            .clone()
            .set_routine_enabled(bot, routine, enabled);
        let change = RoutineSave::Saving { attempt, enabled };
        self.save_routine(bot, routine, change, request, window, cx);
    }

    fn set_routine_quiet(
        &mut self,
        bot: Uuid,
        routine: Uuid,
        quiet: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let attempt = Uuid::new_v4();
        let request = self.client.clone().set_routine_quiet(bot, routine, quiet);
        let change = RoutineSave::Quieting { attempt, quiet };
        self.save_routine(bot, routine, change, request, window, cx);
    }

    fn dismiss_routine_failures(
        &mut self,
        bot: Uuid,
        routine: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let attempt = Uuid::new_v4();
        let request = self.client.clone().dismiss_routine_failures(bot, routine);
        let change = RoutineSave::Dismissing { attempt };
        self.save_routine(bot, routine, change, request, window, cx);
    }

    pub(super) fn retry_routine(
        &mut self,
        bot: Uuid,
        routine: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let attempt = Uuid::new_v4();
        let request = self.client.clone().retry_routine(bot, routine, attempt);
        let change = RoutineSave::Retrying { attempt };
        self.save_routine(bot, routine, change, request, window, cx);
    }

    fn save_routine(
        &mut self,
        bot: Uuid,
        routine: Uuid,
        change: RoutineSave,
        request: impl std::future::Future<Output = Result<Routine, crate::api::ClientError>> + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(attempt) = change.attempt() else {
            return;
        };
        let retrying = matches!(change, RoutineSave::Retrying { .. });
        if !self.begin_routine_change(bot, routine, change) {
            return;
        }
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |app, cx| {
                let Some(entry) = current_change(&mut app.settings, bot, routine, attempt) else {
                    return;
                };
                match result {
                    Ok(saved) => {
                        entry.routines.remove(&routine);
                        if let Some(conversation) = app
                            .view
                            .as_mut()
                            .filter(|view| view.bot_id == bot)
                            .and_then(|view| view.conversation.as_mut())
                        {
                            conversation.apply_routine(saved);
                        }
                    }
                    Err(err) => {
                        let text = if retrying {
                            retry_failure(&err)
                        } else {
                            format!("Couldn't save. {err}")
                        };
                        entry.routines.insert(routine, RoutineSave::Failed(text));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn changeable_routine(&self, bot: Uuid, routine: Uuid) -> Option<&Routine> {
        if self.connection != Connection::Connected {
            return None;
        }
        let entry = self.settings.get(&bot)?;
        if entry.routines.get(&routine).is_some_and(RoutineSave::busy) {
            return None;
        }
        self.view
            .as_ref()
            .filter(|view| view.bot_id == bot)
            .and_then(|view| view.conversation.as_ref())?
            .routines()
            .iter()
            .find(|r| r.id == routine)
    }

    fn begin_routine_change(&mut self, bot: Uuid, routine: Uuid, change: RoutineSave) -> bool {
        if self.changeable_routine(bot, routine).is_none() {
            return false;
        }
        let Some(entry) = self.settings.get_mut(&bot) else {
            return false;
        };
        entry.routines.insert(routine, change);
        true
    }

    fn open_routine_delete_dialog(
        &mut self,
        bot: Uuid,
        routine: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self
            .changeable_routine(bot, routine)
            .map(|r| match r.run_at {
                Some(_) => DeleteTarget::Reminder(r.name.clone()),
                None => DeleteTarget::Routine(r.name.clone()),
            })
        else {
            return;
        };
        self.open_delete_confirmation(target, window, cx, move |app, window, cx| {
            app.delete_routine(bot, routine, window, cx);
        });
    }

    fn delete_routine(
        &mut self,
        bot: Uuid,
        routine: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let attempt = Uuid::new_v4();
        if !self.begin_routine_change(bot, routine, RoutineSave::Deleting { attempt }) {
            return;
        }
        cx.notify();
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.delete_routine(bot, routine).await;
            let _ = this.update(cx, |app, cx| {
                let Some(entry) = current_change(&mut app.settings, bot, routine, attempt) else {
                    return;
                };
                match result {
                    Ok(()) => {
                        app.expanded_routine_problems.remove(&routine);
                    }
                    Err(err) => {
                        entry.routines.insert(
                            routine,
                            RoutineSave::Failed(format!("Couldn't delete. {err}")),
                        );
                        cx.notify();
                    }
                }
            });
        })
        .detach();
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

impl ElseApp {
    fn render_routines(
        &self,
        bot: Uuid,
        saves: &HashMap<Uuid, RoutineSave>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = section("Routines", cx);
        let Some(conversation) = self
            .view
            .as_ref()
            .filter(|view| view.bot_id == bot)
            .and_then(|view| view.conversation.as_ref())
        else {
            return body.child(muted("Loading…", cx)).into_any_element();
        };
        let routines: Vec<&Routine> = conversation
            .routines()
            .iter()
            .filter(|routine| routine.run_at.is_none())
            .collect();
        if routines.is_empty() {
            return body
                .child(muted(
                    "No routines yet. Ask the bot in chat to do something on a schedule, for \
                     example \"every weekday at 9, summarize my unread mail\". Its routines \
                     appear here.",
                    cx,
                ))
                .into_any_element();
        }
        let connected = self.connection == Connection::Connected;
        let rows = routines
            .into_iter()
            .map(|routine| {
                self.render_routine(bot, routine, saves.get(&routine.id), connected, window, cx)
            })
            .collect::<Vec<_>>();
        body.child(list_rows(rows)).into_any_element()
    }

    fn render_reminders(
        &self,
        bot: Uuid,
        saves: &HashMap<Uuid, RoutineSave>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = section("Reminders", cx);
        let Some(conversation) = self
            .view
            .as_ref()
            .filter(|view| view.bot_id == bot)
            .and_then(|view| view.conversation.as_ref())
        else {
            return body.child(muted("Loading…", cx)).into_any_element();
        };
        let mut reminders: Vec<&Routine> = conversation
            .routines()
            .iter()
            .filter(|routine| routine.run_at.is_some())
            .collect();
        if reminders.is_empty() {
            return body
                .child(muted(
                    "No reminders. Ask the bot to do something once, later, for example \
                     \"in 6 hours, check how Scout is doing\". It appears here until it runs.",
                    cx,
                ))
                .into_any_element();
        }
        reminders.sort_by_key(|reminder| (reminder.run_at, reminder.id));
        let connected = self.connection == Connection::Connected;
        let rows = reminders
            .into_iter()
            .map(|reminder| {
                self.render_routine(
                    bot,
                    reminder,
                    saves.get(&reminder.id),
                    connected,
                    window,
                    cx,
                )
            })
            .collect::<Vec<_>>();
        body.child(list_rows(rows)).into_any_element()
    }

    fn render_routine(
        &self,
        bot: Uuid,
        routine: &Routine,
        save: Option<&RoutineSave>,
        connected: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = routine.id;
        let busy = save.is_some_and(RoutineSave::busy);
        let checked = match save {
            Some(RoutineSave::Saving { enabled, .. }) => *enabled,
            _ => routine.enabled,
        };
        let passed = !checked && routine.run_at.is_some_and(|at| at <= Utc::now());
        let switch = toggle(
            ElementId::Name(format!("routine-{id}").into()),
            checked,
            busy || !connected || passed,
            cx,
        )
        .accessibility_label(format!("Run {}", routine.name))
        .on_click(cx.listener(move |app, enabled: &bool, window, cx| {
            app.set_routine(bot, id, *enabled, window, cx)
        }));
        let retrying = routine.retrying || matches!(save, Some(RoutineSave::Retrying { .. }));
        let dismiss = (routine.failed_runs > 0 && !retrying).then(|| {
            Button::new(ElementId::Name(format!("routine-dismiss-{id}").into()))
                .control(Tier::Ghost, Scale::Compact, busy || !connected, cx)
                .label("Dismiss")
                .loading(matches!(save, Some(RoutineSave::Dismissing { .. })))
                .accessibility_label(format!("Dismiss the failures of {}", routine.name))
                .on_click(cx.listener(move |app, _, window, cx| {
                    app.dismiss_routine_failures(bot, id, window, cx)
                }))
        });
        let retry = (retrying || retryable(routine)).then(|| {
            Button::new(ElementId::Name(format!("routine-retry-{id}").into()))
                .control(
                    Tier::Primary,
                    Scale::Compact,
                    busy || !connected || retrying,
                    cx,
                )
                .label("Retry")
                .loading(retrying)
                .accessibility_label(format!("Retry {} now", routine.name))
                .on_click(
                    cx.listener(move |app, _, window, cx| app.retry_routine(bot, id, window, cx)),
                )
        });
        // Separate tokens, so a narrow panel wraps between them. Only a
        // token wider than the row, like a long cron description, wraps
        // inside itself.
        let when = match (routine.run_at, &routine.active_hours) {
            (Some(at), _) => vec!["Once".to_owned(), local_time(at, &routine.timezone)],
            (None, Some(hours)) => vec![describe_cron(&routine.cron), describe_hours(hours)],
            (None, None) => vec![describe_cron(&routine.cron)],
        };
        let schedule = format!("{} · {}", when.join(" · "), routine.timezone);
        let last = last_run(routine);
        let choice = routine.model.as_ref().or_else(|| {
            self.settings
                .get(&bot)
                .and_then(|entry| entry.model.shown()?.as_ref())
                .map(|profile| &profile.choice)
        });
        let model_title = match (&routine.model, choice) {
            (Some(choice), _) => self.model_name(choice),
            (None, Some(choice)) => format!("Bot model · {}", self.model_name(choice)),
            (None, None) => "Bot model".to_owned(),
        };
        let model_details = choice
            .map(|choice| {
                let speed = if choice.service_tier == "priority" {
                    "Fast"
                } else {
                    "Standard"
                };
                format!(
                    "{} · {} reasoning · {speed}",
                    self.provider_label(&choice.provider_id),
                    effort_label(&choice.reasoning_effort)
                )
            })
            .unwrap_or_else(|| "No model yet. Choose a bot model.".to_owned());
        let unavailable = routine
            .model
            .as_ref()
            .is_some_and(|choice| !self.model_available(choice));
        let warning = routine
            .requested_model
            .as_ref()
            .map(|requested| format!("Couldn't use \"{requested}\". Using bot model."))
            .or_else(|| {
                unavailable.then(|| "Saved model is unavailable. Choose another model.".to_owned())
            });
        // The model is changed from the row's menu. The meta line names it
        // only when the routine overrides the bot model shown above.
        let quiet = match save {
            Some(RoutineSave::Quieting { quiet, .. }) => *quiet,
            _ => routine.quiet,
        };
        let mut tokens: Vec<AnyElement> = when
            .into_iter()
            .map(|token| meta(token, cx).min_w_0().into_any_element())
            .collect();
        tokens.push(
            meta(routine.timezone.clone(), cx)
                .flex_none()
                .whitespace_nowrap()
                .into_any_element(),
        );
        if let Some(choice) = routine.model.as_ref().filter(|_| routine.run_at.is_none()) {
            tokens.push(
                meta(self.model_name(choice), cx)
                    .flex_none()
                    .whitespace_nowrap()
                    .into_any_element(),
            );
        }
        if quiet && routine.run_at.is_none() {
            tokens.push(
                meta("Quiet".to_owned(), cx)
                    .flex_none()
                    .whitespace_nowrap()
                    .into_any_element(),
            );
        }
        let summary = div()
            .id(ElementId::Name(format!("routine-pill-{id}").into()))
            .w_full()
            .child(
                item_name(routine.name.clone())
                    .line_clamp(2)
                    .text_ellipsis(),
            );
        let model_line = routine
            .run_at
            .is_none()
            .then(|| format!("{model_title} · {model_details}"));
        let name = routine.name.clone();
        let prompt = routine.prompt.clone();
        let next = match (routine.enabled, routine.next_run_at) {
            (false, _) => None,
            _ if routine.run_at.is_some() => None,
            (true, Some(at)) => Some(format!("Next run {}", local_time(at, &routine.timezone))),
            (true, None) => Some("No further runs".to_owned()),
        };
        let description = HoverCard::new(ElementId::Name(format!("routine-card-{id}").into()))
            .w_full()
            .anchor(Anchor::RightCenter)
            .open_delay(Duration::from_millis(300))
            .trigger(summary)
            .content(move |_, _, cx| {
                let theme = cx.theme();
                let line =
                    |text: String, color: Hsla| div().text_xs().text_color(color).child(text);
                let card = v_flex()
                    .w(px(300.))
                    .min_w_0()
                    .p_3()
                    .gap_2()
                    .rounded(theme.radius_lg)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .whitespace_normal()
                            .child(name.clone()),
                    )
                    .child(
                        div()
                            .id("routine-card-prompt")
                            .max_h(px(240.))
                            .overflow_y_scroll()
                            .text_sm()
                            .whitespace_normal()
                            .text_color(theme.foreground)
                            .child(prompt.clone()),
                    )
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(line(schedule.clone(), theme.muted_foreground))
                            .children(
                                model_line
                                    .clone()
                                    .map(|text| line(text, theme.muted_foreground)),
                            )
                            .children(next.clone().map(|text| line(text, theme.muted_foreground)))
                            .children(last.clone().map(|(text, error)| {
                                line(
                                    text,
                                    if error {
                                        theme.danger
                                    } else {
                                        theme.muted_foreground
                                    },
                                )
                            })),
                    );
                let (border, background) = (theme.border, theme.popover);
                h_flex()
                    .id("routine-preview")
                    .items_center()
                    .pr_3()
                    .child(card)
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, _| {
                                let mid = bounds.center().y;
                                for (inset, color) in [(0., border), (1., background)] {
                                    let mut path = PathBuilder::fill();
                                    path.move_to(point(
                                        bounds.left() - px(1.),
                                        bounds.top() + px(inset),
                                    ));
                                    path.line_to(point(bounds.right() - px(inset), mid));
                                    path.line_to(point(
                                        bounds.left() - px(1.),
                                        bounds.bottom() - px(inset),
                                    ));
                                    path.close();
                                    if let Ok(path) = path.build() {
                                        window.paint_path(path, color);
                                    }
                                }
                            },
                        )
                        .w(px(8.))
                        .h(px(16.)),
                    )
            })
            .into_any_element();
        let theme = cx.theme().clone();
        let owner = cx.entity().downgrade();
        let reminder = routine.run_at.is_some();
        let no_models = self.caps.is_none();
        let (model_badge, model_hint) = match (&routine.model, choice) {
            (Some(choice), _) => (
                "Custom",
                Hint {
                    title: format!("{}, for this routine only", self.model_name(choice)).into(),
                    body: format!("{model_details}. The bot's chats keep their own model.").into(),
                },
            ),
            (None, Some(choice)) => (
                "Bot",
                Hint {
                    title: format!("{}, from the bot", self.model_name(choice)).into(),
                    body: format!(
                        "{model_details}. Pick another model for this routine only; \
                         the bot's chats keep theirs."
                    )
                    .into(),
                },
            ),
            (None, None) => (
                "Bot",
                Hint {
                    title: "No bot model yet".into(),
                    body: model_details.clone().into(),
                },
            ),
        };
        let quiet_hint = Hint {
            title: "Only post when there's news".into(),
            body: "The routine still runs on schedule. If nothing changed since the last run, \
                   it stays silent instead of saying so."
                .into(),
        };
        let danger = theme.danger;
        // Quiet, the model and Delete sit in one menu, so the name and the
        // schedule keep the row's width in a narrow panel. Each setting
        // explains itself in a card beside the menu.
        let actions_menu = Button::new(ElementId::Name(format!("routine-more-{id}").into()))
            .focus_ring(false)
            .cursor_pointer()
            .ghost()
            .xsmall()
            .icon(IconName::Ellipsis)
            .accessibility_label(format!("Actions for {}", routine.name))
            .loading(matches!(
                save,
                Some(
                    RoutineSave::Quieting { .. }
                        | RoutineSave::Model { .. }
                        | RoutineSave::Deleting { .. }
                )
            ))
            .disabled(busy || !connected)
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                let act = |f: fn(&mut Self, Uuid, Uuid, &mut Window, &mut Context<Self>)| {
                    let owner = owner.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let _ = owner.update(cx, |app, cx| f(app, bot, id, window, cx));
                    }
                };
                // Each item draws its own icon, so the hint card anchors at
                // the menu's edge; the quiet check goes on the right.
                let menu = menu.check_side(gpui_kit::component::Side::Right);
                let menu = if reminder {
                    menu
                } else {
                    let quiet_hint = quiet_hint.clone();
                    let model_hint = model_hint.clone();
                    menu.item(menu_heading("Delivery"))
                        .item(
                            PopupMenuItem::element(move |_, cx| {
                                hinted(
                                    format!("routine-quiet-hint-{id}"),
                                    gpui_kit::assets::IconName::BellOff,
                                    "Quiet if no news",
                                    None,
                                    &quiet_hint,
                                    cx,
                                )
                            })
                            .checked(quiet)
                            .on_click(if quiet {
                                act(|app, bot, id, window, cx| {
                                    app.set_routine_quiet(bot, id, false, window, cx)
                                })
                            } else {
                                act(|app, bot, id, window, cx| {
                                    app.set_routine_quiet(bot, id, true, window, cx)
                                })
                            }),
                        )
                        .item(menu_heading("Run with"))
                        .item(
                            PopupMenuItem::element(move |_, cx| {
                                hinted(
                                    format!("routine-model-hint-{id}"),
                                    gpui_kit::assets::IconName::Cpu,
                                    "Change model…",
                                    Some(model_badge),
                                    &model_hint,
                                    cx,
                                )
                            })
                            .disabled(no_models)
                            .on_click(act(
                                |app, bot, id, window, cx| {
                                    app.open_routine_model_dialog(bot, id, window, cx)
                                },
                            )),
                        )
                        .separator()
                };
                menu.item(
                    PopupMenuItem::element(move |_, _| {
                        h_flex()
                            .gap_2()
                            .text_color(danger)
                            .child(Icon::new(gpui_kit::assets::IconName::Trash).xsmall())
                            .child("Delete…")
                    })
                    .on_click(act(|app, bot, id, window, cx| {
                        app.open_routine_delete_dialog(bot, id, window, cx)
                    })),
                )
            });
        // The switch lines up with the name; the schedule below it gets the
        // row's whole width.
        let header = v_flex()
            .w_full()
            .gap_0p5()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_start()
                    .child(div().flex_1().min_w_0().child(description))
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .items_center()
                            .child(actions_menu)
                            .child(switch),
                    ),
            )
            .child(meta_line(tokens, cx));
        let banner = banner(routine, save);
        let shown = banner.as_ref().map_or("", Banner::text).to_owned();
        let problem = matches!(banner, Some(Banner::Problem(_)));
        let expanded = banner.is_some() && self.expanded_routine_problems.get(&id) == Some(&shown);
        // The panel's 1 px border and its sections' 16 px sides.
        let column = px(self.details_panel_width(window) - 33.);
        let width = if problem {
            column - REASON_ICON
        } else {
            column
        };
        let long = banner.is_some() && reason_is_long(&shown, width, window, cx);
        let opened = shown.clone();
        let more = long.then(|| {
            Button::new(ElementId::Name(format!("routine-reason-{id}").into()))
                .control(Tier::Ghost, Scale::Compact, false, cx)
                .flush_start(Scale::Compact)
                .label(if expanded { "Show less" } else { "Show more" })
                .on_click(cx.listener(move |app, _, _, cx| {
                    if expanded {
                        app.expanded_routine_problems.remove(&id);
                    } else {
                        app.expanded_routine_problems.insert(id, opened.clone());
                    }
                    cx.notify();
                }))
        });
        let actions = (retry.is_some() || dismiss.is_some()).then(|| {
            h_flex()
                .gap_1()
                .items_center()
                .children(retry)
                .children(dismiss)
        });
        let color = if problem {
            theme.danger
        } else {
            theme.muted_foreground
        };
        let banner = match banner {
            Some(_) => Some(
                reason(shown, color, problem, long && !expanded, more, actions).into_any_element(),
            ),
            None => actions.map(IntoElement::into_any_element),
        };
        v_flex()
            .w_full()
            .gap_1()
            .child(header)
            .children(banner)
            .children(warning.map(|warning| alert(warning, theme.warning, cx)))
            .into_any_element()
    }
}

/// Why a Retry queued nothing, in the owner's terms.
fn retry_failure(err: &crate::api::ClientError) -> String {
    match err.code() {
        Some("provider_unavailable") => "Couldn't retry. No provider can run this routine now. \
            Check Settings or choose another model for the routine."
            .to_owned(),
        Some("already_running") => "A run of this routine is already in progress.".to_owned(),
        Some("nothing_to_retry") => "This routine's last run no longer needs a retry.".to_owned(),
        Some("queue_full") => {
            "Couldn't retry. The bot's queue is full; try again once its requests finish."
                .to_owned()
        }
        _ => format!("Couldn't retry. {err}"),
    }
}

fn current_change(
    settings: &mut HashMap<Uuid, BotSettings>,
    bot: Uuid,
    routine: Uuid,
    attempt: Uuid,
) -> Option<&mut BotSettings> {
    let entry = settings.get_mut(&bot)?;
    (entry.routines.get(&routine).and_then(RoutineSave::attempt) == Some(attempt)).then_some(entry)
}

/// Whether the owner may run the routine again now: it has failures the
/// owner has not dismissed, or else.bot paused it. The API accepts a Retry
/// under the same rule.
pub(super) fn retryable(routine: &Routine) -> bool {
    routine.failed_runs > 0 || (!routine.enabled && routine.paused_reason.is_some())
}

/// What a routine shows under its schedule.
#[derive(Debug, PartialEq, Eq)]
enum Banner {
    /// How a retry is going.
    Progress(String),
    /// Why the routine stopped, or why the owner's last change didn't hold.
    Problem(String),
}

impl Banner {
    fn text(&self) -> &str {
        match self {
            Self::Progress(text) | Self::Problem(text) => text,
        }
    }
}

/// A change that failed outranks a retry's progress: it explains why the
/// switch or menu choice didn't hold.
fn banner(routine: &Routine, save: Option<&RoutineSave>) -> Option<Banner> {
    if let Some(RoutineSave::Failed(err)) = save {
        return Some(Banner::Problem(err.clone()));
    }
    let (text, attention) = last_run(routine)?;
    if routine.retrying {
        Some(Banner::Progress(text))
    } else {
        attention.then_some(Banner::Problem(text))
    }
}

fn last_run(routine: &Routine) -> Option<(String, bool)> {
    let run = routine.last_run.as_ref()?;
    let when = local_time(run.due_at, &routine.timezone);
    let error = run.error.as_ref().and_then(|e| e.message.clone());
    if routine.retrying {
        let progress = match run.status {
            RunStatus::Queued => "Retrying: waiting to start",
            RunStatus::Checking | RunStatus::Running => "Retrying: running now",
            _ => "Retrying: checking the result",
        };
        return Some((progress.to_owned(), false));
    }
    if !routine.enabled
        && let Some(reason) = &routine.paused_reason
    {
        let why = match reason {
            PausedReason::Repeated => format!("Paused after {} failed runs", routine.failed_runs),
            _ => "Paused: retrying won't fix the last failure".to_owned(),
        };
        let detail = error.map_or_else(String::new, |e| format!(" ({e})"));
        return Some((format!("{why}{detail}"), true));
    }
    let (text, attention) = last_run_status(run, &when, error);
    Some((
        text,
        attention && (routine.failed_runs > 0 || run.status == RunStatus::Skipped),
    ))
}

fn last_run_status(run: &LastRun, when: &str, error: Option<String>) -> (String, bool) {
    let reason = |skip: Option<SkipReason>| match skip {
        Some(SkipReason::Overlap) => "the previous run was still going",
        Some(SkipReason::QueueFull) => "too many messages were waiting",
        Some(SkipReason::ProviderUnavailable) => "no provider was available",
        Some(SkipReason::CheckCancelled) => "its check was stopped",
        Some(SkipReason::CheckUnavailable) => "its check could not start",
        Some(SkipReason::NothingNew | SkipReason::Unknown) | None => "it could not start",
    };
    let status = run.status.as_str();
    match run.status {
        RunStatus::Skipped if run.skip_reason == Some(SkipReason::NothingNew) => {
            (format!("Last run {when}: nothing new"), false)
        }
        RunStatus::NoNews => (format!("Last run {when}: no news"), false),
        RunStatus::Failed | RunStatus::Uncertain | RunStatus::Interrupted => {
            let detail = error.unwrap_or_else(|| status.to_owned());
            (format!("Last run {when} {status}: {detail}"), true)
        }
        RunStatus::Skipped => (format!("Skipped {when}: {}", reason(run.skip_reason)), true),
        RunStatus::Checking => (format!("Checking for new work (due {when})"), false),
        RunStatus::Queued | RunStatus::Running => (format!("Running now (due {when})"), false),
        RunStatus::Completed | RunStatus::Other(_) => (format!("Last run {when}: {status}"), false),
    }
}

/// What a menu item's side card says about the setting it changes.
#[derive(Clone)]
struct Hint {
    title: SharedString,
    body: SharedString,
}

/// A muted, unclickable heading over a group of menu items.
fn menu_heading(title: &'static str) -> PopupMenuItem {
    PopupMenuItem::element(move |_, _| div().text_xs().child(title.to_uppercase())).disabled(true)
}

/// A menu item's label, an optional value badge, and a card beside the menu
/// that explains the setting while the pointer rests on the item.
fn hinted(
    id: String,
    icon: gpui_kit::assets::IconName,
    label: &'static str,
    badge: Option<&'static str>,
    hint: &Hint,
    cx: &App,
) -> HoverCard {
    let theme = cx.theme();
    let badge = badge.map(|text| {
        div()
            .px_1p5()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(text)
    });
    let hint = hint.clone();
    HoverCard::new(ElementId::Name(id.into()))
        .flex_1()
        .anchor(Anchor::RightCenter)
        .open_delay(Duration::from_millis(300))
        .trigger(
            h_flex()
                .w_full()
                .gap_2()
                .child(Icon::new(icon).xsmall())
                .child(div().flex_1().child(label))
                .children(badge),
        )
        .content(move |_, _, cx| {
            let theme = cx.theme();
            let card = v_flex()
                .w(px(240.))
                .p_3()
                .gap_1()
                .rounded(theme.radius_lg)
                .bg(theme.popover)
                .text_color(theme.popover_foreground)
                .border_1()
                .border_color(theme.border)
                .shadow_lg()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .whitespace_normal()
                        .child(hint.title.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .whitespace_normal()
                        .text_color(theme.muted_foreground)
                        .child(hint.body.clone()),
                );
            // The padding, not a margin, keeps the card clear of the menu's
            // edge: the popup measures this row to place it.
            h_flex().id("routine-hint").pr_5().child(card)
        })
}

/// The lines a routine's banner shows before "Show more".
const REASON_LINES: usize = 3;
const REASON_SIZE: Pixels = px(12.5);
/// The warning icon and its gap, beside the reason.
const REASON_ICON: Pixels = px(18.);

/// Whether `text` wraps past [`REASON_LINES`] at `width`, shaped as the
/// text element shapes it.
fn reason_is_long(text: &str, width: Pixels, window: &Window, cx: &App) -> bool {
    let run = TextRun {
        len: text.len(),
        font: font(cx.theme().font_family.clone()),
        color: Hsla::default(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_text(
            SharedString::from(text.to_owned()),
            REASON_SIZE,
            &[run],
            Some(width),
            None,
        )
        .is_ok_and(|lines| {
            lines
                .iter()
                .map(|line| line.wrap_boundaries().len() + 1)
                .sum::<usize>()
                > REASON_LINES
        })
}

/// A routine's state under its name: why it stopped or how a retry is
/// going, with the actions that answer it on the line below.
fn reason(
    text: String,
    color: Hsla,
    icon: bool,
    clamped: bool,
    more: Option<Button>,
    actions: Option<Div>,
) -> Div {
    let body = v_flex()
        .flex_1()
        .min_w_0()
        .items_start()
        .child(
            div()
                .w_full()
                .whitespace_normal()
                .when(clamped, |body| {
                    body.line_clamp(REASON_LINES).text_ellipsis()
                })
                .child(text),
        )
        .children(more);
    v_flex()
        .w_full()
        .gap_1()
        .child(
            h_flex()
                .pt_0p5()
                .gap_1p5()
                .items_start()
                .text_size(REASON_SIZE)
                .line_height(px(18.))
                .text_color(color)
                .when(icon, |row| {
                    row.child(
                        Icon::new(IconName::TriangleAlert)
                            .size_3()
                            .mt(px(3.))
                            .flex_none(),
                    )
                })
                .child(body),
        )
        .children(actions.map(|actions| {
            h_flex()
                .when(icon, |row| row.child(div().flex_none().w(REASON_ICON)))
                .child(actions)
        }))
}

#[cfg(test)]
mod tests {
    use super::{Banner, RoutineSave, banner, last_run, retryable};

    #[test]
    fn a_failed_change_shows_over_a_retry_in_progress() {
        let retrying: crate::model::Routine = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::nil(), "name": "Tickets", "prompt": "p",
            "cron": "0 * * * *", "timezone": "Europe/Paris", "enabled": false,
            "next_run_at": null, "updated_at": "2026-11-08T03:00:00Z",
            "failed_runs": 1, "paused_reason": "needs_action", "retrying": true,
            "last_run": { "due_at": "2026-11-08T02:00:00Z", "status": "running",
                          "skip_reason": null, "error": null },
        }))
        .unwrap();
        assert_eq!(
            banner(&retrying, None),
            Some(Banner::Progress("Retrying: running now".to_owned()))
        );
        let failed = RoutineSave::Failed("Couldn't save. offline".to_owned());
        assert_eq!(
            banner(&retrying, Some(&failed)),
            Some(Banner::Problem("Couldn't save. offline".to_owned())),
            "the owner learns why the switch snapped back"
        );
    }

    #[test]
    fn a_routine_whose_last_run_failed_offers_retry_while_enabled_or_paused() {
        let routine = |enabled: bool, failed_runs: i64, paused: Option<&str>| {
            serde_json::from_value::<crate::model::Routine>(serde_json::json!({
                "id": uuid::Uuid::nil(), "name": "Tickets", "prompt": "p",
                "cron": "0 9 * * 1-5", "timezone": "Europe/Paris", "enabled": enabled,
                "next_run_at": null, "updated_at": "2026-11-20T09:00:00Z",
                "failed_runs": failed_runs, "paused_reason": paused,
            }))
            .unwrap()
        };
        assert!(
            retryable(&routine(true, 1, None)),
            "an enabled routine's failed run"
        );
        assert!(retryable(&routine(false, 3, Some("repeated"))));
        assert!(!retryable(&routine(true, 0, None)), "nothing failed");
        assert!(
            !retryable(&routine(false, 0, None)),
            "the owner switched it off"
        );
    }

    #[test]
    fn a_routine_card_flags_pauses_and_undismissed_failures_only() {
        let routine = |extra: serde_json::Value| -> crate::model::Routine {
            let mut value = serde_json::json!({
                "id": uuid::Uuid::nil(), "name": "Tickets", "prompt": "p",
                "cron": "0 * * * *", "timezone": "Europe/Paris", "enabled": true,
                "next_run_at": null, "updated_at": "2026-11-08T03:00:00Z",
                "last_run": { "due_at": "2026-11-08T02:00:00Z", "status": "failed",
                              "skip_reason": null,
                              "error": { "code": "provider_unavailable",
                                         "message": "the provider process exited" } },
            });
            for (key, field) in extra.as_object().unwrap() {
                value[key] = field.clone();
            }
            serde_json::from_value(value).unwrap()
        };
        let (text, attention) =
            last_run(&routine(serde_json::json!({ "failed_runs": 1 }))).unwrap();
        assert!(attention);
        assert!(
            text.ends_with("failed: the provider process exited"),
            "{text}"
        );
        let (_, attention) = last_run(&routine(serde_json::json!({}))).unwrap();
        assert!(!attention, "dismissed failures stay visible but quiet");
        let (text, attention) = last_run(&routine(serde_json::json!({
            "enabled": false, "failed_runs": 3, "paused_reason": "repeated",
        })))
        .unwrap();
        assert!(attention);
        assert_eq!(
            text,
            "Paused after 3 failed runs (the provider process exited)"
        );
        let (text, attention) = last_run(&routine(serde_json::json!({
            "enabled": false, "failed_runs": 1, "paused_reason": "needs_action", "retrying": true,
            "last_run": { "due_at": "2026-11-08T02:00:00Z", "status": "running",
                          "skip_reason": null, "error": null },
        })))
        .unwrap();
        assert!(!attention, "a retry in progress hides the old failure");
        assert_eq!(text, "Retrying: running now");
        let skipped = |reason: &str| {
            last_run(&routine(serde_json::json!({
                "last_run": { "due_at": "2026-11-08T02:00:00Z", "status": "skipped",
                              "skip_reason": reason, "error": null },
            })))
            .unwrap()
        };
        let (text, attention) = skipped("nothing_new");
        assert!(!attention, "a check that found nothing is a normal run");
        assert!(text.ends_with(": nothing new"), "{text}");
        let (text, attention) = skipped("check_unavailable");
        assert!(attention);
        assert!(text.ends_with(": its check could not start"), "{text}");
        let (text, attention) = last_run(&routine(serde_json::json!({
            "quiet": true,
            "last_run": { "due_at": "2026-11-08T02:00:00Z", "status": "no_news",
                          "skip_reason": null, "error": null },
        })))
        .unwrap();
        assert!(!attention, "a run with no news is a normal run");
        assert_eq!(text, "Last run Sun 8 Nov, 03:00: no news");
    }
}
