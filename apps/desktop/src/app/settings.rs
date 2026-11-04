use super::view::FOOTER_CONTROL_HEIGHT;
use super::{APP_CONTEXT, Connection, ElseApp, ToggleSettings};
use std::collections::HashMap;

use crate::api::{Bot, BotProfile, ProfileChoice};
use crate::avatar::{Avatar, AvatarColor, AvatarShape, bot_avatar};
use crate::model::{LastRun, Routine};
use crate::profile_picker::{ProfilePicker, effort_label, model_label};
use chrono::{DateTime, Utc};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::hover_card::HoverCard;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Size, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::*;
use std::time::Duration;
use uuid::Uuid;

pub(super) const PANEL_WIDTH: Pixels = px(336.);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Metadata {
    name: String,
    avatar: Avatar,
}

impl Metadata {
    fn of(bot: &Bot) -> Self {
        Self {
            name: bot.name.clone(),
            avatar: bot.avatar,
        }
    }
}

fn parse_name(text: &str) -> Result<String, &'static str> {
    let name = text.trim();
    if name.is_empty() {
        Err("Give the bot a name.")
    } else if name.chars().count() > 80 {
        Err("Keep the name to 80 characters or fewer.")
    } else {
        Ok(name.to_owned())
    }
}

#[derive(Debug, Clone, PartialEq)]
struct MetadataSync {
    seen: Metadata,
    revision: Option<i64>,
    saved: Metadata,
    desired: Metadata,
    sending: Option<Metadata>,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct MetadataChange {
    name: Option<String>,
    avatar: Option<Avatar>,
}

impl MetadataSync {
    fn new(values: Metadata, revision: Option<i64>) -> Self {
        Self {
            seen: values.clone(),
            revision,
            saved: values.clone(),
            desired: values,
            sending: None,
            error: None,
        }
    }

    fn begin(&mut self) -> Option<MetadataChange> {
        if self.sending.is_some() || self.desired == self.saved {
            return None;
        }
        let change = MetadataChange {
            name: (self.desired.name != self.saved.name).then(|| self.desired.name.clone()),
            avatar: (self.desired.avatar != self.saved.avatar).then_some(self.desired.avatar),
        };
        self.sending = Some(self.desired.clone());
        self.error = None;
        Some(change)
    }

    fn finish(&mut self, result: Result<(Metadata, Option<i64>), String>) {
        let Some(sent) = self.sending.take() else {
            return;
        };
        match result {
            Ok((server, revision)) => {
                let server = if revision < self.revision {
                    self.saved.clone()
                } else {
                    self.revision = revision;
                    server
                };
                if self.desired.name == sent.name {
                    self.desired.name = server.name.clone();
                }
                if self.desired.avatar == sent.avatar {
                    self.desired.avatar = server.avatar;
                }
                self.saved = server;
            }
            Err(err) => self.error = (self.desired != self.saved).then_some(err),
        }
    }

    fn remote(&mut self, latest: Metadata, revision: Option<i64>) {
        if revision < self.revision || (revision.is_none() && latest == self.seen) {
            return;
        }
        if self.desired.name == self.saved.name {
            self.desired.name = latest.name.clone();
        }
        if self.desired.avatar == self.saved.avatar {
            self.desired.avatar = latest.avatar;
        }
        self.saved = latest.clone();
        self.seen = latest;
        self.revision = revision;
        if self.desired == self.saved {
            self.error = None;
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Save {
    Idle,
    Saving(Uuid),
    Failed(String),
}

impl Save {
    fn busy(&self) -> bool {
        matches!(self, Self::Saving(_))
    }
}

struct Details {
    sync: MetadataSync,
    name: Entity<InputState>,
    editing: bool,
    problem: Option<&'static str>,
    picking: bool,
    _edits: Subscription,
}

enum ModelState {
    Loading(Uuid),
    Failed(String),
    Ready(Option<BotProfile>),
}

pub(super) struct ModelDialog {
    bot: Uuid,
    saved: Option<ProfileChoice>,
    picker: Entity<ProfilePicker>,
    save: Save,
    _picker: Subscription,
}

impl ModelDialog {
    fn dirty(&self, cx: &App) -> bool {
        self.picker.read(cx).choice(cx) != self.saved
    }
}

#[derive(Debug, Clone, PartialEq)]
enum RoutineSave {
    Saving { attempt: Uuid, enabled: bool },
    Deleting { attempt: Uuid },
    Failed(String),
}

impl RoutineSave {
    fn busy(&self) -> bool {
        matches!(self, Self::Saving { .. } | Self::Deleting { .. })
    }

    fn attempt(&self) -> Option<Uuid> {
        match self {
            Self::Saving { attempt, .. } | Self::Deleting { attempt } => Some(*attempt),
            Self::Failed(_) => None,
        }
    }
}

pub(super) struct BotSettings {
    details: Details,
    model: ModelState,
    routines: HashMap<Uuid, RoutineSave>,
}

impl ElseApp {
    pub(super) fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = !self.settings_open;
        if self.settings_open {
            if let Some(bot) = self.selected.filter(|id| {
                self.settings
                    .get(id)
                    .is_some_and(|entry| !matches!(entry.model, ModelState::Loading(_)))
            }) {
                self.load_profile(bot, window, cx);
            }
        } else {
            self.composer.read(cx).focus_handle(cx).focus(window, cx);
        }
        self.ensure_settings(window, cx);
        cx.notify();
    }

    pub(super) fn rename_bot(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if !self.active_bots().any(|b| b.id == id) {
            return;
        }
        self.open_bot(id, window, cx);
        if !self.settings_open {
            self.toggle_settings(window, cx);
        }
        self.edit_name(id, window, cx);
    }

    pub(super) fn settings_shown(&self) -> bool {
        self.settings_open && self.selected_bot().is_some_and(|b| b.is_active())
    }

    pub(super) fn ensure_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_shown() {
            return;
        }
        let Some(bot) = self.selected_bot().cloned() else {
            return;
        };
        if self.settings.contains_key(&bot.id) {
            return;
        }
        let details = new_details(&bot, window, cx);
        self.settings.insert(
            bot.id,
            BotSettings {
                details,
                model: ModelState::Loading(Uuid::nil()),
                routines: HashMap::new(),
            },
        );
        self.load_profile(bot.id, window, cx);
    }

    pub(super) fn sync_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let bots = &self.bots;
        self.settings
            .retain(|id, _| bots.iter().any(|b| b.id == *id && b.is_active()));
        for bot in &self.bots {
            if let Some(entry) = self.settings.get_mut(&bot.id) {
                entry
                    .details
                    .sync
                    .remote(Metadata::of(bot), bot.metadata_revision);
            }
        }
        self.ensure_settings(window, cx);
    }

    fn edit_name(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(details) = self.settings.get_mut(&bot).map(|s| &mut s.details) else {
            return;
        };
        if !details.editing {
            details.editing = true;
            details.problem = None;
            let name = details.sync.desired.name.clone();
            details
                .name
                .update(cx, |state, cx| state.set_value(name, window, cx));
        }
        details.name.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
        cx.notify();
    }

    fn commit_name(&mut self, bot: Uuid, cx: &mut Context<Self>) {
        let Some(details) = self.settings.get_mut(&bot).map(|s| &mut s.details) else {
            return;
        };
        if !details.editing {
            return;
        }
        match parse_name(&details.name.read(cx).value()) {
            Ok(name) => {
                details.editing = false;
                details.problem = None;
                details.sync.desired.name = name;
                self.flush_details(bot, cx);
            }
            Err(problem) => details.problem = Some(problem),
        }
        cx.notify();
    }

    fn pick_avatar(&mut self, bot: Uuid, avatar: Avatar, cx: &mut Context<Self>) {
        if let Some(details) = self.settings.get_mut(&bot).map(|s| &mut s.details) {
            details.sync.desired.avatar = avatar;
            self.flush_details(bot, cx);
            cx.notify();
        }
    }

    fn set_picking(&mut self, bot: Uuid, open: bool, cx: &mut Context<Self>) {
        if let Some(details) = self.settings.get_mut(&bot).map(|s| &mut s.details) {
            details.picking = open;
            cx.notify();
        }
    }

    fn flush_details(&mut self, bot: Uuid, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let Some(change) = self
            .settings
            .get_mut(&bot)
            .and_then(|s| s.details.sync.begin())
        else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = client.update_bot(bot, change.name, change.avatar).await;
            let _ = this.update(cx, |app, cx| {
                let Some(details) = app.settings.get_mut(&bot).map(|s| &mut s.details) else {
                    return;
                };
                details.sync.finish(
                    result
                        .as_ref()
                        .map(|bot| (Metadata::of(bot), bot.metadata_revision))
                        .map_err(ToString::to_string),
                );
                if details.sync.error.is_none() {
                    app.flush_details(bot, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn load_profile(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let Some(entry) = self.settings.get_mut(&bot) else {
            return;
        };
        let attempt = Uuid::new_v4();
        entry.model = ModelState::Loading(attempt);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.bot_profile(bot).await;
            let _ = this.update(cx, |app, cx| {
                let Some(entry) = app.settings.get_mut(&bot) else {
                    return;
                };
                if !matches!(entry.model, ModelState::Loading(a) if a == attempt) {
                    return;
                }
                entry.model = match result {
                    Ok(saved) => ModelState::Ready(saved),
                    Err(err) => ModelState::Failed(err.to_string()),
                };
                cx.notify();
            });
        })
        .detach();
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
        if !self.begin_routine_change(bot, routine, RoutineSave::Saving { attempt, enabled }) {
            return;
        }
        cx.notify();
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.set_routine_enabled(bot, routine, enabled).await;
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
                        entry.routines.insert(
                            routine,
                            RoutineSave::Failed(format!("Couldn't save. {err}")),
                        );
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
        let Some(name) = self
            .changeable_routine(bot, routine)
            .map(|r| r.name.clone())
        else {
            return;
        };
        let app = cx.entity().downgrade();
        let cancel = cx.focus_handle();
        let confirm = cx.focus_handle();
        let focus_once = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_once.replace(false) {
                cancel.focus(window, cx);
            }
            let theme = cx.theme();
            let consequence = |text: &'static str| {
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(div().text_color(theme.muted_foreground).child("•"))
                    .child(text)
            };
            let app = app.clone();
            let app_ok = app.clone();
            let confirm_ok = confirm.clone();
            dialog
                .title(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Icon::new(IconName::TriangleAlert).text_color(theme.danger))
                        .child(div().truncate().child(format!("Delete routine {name}?"))),
                )
                .w(px(460.))
                .on_ok(move |_, window, cx| {
                    if confirm_ok.is_focused(window) {
                        let _ = app_ok
                            .update(cx, |app, cx| app.delete_routine(bot, routine, window, cx));
                    }
                    true
                })
                .child(
                    v_flex()
                        .gap_1()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(consequence("It never runs again and leaves this list."))
                        .child(consequence("A run already in progress finishes.")),
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("routine-delete-cancel")
                                .label("Cancel")
                                .outline()
                                .track_focus(&cancel)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("routine-delete-confirm")
                                .label("Delete routine")
                                .danger()
                                .track_focus(&confirm)
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ = app.update(cx, |app, cx| {
                                        app.delete_routine(bot, routine, window, cx)
                                    });
                                }),
                        ),
                )
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
                if let Err(err) = result {
                    entry.routines.insert(
                        routine,
                        RoutineSave::Failed(format!("Couldn't delete. {err}")),
                    );
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn open_model_dialog(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(caps) = self.caps.clone() else {
            return;
        };
        let Some(ModelState::Ready(saved)) = self.settings.get(&bot).map(|s| &s.model) else {
            return;
        };
        let saved = saved.as_ref().map(|p| p.choice.clone());
        let picker = cx.new(|cx| ProfilePicker::new(&caps, saved.as_ref(), window, cx).stacked());
        let observer = cx.observe_in(&picker, window, |_, _, window, _| window.refresh());
        let holder = cx.new(|_| ModelDialog {
            bot,
            saved,
            picker: picker.clone(),
            save: Save::Idle,
            _picker: observer,
        });
        self.model_dialog = Some(holder.clone());
        let app = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let form = holder.read(cx);
            let saving = form.save.busy();
            let connected = app
                .upgrade()
                .is_some_and(|app| app.read(cx).connection == Connection::Connected);
            let ready = form.dirty(cx) && form.picker.read(cx).choice(cx).is_some();
            let body = v_flex()
                .gap_3()
                .child(picker.clone())
                .child(muted(
                    "Changes apply from the bot's next message; work already running is unaffected.",
                    cx,
                ))
                .children(match &form.save {
                    Save::Failed(err) => Some(note(format!("Couldn't apply. {err}"), true, cx)),
                    _ => None,
                });
            let app_ok = app.clone();
            let app_apply = app.clone();
            let app_close = app.clone();
            let mine = holder.clone();
            dialog
                .title("Change model")
                .w(px(420.))
                .on_ok(move |_, window, cx| {
                    let _ = app_ok.update(cx, |app, cx| app.apply_model(window, cx));
                    false
                })
                // Dismissed: a late result must not close another dialog.
                .on_close(move |_, _, cx| {
                    let _ = app_close.update(cx, |app, _| {
                        if app.model_dialog.as_ref() == Some(&mine) {
                            app.model_dialog = None;
                        }
                    });
                })
                .child(body)
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("model-cancel")
                                .label("Cancel")
                                .ghost()
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("model-apply")
                                .label("Apply")
                                .primary()
                                .loading(saving)
                                .disabled(!ready || saving || !connected)
                                .on_click(move |_, window, cx| {
                                    let _ = app_apply.update(cx, |app, cx| app.apply_model(window, cx));
                                }),
                        ),
                )
        });
    }

    fn apply_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (client, Some(holder)) = (self.client.clone(), self.model_dialog.clone()) else {
            return;
        };
        let (bot, picker) = {
            let form = holder.read(cx);
            (form.bot, form.picker.clone())
        };
        let Some(choice) = picker.read(cx).choice(cx) else {
            return;
        };
        if holder.read(cx).save.busy() || !holder.read(cx).dirty(cx) {
            return;
        }
        let attempt = Uuid::new_v4();
        picker.update(cx, |picker, cx| picker.set_disabled(true, cx));
        holder.update(cx, |form, cx| {
            form.save = Save::Saving(attempt);
            cx.notify();
        });
        window.refresh();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.set_bot_profile(bot, choice).await;
            let _ = this.update_in(cx, |app, window, cx| {
                if holder.read(cx).save != Save::Saving(attempt) {
                    return;
                }
                match result {
                    Ok(saved) => {
                        if let Some(entry) = app.settings.get_mut(&bot)
                            && matches!(entry.model, ModelState::Ready(_))
                        {
                            entry.model = ModelState::Ready(Some(saved));
                        }
                        if app.model_dialog.as_ref() == Some(&holder) {
                            app.model_dialog = None;
                            window.close_dialog(cx);
                        }
                        cx.notify();
                    }
                    Err(err) => {
                        picker.update(cx, |picker, cx| picker.set_disabled(false, cx));
                        holder.update(cx, |form, cx| {
                            form.save = Save::Failed(err.to_string());
                            cx.notify();
                        });
                        window.refresh();
                    }
                }
            });
        })
        .detach();
    }

    pub(super) fn render_settings(&self, bot: &Bot, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let header = h_flex()
            .flex_none()
            .px_4()
            .py_2()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Bot settings"),
            )
            .child(
                Button::new("close-settings")
                    .icon(IconName::PanelRightClose)
                    .small()
                    .ghost()
                    .tooltip_with_action("Hide settings", &ToggleSettings, Some(APP_CONTEXT))
                    .on_click(cx.listener(|app, _, window, cx| app.toggle_settings(window, cx))),
            );
        let mut body = v_flex()
            .id("bot-settings-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_4()
            .gap_6();
        match self.settings.get(&bot.id) {
            Some(entry) => {
                body = body
                    .child(self.render_details(bot.id, &entry.details, cx))
                    .child(self.render_model(bot.id, &entry.model, cx))
                    .child(self.render_routines(bot.id, &entry.routines, cx));
            }
            None => body = body.child(muted("Loading…", cx)),
        }
        let mut panel = v_flex()
            .id("bot-settings")
            .w(PANEL_WIDTH)
            .flex_none()
            .h_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_l_1()
            .border_color(theme.sidebar_border)
            .child(header)
            .child(body);
        if self.default_bot_id != Some(bot.id) {
            let id = bot.id;
            panel = panel.child(
                div()
                    .flex_none()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        Button::new("delete-bot")
                            .label("Delete bot")
                            .h(FOOTER_CONTROL_HEIGHT)
                            .danger()
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

    fn render_details(&self, bot: Uuid, details: &Details, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let name = if details.editing {
            div()
                .id("bot-name-editor")
                .w_full()
                .on_mouse_down_out(cx.listener(move |app, _, _, cx| app.commit_name(bot, cx)))
                .child(
                    Input::new(&details.name)
                        .text_center()
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .into_any_element()
        } else {
            let (hover, border) = (theme.sidebar_accent, theme.border);
            div()
                .id("bot-name")
                .max_w_full()
                .px_2()
                .py_0p5()
                .rounded_md()
                .border_1()
                .border_color(transparent_black())
                .cursor_text()
                .hover(move |style| style.bg(hover).border_color(border))
                .font_weight(FontWeight::SEMIBOLD)
                .text_center()
                .truncate()
                .aria_description("Click to rename")
                .child(details.sync.desired.name.clone())
                .on_click(cx.listener(move |app, _, window, cx| app.edit_name(bot, window, cx)))
                .into_any_element()
        };
        let problem = details
            .problem
            .filter(|_| details.editing)
            .map(|problem| note(problem, true, cx));
        let error = details.sync.error.clone().map(|err| {
            h_flex()
                .gap_2()
                .items_center()
                .child(note(format!("Couldn't save. {err}"), true, cx))
                .child(
                    Button::new("retry-details")
                        .label("Retry")
                        .xsmall()
                        .outline()
                        .on_click(cx.listener(move |app, _, _, cx| app.flush_details(bot, cx))),
                )
        });
        v_flex()
            .items_center()
            .gap_2()
            .child(self.render_avatar_editor(bot, details, cx))
            .child(h_flex().w_full().justify_center().child(name))
            .children(problem)
            .children(error)
            .into_any_element()
    }

    fn render_avatar_editor(
        &self,
        bot: Uuid,
        details: &Details,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let editing = if details.picking {
            "Close avatar picker"
        } else {
            "Edit avatar"
        };
        let badge = div()
            .absolute()
            .right(px(2.))
            .bottom(px(2.))
            .size(px(26.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .text_color(theme.popover_foreground)
            .shadow_sm()
            .opacity(if details.picking { 1. } else { 0. })
            .group_hover("avatar-editor", |style| style.opacity(1.))
            .child(
                Icon::new(gpui_kit::assets::IconName::PencilLine).with_size(Size::Size(px(14.))),
            );
        let hero = Button::new("edit-avatar")
            .ghost()
            .w(px(112.))
            .h(px(112.))
            .p_0()
            .rounded(px(28.))
            .tooltip(editing)
            .accessibility_label(editing)
            .toggled(details.picking)
            .child(
                div()
                    .relative()
                    .group("avatar-editor")
                    .child(bot_avatar(
                        ElementId::Name(format!("settings-avatar-{bot}").into()),
                        details.sync.desired.avatar,
                        px(96.),
                    ))
                    .child(badge),
            );
        let app = cx.entity().downgrade();
        let toggle = app.clone();
        Popover::new("avatar-picker")
            .anchor(Anchor::TopCenter)
            .open(details.picking)
            .on_open_change(move |open, _, cx| {
                let _ = toggle.update(cx, |app, cx| app.set_picking(bot, *open, cx));
            })
            .trigger(hero)
            .content(move |_, _, cx| {
                let current = app
                    .upgrade()
                    .and_then(|app| {
                        let app = app.read(cx);
                        app.settings
                            .get(&bot)
                            .map(|s| s.details.sync.desired.avatar)
                    })
                    .unwrap_or_default();
                avatar_picker(app.clone(), bot, current, cx)
            })
            .into_any_element()
    }

    fn render_model(&self, bot: Uuid, state: &ModelState, cx: &mut Context<Self>) -> AnyElement {
        let body = section("Model", cx);
        let saved = match state {
            ModelState::Loading(_) => return body.child(muted("Loading…", cx)).into_any_element(),
            ModelState::Failed(err) => {
                return body
                    .child(note(format!("Couldn't load the model. {err}"), true, cx))
                    .child(
                        h_flex().child(
                            Button::new("retry-model")
                                .label("Retry")
                                .small()
                                .outline()
                                .on_click(cx.listener(move |app, _, window, cx| {
                                    app.load_profile(bot, window, cx)
                                })),
                        ),
                    )
                    .into_any_element();
            }
            ModelState::Ready(saved) => saved,
        };
        body.child(self.render_current_model(bot, saved.as_ref(), cx))
            .into_any_element()
    }

    fn render_current_model(
        &self,
        bot: Uuid,
        saved: Option<&BotProfile>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = match saved {
            None => v_flex().child(muted(
                "No model yet. The bot uses the defaults once they are set.",
                cx,
            )),
            Some(saved) => {
                let choice = &saved.choice;
                let speed = if choice.service_tier == "priority" {
                    "Fast"
                } else {
                    "Standard speed"
                };
                let offered = self.caps.as_ref().is_none_or(|caps| {
                    caps.providers.iter().any(|p| {
                        p.id == choice.provider_id && p.catalog.models.contains(&choice.model)
                    })
                });
                v_flex()
                    .gap_1()
                    .child(muted("Now using", cx))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(model_label(&choice.model)),
                    )
                    .child(div().text_xs().child(format!(
                        "{} · {} reasoning · {speed}",
                        self.provider_label(&choice.provider_id),
                        effort_label(&choice.reasoning_effort),
                    )))
                    .children((!offered).then(|| {
                        note(
                            "This provider no longer offers the saved model. It stays until you apply another.",
                            true,
                            cx,
                        )
                    }))
            }
        };
        // Button centers and nowraps its content; the card reads as a left-aligned block.
        Button::new("change-model")
            .outline()
            .with_size(Size::Size(px(16.)))
            .w_full()
            .p_3()
            .tooltip("Change model")
            .disabled(self.caps.is_none() || self.connection != Connection::Connected)
            .child(
                content
                    .flex_1()
                    .min_w_0()
                    .items_start()
                    .text_left()
                    .whitespace_normal(),
            )
            .child(Icon::new(IconName::ChevronRight).flex_none())
            .on_click(cx.listener(move |app, _, window, cx| app.open_model_dialog(bot, window, cx)))
            .into_any_element()
    }
}

impl ElseApp {
    fn render_routines(
        &self,
        bot: Uuid,
        saves: &HashMap<Uuid, RoutineSave>,
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
        let routines = conversation.routines();
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
        body.children(routines.iter().map(|routine| {
            self.render_routine(bot, routine, saves.get(&routine.id), connected, cx)
        }))
        .into_any_element()
    }

    fn render_routine(
        &self,
        bot: Uuid,
        routine: &Routine,
        save: Option<&RoutineSave>,
        connected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = routine.id;
        let busy = save.is_some_and(RoutineSave::busy);
        let checked = match save {
            Some(RoutineSave::Saving { enabled, .. }) => *enabled,
            _ => routine.enabled,
        };
        let switch = Switch::new(ElementId::Name(format!("routine-{id}").into()))
            .checked(checked)
            .small()
            .tooltip(if checked {
                "Pause routine"
            } else {
                "Resume routine"
            })
            .accessibility_label(format!("Run {}", routine.name))
            .disabled(busy || !connected)
            .on_click(cx.listener(move |app, enabled: &bool, window, cx| {
                app.set_routine(bot, id, *enabled, window, cx)
            }));
        let delete = Button::new(ElementId::Name(format!("routine-delete-{id}").into()))
            .icon(gpui_kit::assets::IconName::Trash)
            .ghost()
            .xsmall()
            .loading(matches!(save, Some(RoutineSave::Deleting { .. })))
            .tooltip("Delete routine")
            .accessibility_label(format!("Delete {}", routine.name))
            .disabled(busy || !connected)
            .on_click(cx.listener(move |app, _, window, cx| {
                app.open_routine_delete_dialog(bot, id, window, cx)
            }));
        let schedule = format!("{} · {}", routine.cron, routine.timezone);
        let last = routine
            .last_run
            .as_ref()
            .map(|run| last_run(run, &routine.timezone));
        let problem = match save {
            Some(RoutineSave::Failed(err)) => Some(err.clone()),
            _ => last
                .clone()
                .filter(|(_, error)| *error)
                .map(|(text, _)| text),
        };
        let theme = cx.theme();
        let (hover_bg, hover_border) = (theme.sidebar_accent, theme.border);
        let pill = h_flex()
            .id(ElementId::Name(format!("routine-pill-{id}").into()))
            .w_full()
            .gap_2()
            .items_center()
            .px_3()
            .py_2()
            .rounded_lg()
            .border_1()
            .border_color(theme.sidebar_border)
            .bg(theme.sidebar_accent.opacity(0.4))
            .hover(move |style| style.bg(hover_bg).border_color(hover_border))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(routine.name.clone()),
                    )
                    .child(muted(schedule.clone(), cx).truncate())
                    .children(problem.map(|text| note(text, true, cx))),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .items_center()
                    .child(delete)
                    .child(switch),
            );
        let name = routine.name.clone();
        let prompt = routine.prompt.clone();
        let next = match (routine.enabled, routine.next_run_at) {
            (false, _) => None,
            (true, Some(at)) => Some(format!("Next run {}", local_time(at, &routine.timezone))),
            (true, None) => Some("No further runs".to_owned()),
        };
        HoverCard::new(ElementId::Name(format!("routine-card-{id}").into()))
            .trigger_style(StyleRefinement::default().w_full())
            .anchor(Anchor::RightCenter)
            .open_delay(Duration::from_millis(300))
            .w(px(300.))
            .trigger(pill)
            .content(move |_, _, cx| {
                let theme = cx.theme();
                let line =
                    |text: String, color: Hsla| div().text_xs().text_color(color).child(text);
                v_flex()
                    .gap_2()
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
                    )
            })
            .into_any_element()
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

fn local_time(at: DateTime<Utc>, timezone: &str) -> String {
    match timezone.parse::<chrono_tz::Tz>() {
        Ok(tz) => at.with_timezone(&tz).format("%a %-d %b, %H:%M").to_string(),
        Err(_) => at.format("%a %-d %b, %H:%M UTC").to_string(),
    }
}

fn last_run(run: &LastRun, timezone: &str) -> (String, bool) {
    let when = local_time(run.due_at, timezone);
    let reason = |code: &str| match code {
        "overlap" => "the previous run was still going",
        "queue_full" => "too many messages were waiting",
        "provider_unavailable" => "no provider was available",
        _ => "it could not start",
    };
    match run.status.as_str() {
        "failed" | "uncertain" | "interrupted" => {
            let detail = run
                .error
                .as_ref()
                .and_then(|e| e.message.clone())
                .unwrap_or_else(|| run.status.clone());
            (format!("Last run {when} {}: {detail}", run.status), true)
        }
        "skipped" => (
            format!(
                "Skipped {when}: {}",
                reason(run.skip_reason.as_deref().unwrap_or_default())
            ),
            true,
        ),
        "queued" | "running" => (format!("Running now (due {when})"), false),
        status => (format!("Last run {when}: {status}"), false),
    }
}

fn new_details(bot: &Bot, window: &mut Window, cx: &mut Context<ElseApp>) -> Details {
    let values = Metadata::of(bot);
    let id = bot.id;
    let name = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder("Name")
            .default_value(values.name.clone())
    });
    Details {
        _edits: cx.subscribe(&name, move |app, _, event: &InputEvent, cx| match event {
            InputEvent::PressEnter { .. } | InputEvent::Blur => app.commit_name(id, cx),
            InputEvent::Change => {
                if let Some(details) = app.settings.get_mut(&id).map(|s| &mut s.details) {
                    details.problem = None;
                }
                cx.notify();
            }
            InputEvent::Focus => {}
        }),
        sync: MetadataSync::new(values, bot.metadata_revision),
        name,
        editing: false,
        problem: None,
        picking: false,
    }
}

fn avatar_picker(app: WeakEntity<ElseApp>, bot: Uuid, current: Avatar, cx: &App) -> AnyElement {
    let theme = cx.theme().clone();
    let pick = move |avatar: Avatar| {
        let app = app.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            let _ = app.update(cx, |app, cx| app.pick_avatar(bot, avatar, cx));
        }
    };
    let shapes = AvatarShape::ALL.map(|shape| {
        let picked = current.shape == shape;
        let avatar = Avatar { shape, ..current };
        Button::new(ElementId::Name(format!("avatar-shape-{shape:?}").into()))
            .ghost()
            .w(px(44.))
            .h(px(44.))
            .p_0()
            .rounded(px(12.))
            .toggled(picked)
            .accessibility_label(format!("{} shape", shape.label()))
            .child(ring(
                px(44.),
                px(12.),
                picked,
                cx,
                bot_avatar(
                    ElementId::Name(format!("avatar-shape-art-{shape:?}").into()),
                    avatar,
                    px(32.),
                )
                .still(),
            ))
            .on_click(pick(avatar))
    });
    let colors = AvatarColor::ALL.map(|color| {
        let picked = current.color == color;
        let avatar = Avatar { color, ..current };
        let swatch = div()
            .size(px(20.))
            .rounded_full()
            .p(px(1.))
            .bg(theme.border)
            .child(div().size_full().rounded_full().bg(color.fill()));
        Button::new(ElementId::Name(format!("avatar-color-{color:?}").into()))
            .ghost()
            .w(px(30.))
            .h(px(30.))
            .p_0()
            .rounded_full()
            .toggled(picked)
            .accessibility_label(format!("{} color", color.label()))
            .child(ring(px(30.), px(15.), picked, cx, swatch))
            .on_click(pick(avatar))
    });
    let header = h_flex()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child("AVATAR"),
        )
        .child(
            Button::new("reset-avatar")
                .label("Reset")
                .xsmall()
                .ghost()
                .disabled(current == Avatar::default())
                .on_click(pick(Avatar::default())),
        );
    v_flex()
        .w(px(288.))
        .gap_3()
        .child(header)
        .child(
            h_flex()
                .flex_wrap()
                .gap_1()
                .justify_center()
                .children(shapes),
        )
        .child(
            h_flex()
                .flex_wrap()
                .gap_1()
                .justify_center()
                .children(colors),
        )
        .into_any_element()
}

fn ring(size: Pixels, radius: Pixels, picked: bool, cx: &App, content: impl IntoElement) -> Div {
    let theme = cx.theme();
    let (ring, gap) = if picked {
        (theme.ring, theme.popover)
    } else {
        (transparent_black(), transparent_black())
    };
    let layer = |size: Pixels, radius: Pixels, fill: Hsla| {
        div()
            .size(size)
            .rounded(radius)
            .bg(fill)
            .flex()
            .items_center()
            .justify_center()
    };
    layer(size, radius, ring).child(layer(size - px(4.), radius - px(2.), gap).child(content))
}

fn section(title: &'static str, cx: &App) -> Div {
    v_flex().gap_3().child(
        div()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().muted_foreground)
            .child(title.to_uppercase()),
    )
}

fn muted(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

fn note(text: impl Into<SharedString>, error: bool, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .text_xs()
        .text_color(if error { theme.danger } else { theme.success })
        .child(text.into())
}

#[cfg(test)]
mod tests {
    use super::{Metadata, MetadataChange, MetadataSync, parse_name};
    use crate::avatar::{Avatar, AvatarColor, AvatarShape};

    fn metadata(name: &str, avatar: Avatar) -> Metadata {
        Metadata {
            name: name.into(),
            avatar,
        }
    }

    fn egg() -> Avatar {
        Avatar {
            shape: AvatarShape::ALL[1],
            color: AvatarColor::ALL[1],
        }
    }

    #[test]
    fn names_mirror_the_api_limits() {
        assert_eq!(parse_name("  Planner "), Ok("Planner".into()));
        assert!(parse_name(&"é".repeat(80)).is_ok());
        assert!(parse_name(" ").is_err());
        assert!(parse_name(&"é".repeat(81)).is_err());
    }

    #[test]
    fn one_request_at_a_time_and_newer_intent_survives_its_response() {
        let mut sync = MetadataSync::new(metadata("Old", Avatar::default()), Some(0));
        sync.desired.name = "New".into();
        assert_eq!(
            sync.begin(),
            Some(MetadataChange {
                name: Some("New".into()),
                avatar: None,
            })
        );
        sync.desired.avatar = egg();
        assert_eq!(sync.begin(), None, "waits for the request in flight");
        sync.remote(metadata("Old", Avatar::default()), Some(0));
        assert_eq!(sync.desired, metadata("New", egg()), "stale list ignored");

        sync.finish(Ok((metadata("New", Avatar::default()), Some(1))));
        assert_eq!(sync.desired, metadata("New", egg()), "the newer pick stays");
        assert_eq!(
            sync.begin(),
            Some(MetadataChange {
                name: None,
                avatar: Some(egg()),
            }),
            "only the pick made meanwhile"
        );
        sync.finish(Ok((metadata("New", egg()), Some(2))));
        assert_eq!(sync.begin(), None, "nothing left to save");
    }

    #[test]
    fn a_delayed_echo_cannot_change_the_shape_of_the_next_color_pick() {
        let mut sync = MetadataSync::new(metadata("Bot", Avatar::default()), Some(0));
        sync.desired.avatar = egg();
        sync.begin();
        sync.finish(Ok((metadata("Bot", egg()), Some(1))));
        let cloud = Avatar {
            shape: AvatarShape::Cloud,
            ..egg()
        };
        sync.desired.avatar = cloud;
        sync.begin();
        sync.finish(Ok((metadata("Bot", cloud), Some(2))));
        sync.remote(metadata("Bot", egg()), Some(1));
        sync.desired.avatar.color = AvatarColor::Pink;
        assert_eq!(
            sync.begin(),
            Some(MetadataChange {
                name: None,
                avatar: Some(Avatar {
                    color: AvatarColor::Pink,
                    ..cloud
                }),
            })
        );
    }

    #[test]
    fn a_newer_remote_edit_survives_an_older_http_response() {
        let mut sync = MetadataSync::new(metadata("Old", Avatar::default()), Some(0));
        sync.desired.name = "Local".into();
        sync.begin();
        sync.remote(metadata("Remote", Avatar::default()), Some(2));
        sync.finish(Ok((metadata("Local", Avatar::default()), Some(1))));
        assert_eq!(sync.desired.name, "Remote");
        assert_eq!(sync.begin(), None);
    }

    #[test]
    fn a_server_echo_confirms_a_save_whose_response_was_lost() {
        let mut sync = MetadataSync::new(metadata("Old", Avatar::default()), Some(0));
        sync.desired.avatar = egg();
        sync.begin();
        sync.finish(Err("lost response".into()));
        sync.remote(metadata("Old", egg()), Some(1));
        assert_eq!(sync.saved, sync.desired);
        assert_eq!(sync.error, None);
        assert_eq!(sync.begin(), None);
    }

    #[test]
    fn a_server_echo_before_a_lost_response_stays_confirmed() {
        let mut sync = MetadataSync::new(metadata("Old", Avatar::default()), Some(0));
        sync.desired.avatar = egg();
        sync.begin();
        sync.remote(metadata("Old", egg()), Some(1));
        sync.finish(Err("lost response".into()));
        assert_eq!(sync.error, None);
        assert_eq!(sync.begin(), None);
    }

    #[test]
    fn a_failed_save_keeps_the_change_for_a_retry() {
        let mut sync = MetadataSync::new(metadata("Old", Avatar::default()), Some(0));
        sync.desired.avatar = egg();
        sync.begin();
        sync.finish(Err("offline".into()));
        assert_eq!(sync.error.as_deref(), Some("offline"));
        sync.remote(metadata("Elsewhere", Avatar::default()), Some(1));
        assert_eq!(
            sync.desired,
            metadata("Elsewhere", egg()),
            "a remote rename lands; the unsaved pick stays"
        );
        assert_eq!(
            sync.begin(),
            Some(MetadataChange {
                name: None,
                avatar: Some(egg()),
            })
        );
        assert_eq!(sync.error, None);
    }
}
