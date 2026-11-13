use super::delete_dialog::DeleteTarget;
use super::view::HEADER_HEIGHT;
use super::{APP_CONTEXT, Connection, ElseApp, ToggleSettings};
use crate::controls::{Control as _, Scale, Tier, segment, segment_track, text_field, toggle};
use gpui_kit::component::FocusableExt as _;
use std::collections::HashMap;

use crate::api::socials::Provider;
use crate::api::{Bot, BotProfile, ProfileChoice};
use crate::avatar::{Avatar, AvatarPicker, bot_avatar};
use crate::model::{Conversation, LastRun, Routine};
use crate::profile_picker::{ProfilePicker, effort_label, model_label};
use chrono::{DateTime, Utc};
use gpui_kit::base::HoverCard;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Size, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::time::Duration;
use uuid::Uuid;

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
    target: ModelTarget,
    saved: Option<ProfileChoice>,
    picker: Entity<ProfilePicker>,
    save: Save,
    _picker: Subscription,
}

enum ModelTarget {
    Bot,
    Routine {
        id: Uuid,
        name: String,
        use_bot_model: bool,
        unresolved: bool,
    },
}

enum SavedModel {
    Bot(BotProfile),
    Routine(Box<Routine>),
}

enum ModelChange {
    Bot(ProfileChoice),
    Routine {
        id: Uuid,
        choice: Option<ProfileChoice>,
    },
}

impl ModelDialog {
    fn change(&self, cx: &App) -> Option<ModelChange> {
        if self.save.busy() || !self.dirty(cx) {
            return None;
        }
        match self.target {
            ModelTarget::Bot => self.choice(cx).map(ModelChange::Bot),
            ModelTarget::Routine { id, .. } if self.inherits() => {
                Some(ModelChange::Routine { id, choice: None })
            }
            ModelTarget::Routine { id, .. } => self.choice(cx).map(|choice| ModelChange::Routine {
                id,
                choice: Some(choice),
            }),
        }
    }

    fn routine_id(&self) -> Option<Uuid> {
        match self.target {
            ModelTarget::Bot => None,
            ModelTarget::Routine { id, .. } => Some(id),
        }
    }

    fn choice(&self, cx: &App) -> Option<ProfileChoice> {
        if self.inherits() {
            None
        } else {
            self.picker.read(cx).choice()
        }
    }

    fn inherits(&self) -> bool {
        matches!(
            self.target,
            ModelTarget::Routine {
                use_bot_model: true,
                ..
            }
        )
    }

    fn dirty(&self, cx: &App) -> bool {
        self.choice(cx) != self.saved
            || matches!(
                self.target,
                ModelTarget::Routine {
                    unresolved: true,
                    ..
                }
            )
    }
}

#[derive(Debug, Clone, PartialEq)]
enum RoutineSave {
    Saving { attempt: Uuid, enabled: bool },
    Dismissing { attempt: Uuid },
    Model { attempt: Uuid },
    Deleting { attempt: Uuid },
    Failed(String),
}

impl RoutineSave {
    fn busy(&self) -> bool {
        matches!(
            self,
            Self::Saving { .. }
                | Self::Dismissing { .. }
                | Self::Model { .. }
                | Self::Deleting { .. }
        )
    }

    fn attempt(&self) -> Option<Uuid> {
        match self {
            Self::Saving { attempt, .. }
            | Self::Dismissing { attempt }
            | Self::Model { attempt }
            | Self::Deleting { attempt } => Some(*attempt),
            Self::Failed(_) => None,
        }
    }
}

enum TriggerSave {
    Saving { attempt: Uuid, enabled: bool },
    Deleting { attempt: Uuid },
    Failed(String),
}

impl TriggerSave {
    fn attempt(&self) -> Option<Uuid> {
        match self {
            Self::Saving { attempt, .. } | Self::Deleting { attempt } => Some(*attempt),
            Self::Failed(_) => None,
        }
    }

    fn busy(&self) -> bool {
        self.attempt().is_some()
    }
}

pub(super) struct BotSettings {
    details: Details,
    /// The bot's `metadata_revision` when its profile was last loaded: a
    /// bot can change another bot's model, which publishes `bot.updated`.
    revision: Option<i64>,
    model: ModelState,
    routines: HashMap<Uuid, RoutineSave>,
    /// Trigger changes by [`TriggerSource::key`].
    triggers: HashMap<Uuid, TriggerSave>,
}

impl ElseApp {
    pub(super) fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open.toggle();
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
        if !self.settings_open {
            self.computer = None;
            self._computer_layout = None;
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
        let details = new_details(&bot, window, cx);
        self.settings.insert(
            bot.id,
            BotSettings {
                details,
                revision: bot.metadata_revision,
                model: ModelState::Loading(Uuid::nil()),
                routines: HashMap::new(),
                triggers: HashMap::new(),
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
                entry
                    .details
                    .sync
                    .remote(Metadata::of(bot), bot.metadata_revision);
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
        let request = self
            .client
            .clone()
            .set_routine_enabled(bot, routine, enabled);
        let change = RoutineSave::Saving { attempt, enabled };
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

    pub(super) fn sync_trigger_changes(&mut self) {
        if let Some(view) = &self.view
            && let Some(conversation) = &view.conversation
            && let Some(entry) = self.settings.get_mut(&view.bot_id)
        {
            let cards = trigger_cards(conversation);
            entry.triggers.retain(|key, save| {
                let enabled = cards
                    .iter()
                    .find(|card| card.source.key() == *key)
                    .map(|card| card.enabled);
                still_pending(save, enabled)
            });
        }
    }

    fn changeable_trigger(&self, bot: Uuid, source: TriggerSource) -> Option<TriggerCard> {
        if self.connection != Connection::Connected {
            return None;
        }
        let entry = self.settings.get(&bot)?;
        if entry
            .triggers
            .get(&source.key())
            .is_some_and(TriggerSave::busy)
        {
            return None;
        }
        let conversation = self
            .view
            .as_ref()
            .filter(|view| view.bot_id == bot)
            .and_then(|view| view.conversation.as_ref())?;
        trigger_cards(conversation)
            .into_iter()
            .find(|card| card.source == source)
    }

    /// Pause or resume a trigger, or delete it with `None`.
    fn change_trigger(
        &mut self,
        bot: Uuid,
        source: TriggerSource,
        enabled: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.changeable_trigger(bot, source).is_none() {
            return;
        }
        let attempt = Uuid::new_v4();
        let change = match enabled {
            Some(enabled) => TriggerSave::Saving { attempt, enabled },
            None => TriggerSave::Deleting { attempt },
        };
        let Some(entry) = self.settings.get_mut(&bot) else {
            return;
        };
        entry.triggers.insert(source.key(), change);
        cx.notify();
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = match (source, enabled) {
                (TriggerSource::Mail { account }, Some(wake)) => {
                    client.set_email_wake(account, bot, wake).await
                }
                (TriggerSource::Mail { account }, None) => {
                    client.delete_email_trigger(account, bot).await
                }
                (TriggerSource::Social { id }, Some(enabled)) => client
                    .set_social_trigger_enabled(bot, id, enabled)
                    .await
                    .map(|_| ()),
                (TriggerSource::Social { id }, None) => client.delete_social_trigger(bot, id).await,
                (TriggerSource::Webhook { id }, Some(enabled)) => {
                    client.set_webhook_enabled(bot, id, enabled).await
                }
                (TriggerSource::Webhook { id }, None) => client.delete_webhook(bot, id).await,
            };
            let _ = this.update(cx, |app, cx| {
                let Some(entry) = app.settings.get_mut(&bot) else {
                    return;
                };
                if entry
                    .triggers
                    .get(&source.key())
                    .and_then(TriggerSave::attempt)
                    != Some(attempt)
                {
                    return;
                }
                if let Err(error) = result {
                    let action = if enabled.is_some() { "save" } else { "delete" };
                    entry.triggers.insert(
                        source.key(),
                        TriggerSave::Failed(format!("Couldn't {action}. {error}")),
                    );
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn open_trigger_delete_dialog(
        &mut self,
        bot: Uuid,
        source: TriggerSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(card) = self.changeable_trigger(bot, source) else {
            return;
        };
        let target = match source {
            TriggerSource::Webhook { .. } => DeleteTarget::Webhook(card.title),
            TriggerSource::Mail { .. } | TriggerSource::Social { .. } => {
                DeleteTarget::Trigger(card.delete_name)
            }
        };
        self.open_delete_confirmation(target, window, cx, move |app, window, cx| {
            app.change_trigger(bot, source, None, window, cx);
        });
    }

    fn open_model_dialog(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ModelState::Ready(saved)) = self.settings.get(&bot).map(|s| &s.model) else {
            return;
        };
        let saved = saved.as_ref().map(|p| p.choice.clone());
        self.open_model_editor(bot, ModelTarget::Bot, saved, window, cx);
    }

    fn open_routine_model_dialog(
        &mut self,
        bot: Uuid,
        routine: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(routine) = self.changeable_routine(bot, routine) else {
            return;
        };
        let target = ModelTarget::Routine {
            id: routine.id,
            name: routine.name.clone(),
            use_bot_model: routine.model.is_none(),
            unresolved: routine.requested_model.is_some(),
        };
        let saved = routine.model.clone();
        self.open_model_editor(bot, target, saved, window, cx);
    }

    fn open_model_editor(
        &mut self,
        bot: Uuid,
        target: ModelTarget,
        saved: Option<ProfileChoice>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(caps) = self.caps.clone() else {
            return;
        };
        let initial = saved.as_ref().or_else(|| {
            self.settings.get(&bot).and_then(|s| match &s.model {
                ModelState::Ready(Some(profile)) => Some(&profile.choice),
                _ => None,
            })
        });
        let picker = cx.new(|_| ProfilePicker::new(&caps, initial));
        let observer = cx.observe_in(&picker, window, |_, _, window, _| window.refresh());
        let holder = cx.new(|_| ModelDialog {
            bot,
            target,
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
            let ready = form.change(cx).is_some();
            let routine = form.routine_id().is_some();
            let inherit_holder = holder.clone();
            let inheritance = routine.then(|| {
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        v_flex().flex_1().gap_1()
                            .child(div().text_sm().child("Use bot model"))
                            .child(muted("Follow the bot's model, including future changes.", cx)),
                    )
                    .child(
                        toggle("routine-inherit-model", form.inherits(), saving || !connected)
                            .accessibility_label("Use bot model")
                            .on_click(move |enabled: &bool, window, cx| {
                                inherit_holder.update(cx, |form, cx| {
                                    if let ModelTarget::Routine { use_bot_model, .. } = &mut form.target {
                                        *use_bot_model = *enabled;
                                        cx.notify();
                                    }
                                });
                                window.refresh();
                            }),
                    )
            });
            let body = v_flex()
                .gap_3()
                .children(match &form.target {
                    ModelTarget::Routine { name, .. } => Some(div().text_sm().truncate().child(name.clone())),
                    ModelTarget::Bot => None,
                })
                .children(inheritance)
                .when(!form.inherits(), |body| body.child(picker.clone()))
                .child(muted(
                    if routine {
                        "Applies to future runs. Runs already queued or running keep their model."
                    } else {
                        "Changes apply from the bot's next message; work already running is unaffected."
                    },
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
                .title(if routine { "Routine model" } else { "Change model" })
                .w(px(640.))
                .on_ok(move |_, window, cx| {
                    let _ = app_ok.update(cx, |app, cx| app.apply_model(window, cx));
                    false
                })
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
                                .control(Tier::Secondary, Scale::Standard, false, cx)
                                .label("Cancel")
                                                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("model-apply")
                                .label("Apply")
                                .control(Tier::Primary, Scale::Standard, !ready || saving || !connected, cx)
                                .loading(saving)
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
        let (bot, routine, picker, change) = {
            let form = holder.read(cx);
            let Some(change) = form.change(cx) else {
                return;
            };
            (form.bot, form.routine_id(), form.picker.clone(), change)
        };
        let attempt = Uuid::new_v4();
        if let Some(routine) = routine
            && !self.begin_routine_change(bot, routine, RoutineSave::Model { attempt })
        {
            holder.update(cx, |form, cx| {
                form.save = Save::Failed("This routine can't be changed right now. Reopen its model settings and try again.".into());
                cx.notify();
            });
            window.refresh();
            return;
        }
        picker.update(cx, |picker, cx| picker.set_disabled(true, cx));
        holder.update(cx, |form, cx| {
            form.save = Save::Saving(attempt);
            cx.notify();
        });
        window.refresh();
        cx.spawn_in(window, async move |this, cx| {
            let result = match change {
                ModelChange::Routine { id, choice } => client
                    .set_routine_model(bot, id, choice)
                    .await
                    .map(|saved| SavedModel::Routine(Box::new(saved))),
                ModelChange::Bot(choice) => client
                    .set_bot_profile(bot, choice)
                    .await
                    .map(SavedModel::Bot),
            };
            let _ = this.update_in(cx, |app, window, cx| {
                if holder.read(cx).save != Save::Saving(attempt) {
                    return;
                }
                if let Some(routine) = routine
                    && let Some(entry) = current_change(&mut app.settings, bot, routine, attempt)
                {
                    entry.routines.remove(&routine);
                }
                match result {
                    Ok(saved) => {
                        match saved {
                            SavedModel::Bot(saved) => {
                                if let Some(entry) = app.settings.get_mut(&bot)
                                    && matches!(entry.model, ModelState::Ready(_))
                                {
                                    entry.model = ModelState::Ready(Some(saved));
                                }
                            }
                            SavedModel::Routine(saved) => {
                                if let Some(conversation) = app
                                    .view
                                    .as_mut()
                                    .filter(|view| view.bot_id == bot)
                                    .and_then(|view| view.conversation.as_mut())
                                {
                                    conversation.apply_routine(*saved);
                                }
                            }
                        }
                        if app.model_dialog.as_ref() == Some(&holder) {
                            app.model_dialog = None;
                            window.close_dialog(cx);
                        }
                        cx.notify();
                    }
                    Err(err) => {
                        if let Some(routine) = routine
                            && let Some(entry) = app.settings.get_mut(&bot)
                        {
                            entry.routines.insert(
                                routine,
                                RoutineSave::Failed(format!("Couldn't save. {err}")),
                            );
                        }
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

    pub(super) fn render_settings(
        &self,
        bot: &Bot,
        viewport: Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let line = hairline(cx);
        // Same height and rule as the conversation header, so the two
        // dividers meet as one line across the window.
        let header = h_flex()
            .flex_none()
            .h(HEADER_HEIGHT)
            .px_4()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(line)
            .child(
                div()
                    .flex_1()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Bot details"),
            )
            .child(
                Button::new("close-settings")
                    .focus_ring(false)
                    .cursor_pointer()
                    .icon(IconName::PanelRightClose)
                    .small()
                    .ghost()
                    .accessibility_label("Hide bot details")
                    .tooltip_with_action("Hide bot details", &ToggleSettings, Some(APP_CONTEXT))
                    .on_click(cx.listener(|app, _, window, cx| app.toggle_settings(window, cx))),
            );
        let mut body = v_flex()
            .id("bot-settings-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        if let Some(computer) = &self.computer {
            body = body.child(v_flex().flex_1().p_4().child(computer.clone()));
        }
        if self.details_tab == super::computer::DetailsTab::Details {
            if let Some(entry) = self.settings.get(&bot.id) {
                body = body
                    .child(self.render_details(bot.id, &entry.details, cx))
                    .child(self.render_model(bot.id, &entry.model, cx))
                    .child(self.render_rules_section(bot.id, cx))
                    .child(self.render_routines(bot.id, &entry.routines, cx))
                    .child(self.render_reminders(bot.id, &entry.routines, cx))
                    .child(self.render_triggers(bot.id, entry, cx));
            } else {
                body = body.child(muted("Loading…", cx));
            }
        }
        let mut panel = v_flex()
            .id("bot-settings")
            .relative()
            .w(px(self.preferences.details_width).min((viewport - px(360.)).max(px(280.))))
            .on_drag_move(cx.listener(Self::resize_sidebar))
            .flex_none()
            .h_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_l_1()
            .border_color(line)
            .child(header)
            .child(
                h_flex()
                    .flex_none()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(line)
                    .child(
                        segment_track(cx)
                            .w_full()
                            .child(
                                segment(
                                    "details-tab",
                                    "Details",
                                    self.details_tab == super::computer::DetailsTab::Details,
                                    false,
                                    cx,
                                )
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
                                segment(
                                    "computer-tab",
                                    "Computer",
                                    self.details_tab == super::computer::DetailsTab::Computer,
                                    false,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    |app, _, window, cx| {
                                        app.details_tab = super::computer::DetailsTab::Computer;
                                        app.ensure_settings(window, cx);
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
            )
            .child(body)
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

    fn render_details(&self, bot: Uuid, details: &Details, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let name = if details.editing {
            div()
                .id("bot-name-editor")
                .w_full()
                .on_mouse_down_out(cx.listener(move |app, _, _, cx| app.commit_name(bot, cx)))
                .child(
                    text_field(Input::new(&details.name), cx)
                        .text_left()
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .into_any_element()
        } else {
            let hover = theme.sidebar_accent;
            div()
                .id("bot-name")
                .max_w_full()
                .px_2()
                .py_1()
                .text_sm()
                .rounded(theme.radius)
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .font_weight(FontWeight::SEMIBOLD)
                .text_left()
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
                        .control(Tier::Secondary, Scale::Standard, false, cx)
                        .label("Retry")
                        .on_click(cx.listener(move |app, _, _, cx| app.flush_details(bot, cx))),
                )
        });
        // A compact row, not a hero: the conversation header already
        // presents the bot. This is where its avatar and name are edited.
        h_flex()
            .w_full()
            .px_4()
            .py_3()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(hairline(cx))
            .child(self.render_avatar_editor(bot, details, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(name)
                    .children(problem)
                    .children(error),
            )
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
            .right(px(-2.))
            .bottom(px(-2.))
            .size(px(18.))
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
                Icon::new(gpui_kit::assets::IconName::PencilLine).with_size(Size::Size(px(10.))),
            );
        let hero = Button::new("edit-avatar")
            .focus_ring(false)
            .cursor_pointer()
            .ghost()
            .w(px(36.))
            .h(px(36.))
            .p_0()
            .rounded(theme.radius_lg)
            .accessibility_label(editing)
            .toggled(details.picking)
            .child(
                div()
                    .relative()
                    .group("avatar-editor")
                    .child(bot_avatar(
                        ElementId::Name(format!("settings-avatar-{bot}").into()),
                        details.sync.desired.avatar,
                        px(28.),
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
                let app = app.clone();
                AvatarPicker::new(current, move |avatar, _, cx| {
                    let _ = app.update(cx, |app, cx| app.pick_avatar(bot, avatar, cx));
                })
                .into_any_element()
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
                                .control(Tier::Secondary, Scale::Standard, false, cx)
                                .label("Retry")
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
            None => v_flex().child(muted("No model yet. Choose a bot model.", cx)),
            Some(saved) => {
                let choice = &saved.choice;
                let speed = if choice.service_tier == "priority" {
                    "Fast"
                } else {
                    "Standard speed"
                };
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(model_label(&choice.model)),
                    )
                    .child(meta(
                        format!(
                            "{} · {} reasoning · {speed}",
                            self.provider_label(&choice.provider_id),
                            effort_label(&choice.reasoning_effort),
                        ),
                        cx,
                    ))
            }
        };
        // The warning sits under the button: a button keeps its label on one
        // line, which would clip the sentence.
        let unavailable = saved
            .is_some_and(|saved| !self.model_available(&saved.choice))
            .then(|| {
                alert(
                    "Saved model is unavailable. Choose another model.",
                    cx.theme().danger,
                    cx,
                )
            });
        // Button centers and nowraps its content; the card reads as a left-aligned block.
        let button = Button::new("change-model")
            .focus_ring(false)
            .cursor_pointer()
            .ghost()
            .with_size(Size::Size(px(16.)))
            .mx(-ROW_BLEED)
            .px(ROW_BLEED)
            .py_2()
            .accessibility_label("Change model")
            .disabled(self.caps.is_none() || self.connection != Connection::Connected)
            .child(
                content
                    .flex_1()
                    .min_w_0()
                    .items_start()
                    .text_left()
                    .whitespace_normal(),
            )
            .child(
                Icon::new(IconName::ChevronRight)
                    .flex_none()
                    .text_color(cx.theme().muted_foreground),
            )
            .on_click(
                cx.listener(move |app, _, window, cx| app.open_model_dialog(bot, window, cx)),
            );
        v_flex()
            .w_full()
            .child(button)
            .children(unavailable)
            .into_any_element()
    }

    fn model_available(&self, choice: &ProfileChoice) -> bool {
        self.caps.as_ref().is_none_or(|caps| {
            caps.providers.iter().any(|provider| {
                provider.id == choice.provider_id
                    && provider.cli_available
                    && provider.catalog.models.contains(&choice.model)
                    && provider.catalog.efforts.contains(&choice.reasoning_effort)
                    && provider.catalog.tiers.contains(&choice.service_tier)
            })
        })
    }
}

impl ElseApp {
    fn render_triggers(
        &self,
        bot: Uuid,
        entry: &BotSettings,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = section("Triggers", cx);
        let Some(conversation) = self
            .view
            .as_ref()
            .filter(|view| view.bot_id == bot)
            .and_then(|view| view.conversation.as_ref())
        else {
            return body.child(muted("Loading…", cx)).into_any_element();
        };
        let cards = trigger_cards(conversation);
        if cards.is_empty() {
            return body
                .child(muted(
                    "No triggers yet. Ask the bot in chat to react to something, for example \
                     \"wake up when new mail arrives from my accountant\", \"answer mentions in \
                     our Telegram group\", or \"give me a webhook for failed deploys\". Its \
                     triggers appear here.",
                    cx,
                ))
                .into_any_element();
        }
        let rows = cards
            .into_iter()
            .map(|card| {
                let save = entry.triggers.get(&card.source.key());
                self.render_trigger(bot, card, save, cx)
            })
            .collect::<Vec<_>>();
        body.children(list_rows(rows, cx)).into_any_element()
    }

    fn render_trigger(
        &self,
        bot: Uuid,
        card: TriggerCard,
        save: Option<&TriggerSave>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let source = card.source;
        let key = source.key();
        let checked = match save {
            Some(TriggerSave::Saving { enabled, .. }) => *enabled,
            _ => card.enabled,
        };
        let disabled =
            save.is_some_and(TriggerSave::busy) || self.connection != Connection::Connected;
        let switch = toggle(
            ElementId::Name(format!("trigger-{key}").into()),
            checked,
            disabled,
        )
        .accessibility_label(format!("Run the trigger for {}", card.title))
        .on_click(cx.listener(move |app, enabled: &bool, window, cx| {
            app.change_trigger(bot, source, Some(*enabled), window, cx)
        }));
        let delete = Button::new(ElementId::Name(format!("trigger-delete-{key}").into()))
            .focus_ring(false)
            .cursor_pointer()
            .ghost()
            .opacity(0.)
            .group_hover("automation-card", |style| style.opacity(1.))
            .focus(|style| style.opacity(1.))
            .xsmall()
            .icon(gpui_kit::assets::IconName::Trash)
            .accessibility_label(format!("Delete the trigger for {}", card.title))
            .loading(matches!(save, Some(TriggerSave::Deleting { .. })))
            .disabled(disabled)
            .on_click(cx.listener(move |app, _, window, cx| {
                app.open_trigger_delete_dialog(bot, source, window, cx)
            }));
        let error = match save {
            Some(TriggerSave::Failed(error)) => Some(error.clone()),
            _ => None,
        };
        let mut facts = card.pills;
        if !checked {
            facts.push("Paused".to_owned());
        }
        if let Some(at) = card.last_woke {
            facts.push(format!(
                "Last woke {}",
                at.with_timezone(&chrono::Local).format("%a %-d %b, %H:%M")
            ));
        }
        v_flex()
            .w_full()
            .py_3()
            .gap_0p5()
            .group("automation-card")
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .whitespace_normal()
                            .child(card.title.clone()),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .items_center()
                            .child(delete)
                            .child(switch),
                    ),
            )
            .child(meta(card.instructions, cx))
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(card.icon).size_3().flex_none())
                    .child(meta(facts.join(" · "), cx)),
            )
            .children(error.map(|error| alert(error, theme.danger, cx)))
            .into_any_element()
    }

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
            .map(|routine| self.render_routine(bot, routine, saves.get(&routine.id), connected, cx))
            .collect::<Vec<_>>();
        body.children(list_rows(rows, cx)).into_any_element()
    }

    fn render_reminders(
        &self,
        bot: Uuid,
        saves: &HashMap<Uuid, RoutineSave>,
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
                self.render_routine(bot, reminder, saves.get(&reminder.id), connected, cx)
            })
            .collect::<Vec<_>>();
        body.children(list_rows(rows, cx)).into_any_element()
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
        let passed = !checked && routine.run_at.is_some_and(|at| at <= Utc::now());
        let switch = toggle(
            ElementId::Name(format!("routine-{id}").into()),
            checked,
            busy || !connected || passed,
        )
        .accessibility_label(format!("Run {}", routine.name))
        .on_click(cx.listener(move |app, enabled: &bool, window, cx| {
            app.set_routine(bot, id, *enabled, window, cx)
        }));
        let delete = Button::new(ElementId::Name(format!("routine-delete-{id}").into()))
            .focus_ring(false)
            .cursor_pointer()
            .icon(gpui_kit::assets::IconName::Trash)
            .ghost()
            .opacity(0.)
            .group_hover("automation-card", |style| style.opacity(1.))
            .focus(|style| style.opacity(1.))
            .xsmall()
            .loading(matches!(save, Some(RoutineSave::Deleting { .. })))
            .accessibility_label(format!("Delete {}", routine.name))
            .disabled(busy || !connected)
            .on_click(cx.listener(move |app, _, window, cx| {
                app.open_routine_delete_dialog(bot, id, window, cx)
            }));
        let dismiss = (routine.failed_runs > 0).then(|| {
            Button::new(ElementId::Name(format!("routine-dismiss-{id}").into()))
                .control(Tier::Ghost, Scale::Compact, busy || !connected, cx)
                .label("Dismiss")
                .loading(matches!(save, Some(RoutineSave::Dismissing { .. })))
                .accessibility_label(format!("Dismiss the failures of {}", routine.name))
                .on_click(cx.listener(move |app, _, window, cx| {
                    app.dismiss_routine_failures(bot, id, window, cx)
                }))
        });
        let schedule = match (routine.run_at, &routine.active_hours) {
            (Some(at), _) => format!(
                "Once · {} · {}",
                local_time(at, &routine.timezone),
                routine.timezone
            ),
            (None, Some(hours)) => format!(
                "{} · {} · {}",
                describe_cron(&routine.cron),
                describe_hours(hours),
                routine.timezone
            ),
            (None, None) => format!("{} · {}", describe_cron(&routine.cron), routine.timezone),
        };
        let last = last_run(routine);
        let problem = match save {
            Some(RoutineSave::Failed(err)) => Some(err.clone()),
            _ => last
                .clone()
                .filter(|(_, error)| *error)
                .map(|(text, _)| text),
        };
        let choice = routine.model.as_ref().or_else(|| {
            self.settings
                .get(&bot)
                .and_then(|entry| match &entry.model {
                    ModelState::Ready(Some(profile)) => Some(&profile.choice),
                    _ => None,
                })
        });
        let model_title = match (&routine.model, choice) {
            (Some(choice), _) => model_label(&choice.model),
            (None, Some(choice)) => format!("Bot model · {}", model_label(&choice.model)),
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
        let theme = cx.theme();
        let warning = routine
            .requested_model
            .as_ref()
            .map(|requested| format!("Couldn't use \"{requested}\". Using bot model."))
            .or_else(|| {
                unavailable.then(|| "Saved model is unavailable. Choose another model.".to_owned())
            });
        let model_summary = choice
            .map(|choice| {
                format!(
                    "{} · {} · {}",
                    model_label(&choice.model),
                    effort_label(&choice.reasoning_effort),
                    if choice.service_tier == "priority" {
                        "Fast"
                    } else {
                        "Standard"
                    }
                )
            })
            .unwrap_or_else(|| "Choose a model".to_owned());
        let model_control = Button::new(ElementId::Name(format!("routine-model-{id}").into()))
            .focus_ring(false)
            .cursor_pointer()
            .ghost()
            .with_size(Size::Size(px(16.)))
            .mx(-ROW_BLEED)
            .px(ROW_BLEED)
            .py_0p5()
            .accessibility_label(format!(
                "Change model for {}. {model_title}. {model_details}",
                routine.name
            ))
            .loading(matches!(save, Some(RoutineSave::Model { .. })))
            .disabled(busy || !connected || self.caps.is_none())
            .child(
                meta(model_summary, cx)
                    .w_full()
                    .min_w_0()
                    .text_left()
                    .truncate(),
            )
            .on_click(cx.listener(move |app, _, window, cx| {
                app.open_routine_model_dialog(bot, id, window, cx)
            }));
        let summary = v_flex()
            .id(ElementId::Name(format!("routine-pill-{id}").into()))
            .w_full()
            .gap_0p5()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .whitespace_normal()
                    .child(routine.name.clone()),
            )
            .child(meta(schedule.clone(), cx))
            .children(problem.map(|text| alert(text, theme.danger, cx)));
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
        let theme = cx.theme();
        v_flex()
            .w_full()
            .py_3()
            .gap_0p5()
            .group("automation-card")
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
                            .children(dismiss)
                            .child(delete)
                            .child(switch),
                    ),
            )
            .when(routine.run_at.is_none(), |card| card.child(model_control))
            .children(warning.map(|warning| alert(warning, theme.warning, cx)))
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

const MONTHS: [(&str, u8); 12] = [
    ("January", 31),
    ("February", 29),
    ("March", 31),
    ("April", 30),
    ("May", 31),
    ("June", 30),
    ("July", 31),
    ("August", 31),
    ("September", 30),
    ("October", 31),
    ("November", 30),
    ("December", 31),
];

const WEEKDAYS: [&str; 7] = [
    "Sundays",
    "Mondays",
    "Tuesdays",
    "Wednesdays",
    "Thursdays",
    "Fridays",
    "Saturdays",
];

/// A short phrase for the common schedules. Any other schedule the server
/// accepts shows croner's description beside its expression, because that
/// description can blur cron's rules (`*/7` restarts every hour). Any other
/// expression shows as written.
fn describe_cron(expr: &str) -> String {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    let server_grammar = fields.len() == 5
        && fields.iter().all(|field| {
            field
                .chars()
                .all(|c| c.is_ascii_digit() || "*,-/".contains(c))
        });
    fields
        .try_into()
        .ok()
        .and_then(describe_cron_fields)
        .or_else(|| {
            if !server_grammar {
                return None;
            }
            use croner::parser::{CronParser, Seconds, Year};
            let parser = CronParser::builder()
                .seconds(Seconds::Disallowed)
                .year(Year::Disallowed)
                .build();
            let cron = parser.parse(expr).ok()?;
            let description = cron.describe();
            Some(format!(
                "{} ({})",
                description.trim_end_matches('.'),
                expr.trim()
            ))
        })
        .unwrap_or_else(|| expr.to_owned())
}

fn describe_cron_fields([minute, hour, day, month, weekday]: [&str; 5]) -> Option<String> {
    let number = |field: &str, min: u8, max: u8| {
        field
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| field.parse::<u8>().ok())
            .flatten()
            .filter(|n| (min..=max).contains(n))
    };
    let step = |field: &str, cycle: u8| match field {
        "*" => Some(1),
        _ => number(field.strip_prefix("*/")?, 1, cycle - 1).filter(|n| cycle.is_multiple_of(*n)),
    };
    let every = |n: u8, unit: &str| match n {
        1 => format!("Every {unit}"),
        _ => format!("Every {n} {unit}s"),
    };
    let unrestricted = (day, month, weekday) == ("*", "*", "*");
    if unrestricted
        && hour == "*"
        && let Some(n) = step(minute, 60)
    {
        return Some(every(n, "minute"));
    }
    let minute = number(minute, 0, 59)?;
    if unrestricted && let Some(n) = step(hour, 24) {
        let at = match minute {
            0 => String::new(),
            _ => format!(" at :{minute:02}"),
        };
        return Some(format!("{}{at}", every(n, "hour")));
    }
    let at = format!("{:02}:{minute:02}", number(hour, 0, 23)?);
    match (day, month, weekday) {
        ("*", "*", "*") => Some(format!("Daily at {at}")),
        ("*", "*", "1-5") => Some(format!("Weekdays at {at}")),
        ("*", "*", weekday) => {
            let weekday = WEEKDAYS[usize::from(number(weekday, 0, 7)? % 7)];
            Some(format!("{weekday} at {at}"))
        }
        (day, "*", "*") => Some(format!("Monthly on day {} at {at}", number(day, 1, 31)?)),
        (day, month, "*") => {
            let (name, days) = MONTHS[usize::from(number(month, 1, 12)? - 1)];
            Some(format!(
                "Yearly on {} {name} at {at}",
                number(day, 1, days)?
            ))
        }
        _ => None,
    }
}

fn describe_hours(hours: &crate::model::ActiveHours) -> String {
    let has = |names: &[&str]| {
        hours.days.len() == names.len() && names.iter().all(|n| hours.days.iter().any(|d| d == n))
    };
    let days = if has(&["mon", "tue", "wed", "thu", "fri"]) {
        "Weekdays".to_owned()
    } else if has(&["sat", "sun"]) {
        "Weekends".to_owned()
    } else if hours.days.len() == 7 {
        "Every day".to_owned()
    } else {
        hours
            .days
            .iter()
            .map(|day| {
                let mut chars = day.chars();
                chars
                    .next()
                    .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!("{days} {} to {}", hours.start, hours.end)
}

fn last_run(routine: &Routine) -> Option<(String, bool)> {
    let run = routine.last_run.as_ref()?;
    let when = local_time(run.due_at, &routine.timezone);
    let error = run.error.as_ref().and_then(|e| e.message.clone());
    if !routine.enabled
        && let Some(reason) = &routine.paused_reason
    {
        let why = match reason.as_str() {
            "repeated" => format!("Paused after {} failed runs", routine.failed_runs),
            _ => "Paused: retrying won't fix the last failure".to_owned(),
        };
        let detail = error.map_or_else(String::new, |e| format!(" ({e})"));
        return Some((format!("{why}{detail}"), true));
    }
    let (text, attention) = last_run_status(run, &when, error);
    Some((
        text,
        attention && (routine.failed_runs > 0 || run.status == "skipped"),
    ))
}

fn last_run_status(run: &LastRun, when: &str, error: Option<String>) -> (String, bool) {
    let reason = |code: &str| match code {
        "overlap" => "the previous run was still going",
        "queue_full" => "too many messages were waiting",
        "provider_unavailable" => "no provider was available",
        "check_cancelled" => "its check was stopped",
        "check_unavailable" => "its check could not start",
        _ => "it could not start",
    };
    match run.status.as_str() {
        "skipped" if run.skip_reason.as_deref() == Some("nothing_new") => {
            (format!("Last run {when}: nothing new"), false)
        }
        "failed" | "uncertain" | "interrupted" => {
            let detail = error.unwrap_or_else(|| run.status.clone());
            (format!("Last run {when} {}: {detail}", run.status), true)
        }
        "skipped" => (
            format!(
                "Skipped {when}: {}",
                reason(run.skip_reason.as_deref().unwrap_or_default())
            ),
            true,
        ),
        "checking" => (format!("Checking for new work (due {when})"), false),
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

/// The 1 px rule between the panel's header, tabs, sections and rows. It
/// matches the conversation header's rule so both sides read as one line.
pub(super) fn hairline(cx: &App) -> Hsla {
    cx.theme().foreground.opacity(0.06)
}

/// How far a clickable row's hover fill reaches past the text column.
const ROW_BLEED: Pixels = px(8.);

/// A titled group of rows in the Details panel: no box, just a label and
/// a rule under the group.
pub(super) fn section_frame(cx: &App) -> Div {
    v_flex()
        .w_full()
        .px_4()
        .pt_4()
        .pb_2()
        .border_b_1()
        .border_color(hairline(cx))
}

fn section(title: &'static str, cx: &App) -> Div {
    section_frame(cx).child(crate::appearance::section_label(title, cx).pb_1())
}

/// Rows separated by hairlines, without a rule above the first.
fn list_rows(rows: Vec<AnyElement>, cx: &App) -> impl Iterator<Item = Div> {
    let line = hairline(cx);
    rows.into_iter().enumerate().map(move |(index, row)| {
        div()
            .w_full()
            .when(index > 0, |row| row.border_t_1().border_color(line))
            .child(row)
    })
}

/// Secondary text in a row: a schedule, a model, a trigger's details. It
/// wraps instead of truncating.
fn meta(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_size(px(12.5))
        .line_height(px(18.))
        .text_color(cx.theme().muted_foreground)
        .whitespace_normal()
        .child(text.into())
}

/// A problem in a row, with a small warning icon.
fn alert(text: impl Into<SharedString>, color: Hsla, _cx: &App) -> Div {
    h_flex()
        .pt_0p5()
        .gap_1p5()
        .items_start()
        .text_size(px(12.5))
        .line_height(px(18.))
        .text_color(color)
        .child(
            Icon::new(IconName::TriangleAlert)
                .size_3()
                .mt(px(3.))
                .flex_none(),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .whitespace_normal()
                .child(text.into()),
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

/// What a trigger card changes: each kind has its own routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriggerSource {
    Mail { account: Uuid },
    Social { id: Uuid },
    Webhook { id: Uuid },
}

impl TriggerSource {
    /// Unique across kinds, since every ID is a random UUID.
    fn key(self) -> Uuid {
        match self {
            Self::Mail { account } => account,
            Self::Social { id } | Self::Webhook { id } => id,
        }
    }
}

/// One trigger as its sidebar card shows it.
struct TriggerCard {
    source: TriggerSource,
    icon: gpui_kit::assets::IconName,
    title: String,
    /// How the delete dialog names it.
    delete_name: String,
    instructions: String,
    pills: Vec<String>,
    enabled: bool,
    last_woke: Option<DateTime<Utc>>,
}

/// Every trigger that can wake the bot: mail, then chats, then webhooks.
fn trigger_cards(conversation: &Conversation) -> Vec<TriggerCard> {
    let mail = conversation.mail_triggers().iter().map(|trigger| {
        let mut pills = vec!["Email".to_owned(), "New mail".to_owned()];
        if trigger.state == "paused" {
            pills.push("inbox paused".to_owned());
        }
        TriggerCard {
            source: TriggerSource::Mail {
                account: trigger.account_id,
            },
            icon: gpui_kit::assets::IconName::Mail,
            title: trigger.address.clone(),
            delete_name: format!("the trigger for {}", trigger.address),
            instructions: trigger
                .instructions
                .clone()
                .unwrap_or_else(|| "Wakes the bot when new mail arrives.".to_owned()),
            pills,
            enabled: trigger.wake,
            last_woke: trigger.last_woke_at,
        }
    });
    let social = conversation.social_triggers().iter().map(|trigger| {
        let connection = trigger
            .username
            .as_ref()
            .map(|username| format!("@{username}"))
            .unwrap_or_else(|| trigger.connection_name.clone());
        TriggerCard {
            source: TriggerSource::Social { id: trigger.id },
            icon: match trigger.provider {
                Provider::Telegram => gpui_kit::assets::IconName::Send,
                Provider::Discord => gpui_kit::assets::IconName::Gamepad2,
            },
            title: trigger.title().to_owned(),
            delete_name: match trigger.destination {
                Some(_) => format!("the trigger for {}", trigger.title()),
                None => "the trigger for any chat".to_owned(),
            },
            instructions: trigger.instructions.clone(),
            pills: vec![
                trigger.provider.label().to_owned(),
                connection,
                trigger.wake.label().to_owned(),
            ],
            enabled: trigger.enabled,
            last_woke: trigger.last_woke_at,
        }
    });
    let webhooks = conversation.webhooks().iter().map(|webhook| TriggerCard {
        source: TriggerSource::Webhook { id: webhook.id },
        icon: gpui_kit::assets::IconName::Webhook,
        title: webhook.name.clone(),
        delete_name: webhook.name.clone(),
        instructions: webhook.prompt.clone(),
        pills: vec!["Webhook".to_owned(), pace(webhook.min_interval_minutes)],
        enabled: webhook.enabled,
        last_woke: webhook.last_wake.as_ref().map(|wake| wake.woke_at),
    });
    mail.chain(social).chain(webhooks).collect()
}

/// How often a webhook may wake the bot, at most.
fn pace(minutes: i64) -> String {
    match minutes {
        1 => "at most every minute".to_owned(),
        m if m % 60 == 0 && m > 60 => format!("at most every {} hours", m / 60),
        60 => "at most hourly".to_owned(),
        m => format!("at most every {m} min"),
    }
}

/// Whether a trigger's local change still waits for the server: a save until
/// the trigger reports the saved state, a failure until the trigger is gone.
fn still_pending(save: &TriggerSave, current: Option<bool>) -> bool {
    match (save, current) {
        (_, None) => false,
        (TriggerSave::Saving { enabled, .. }, Some(current)) => *enabled != current,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Metadata, MetadataChange, MetadataSync, TriggerSave, describe_cron, describe_hours,
        last_run, pace, parse_name, still_pending, trigger_cards,
    };
    use crate::api::socials::Trigger;
    use crate::avatar::{Avatar, AvatarColor, AvatarShape};
    use crate::model::{Conversation, Snapshot};
    use serde_json::json;
    use uuid::Uuid;

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

    #[test]
    fn common_cron_schedules_read_as_prose() {
        for (cron, readable) in [
            ("0 */6 * * *", "Every 6 hours"),
            ("* * * * *", "Every minute"),
            ("*/1 * * * *", "Every minute"),
            ("*/15 * * * *", "Every 15 minutes"),
            ("0 * * * *", "Every hour"),
            ("0 */1 * * *", "Every hour"),
            ("30 * * * *", "Every hour at :30"),
            ("5 */2 * * *", "Every 2 hours at :05"),
            ("0 0 * * *", "Daily at 00:00"),
            ("59 23 * * *", "Daily at 23:59"),
            ("0 9 * * 1-5", "Weekdays at 09:00"),
            ("0 9 * * 1", "Mondays at 09:00"),
            ("0 9 * * 0", "Sundays at 09:00"),
            ("0 9 * * 7", "Sundays at 09:00"),
            ("0 9 15 * *", "Monthly on day 15 at 09:00"),
            ("0 9 1 1 *", "Yearly on 1 January at 09:00"),
            ("0 9 29 2 *", "Yearly on 29 February at 09:00"),
            ("  0\t*/6  * *   * ", "Every 6 hours"),
        ] {
            assert_eq!(describe_cron(cron), readable, "{cron}");
        }
    }

    #[test]
    fn active_hours_read_as_days_and_a_span() {
        let hours = |days: &[&str], start: &str, end: &str| crate::model::ActiveHours {
            days: days.iter().map(|d| (*d).to_owned()).collect(),
            start: start.to_owned(),
            end: end.to_owned(),
        };
        for (value, readable) in [
            (
                hours(&["mon", "tue", "wed", "thu", "fri"], "08:00", "18:00"),
                "Weekdays 08:00 to 18:00",
            ),
            (
                hours(&["sat", "sun"], "10:00", "12:00"),
                "Weekends 10:00 to 12:00",
            ),
            (
                hours(
                    &["mon", "tue", "wed", "thu", "fri", "sat", "sun"],
                    "07:00",
                    "22:00",
                ),
                "Every day 07:00 to 22:00",
            ),
            (
                hours(&["mon", "wed"], "09:00", "17:30"),
                "Mon, Wed 09:00 to 17:30",
            ),
        ] {
            assert_eq!(describe_hours(&value), readable);
        }
    }

    #[test]
    fn uncommon_schedules_show_a_description_beside_their_cron() {
        for (cron, readable) in [
            ("*/7 * * * *", "At every 7 minutes (*/7 * * * *)"),
            (
                "0 9 * * 1,3",
                "At 09:00, on Monday and Wednesday (0 9 * * 1,3)",
            ),
            ("0 9-17 * * *", "At minute 0, of hour 9-17 (0 9-17 * * *)"),
        ] {
            assert_eq!(describe_cron(cron), readable, "{cron}");
        }
    }

    #[test]
    fn schedules_croner_or_the_server_rejects_keep_their_cron() {
        for cron in [
            "@daily",
            "0 9 * * MON",
            "*/0 * * * *",
            "60 9 * * *",
            "0 24 * * *",
            "0 9 0 * *",
            "0 9 32 * *",
            "0 9 1 13 *",
            "0 9 * * 8",
            "+5 9 * * *",
            "0 9 * *",
            "0 9 * * * *",
            "",
        ] {
            assert_eq!(describe_cron(cron), cron);
        }
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
    }

    fn trigger(enabled: bool) -> Trigger {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::from_u128(1), "connection_id": Uuid::from_u128(2), "bot_id": Uuid::from_u128(3),
            "provider": "telegram", "connection_name": "Proliquid", "username": "proliquid_bot",
            "destination": null, "destination_name": null, "wake": "mentions",
            "instructions": "Answer questions.", "enabled": enabled,
            "created_at": "2026-11-01T07:00:00.123+00:00", "updated_at": "2026-11-01T07:00:00.123+00:00"
        }))
        .unwrap()
    }

    #[test]
    fn a_trigger_switch_stays_busy_until_the_server_reports_the_saved_state() {
        let saving = TriggerSave::Saving {
            attempt: Uuid::new_v4(),
            enabled: false,
        };
        assert!(
            still_pending(&saving, Some(trigger(true).enabled)),
            "a stale event keeps it busy"
        );
        assert!(!still_pending(&saving, Some(trigger(false).enabled)));
        let failed = TriggerSave::Failed("Couldn't save.".into());
        assert!(
            still_pending(&failed, Some(trigger(true).enabled)),
            "the error stays visible"
        );
        let deleting = TriggerSave::Deleting {
            attempt: Uuid::new_v4(),
        };
        assert!(
            !still_pending(&deleting, None),
            "a deleted trigger drops its state"
        );
        assert!(!still_pending(&failed, None));
    }

    #[test]
    fn every_trigger_kind_becomes_one_card_in_a_stable_order() {
        let snapshot: Snapshot = serde_json::from_value(json!({
            "event_cursor": "server:bot:1", "items": [], "turns": [],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 },
            "mail_triggers": [{
                "account_id": Uuid::from_u128(1), "address": "owner@example.com",
                "state": "paused", "health": "connected", "wake": false
            }],
            "social_triggers": [{
                "id": Uuid::from_u128(2), "connection_id": Uuid::from_u128(4),
                "bot_id": Uuid::from_u128(5), "provider": "telegram",
                "connection_name": "Team", "username": null, "destination": null,
                "destination_name": null, "wake": "mentions", "instructions": "Answer.",
                "enabled": true, "created_at": "2026-11-01T07:00:00Z",
                "updated_at": "2026-11-01T07:00:00Z"
            }],
            "webhooks": [{
                "id": Uuid::from_u128(3), "name": "Deploys", "prompt": "Open an issue.",
                "min_interval_minutes": 120, "enabled": true, "last_wake": null
            }]
        }))
        .unwrap();
        let conversation = Conversation::from_snapshot(snapshot).unwrap();
        let cards = trigger_cards(&conversation);
        let titles: Vec<_> = cards.iter().map(|card| card.title.as_str()).collect();
        assert_eq!(titles, ["owner@example.com", "Any chat", "Deploys"]);
        let mail = &cards[0];
        assert_eq!(mail.instructions, "Wakes the bot when new mail arrives.");
        assert_eq!(mail.pills, ["Email", "New mail", "inbox paused"]);
        assert!(!mail.enabled);
        assert_eq!(cards[1].delete_name, "the trigger for any chat");
        assert_eq!(cards[2].pills, ["Webhook", "at most every 2 hours"]);
        let keys: std::collections::HashSet<_> =
            cards.iter().map(|card| card.source.key()).collect();
        assert_eq!(keys.len(), 3);
    }

    #[test]
    fn webhook_paces_read_naturally() {
        assert_eq!(pace(1), "at most every minute");
        assert_eq!(pace(15), "at most every 15 min");
        assert_eq!(pace(60), "at most hourly");
        assert_eq!(pace(90), "at most every 90 min");
        assert_eq!(pace(1440), "at most every 24 hours");
    }
}
