use super::delete_dialog::DeleteTarget;
use super::view::HEADER_HEIGHT;
use super::{APP_CONTEXT, Connection, ElseApp, ToggleSettings};
use crate::controls::{Control as _, Scale, Tier, segment, segment_track, text_field, toggle};
use gpui_kit::component::FocusableExt as _;
use std::collections::HashMap;

use crate::api::socials::Provider;
use crate::api::{Bot, BotProfile, Capabilities, ModelInfo, ProfileChoice};
use crate::avatar::{Avatar, AvatarPicker, bot_avatar};
use crate::model::{Conversation, LastRun, Routine, Watcher, WatcherStatus};
use crate::profile_picker::{ProfilePicker, effort_label, model_label};
use chrono::{DateTime, Utc};
use gpui_kit::base::HoverCard;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
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
    /// Every routine, trigger and turn change moves the bot's revision and
    /// reloads its profile, so a reload keeps the shown profile on screen
    /// and the panel's sections don't jump.
    Refreshing {
        attempt: Uuid,
        shown: Option<BotProfile>,
    },
}

impl ModelState {
    fn shown(&self) -> Option<&Option<BotProfile>> {
        match self {
            Self::Ready(shown) | Self::Refreshing { shown, .. } => Some(shown),
            Self::Loading(_) | Self::Failed(_) => None,
        }
    }

    fn pending(&self) -> bool {
        matches!(self, Self::Loading(_) | Self::Refreshing { .. })
    }

    fn reload(&mut self, attempt: Uuid) {
        *self = match std::mem::replace(self, Self::Loading(attempt)) {
            Self::Ready(shown) | Self::Refreshing { shown, .. } => {
                Self::Refreshing { attempt, shown }
            }
            Self::Loading(_) | Self::Failed(_) => Self::Loading(attempt),
        };
    }

    /// Applies the answer to `attempt`, unless a later load superseded it.
    /// A failed refresh keeps the shown profile; the next revision retries.
    fn answer(&mut self, attempt: Uuid, result: Result<Option<BotProfile>, String>) -> bool {
        let next = match (&*self, result) {
            (Self::Loading(a) | Self::Refreshing { attempt: a, .. }, Ok(profile))
                if *a == attempt =>
            {
                Self::Ready(profile)
            }
            (Self::Loading(a), Err(err)) if *a == attempt => Self::Failed(err),
            (Self::Refreshing { attempt: a, shown }, Err(_)) if *a == attempt => {
                Self::Ready(shown.clone())
            }
            _ => return false,
        };
        *self = next;
        true
    }

    /// A saved profile is current, but a pending load stays pending so a
    /// change made elsewhere after the save still lands.
    fn saved(&mut self, profile: BotProfile) {
        match self {
            Self::Ready(_) | Self::Failed(_) => *self = Self::Ready(Some(profile)),
            Self::Refreshing { shown, .. } => *shown = Some(profile),
            Self::Loading(_) => {}
        }
    }
}

/// A one-click switch of a bot to the newer model of its family.
pub(super) enum ModelUpgrade {
    Saving { bot: Uuid },
    Failed { bot: Uuid, error: String },
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum WatcherStop {
    Stopping,
    Failed(String),
    Unconfirmed(String),
}

/// What a finished stop request leaves on the row. `None` keeps it
/// stopping until `bot.updated` drops the watcher: it stopped, or it had
/// already ended.
fn stop_result(result: Result<(), crate::api::ClientError>) -> Option<WatcherStop> {
    match result {
        Ok(()) => None,
        Err(err) if err.code() == Some("not_active") => None,
        Err(err) if err.outcome_unknown() => Some(WatcherStop::Unconfirmed(format!(
            "Couldn't confirm the stop. {err}"
        ))),
        Err(err) => Some(WatcherStop::Failed(format!("Couldn't stop. {err}"))),
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

    pub(super) fn show_watchers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.details_tab = super::computer::DetailsTab::Details;
        self.computer = None;
        self._computer_layout = None;
        self.details_scroll.scroll_to_item(WATCHING_SECTION);
        if !self.settings_open {
            self.toggle_settings(window, cx);
        } else {
            cx.notify();
        }
    }

    pub(super) fn watchers(&self, bot: Uuid) -> &[Watcher] {
        self.view
            .as_ref()
            .filter(|view| view.bot_id == bot)
            .and_then(|view| view.conversation.as_ref())
            .map_or(&[], Conversation::watchers)
    }

    pub(super) fn sync_watcher_clock(&mut self, cx: &mut Context<Self>) {
        let visible = self.settings_shown()
            && self.details_tab == super::computer::DetailsTab::Details
            && self
                .selected
                .is_some_and(|bot| !self.watchers(bot).is_empty());
        if !visible {
            self.watcher_clock = None;
        } else if self.watcher_clock.is_none() {
            self.watcher_clock = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(30))
                        .await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        }
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
        entry.model.reload(attempt);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.bot_profile(bot).await;
            let _ = this.update(cx, |app, cx| {
                let Some(entry) = app.settings.get_mut(&bot) else {
                    return;
                };
                if entry
                    .model
                    .answer(attempt, result.map_err(|err| err.to_string()))
                {
                    cx.notify();
                }
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

    fn stop_watcher(
        &mut self,
        bot: Uuid,
        watcher: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connection != Connection::Connected
            || !self.watchers(bot).iter().any(|active| active.id == watcher)
        {
            return;
        }
        let Some(entry) = self.settings.get_mut(&bot) else {
            return;
        };
        if matches!(
            entry.watcher_stops.get(&watcher),
            Some(WatcherStop::Stopping)
        ) {
            return;
        }
        entry.watcher_stops.insert(watcher, WatcherStop::Stopping);
        cx.notify();
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Some(state) = stop_result(client.stop_watcher(bot, watcher).await) else {
                return;
            };
            let _ = this.update(cx, |app, cx| {
                if let Some(entry) = app.settings.get_mut(&bot)
                    && let Some(stop) = entry.watcher_stops.get_mut(&watcher)
                {
                    *stop = state;
                    cx.notify();
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
            let active = |id| {
                conversation
                    .watchers()
                    .iter()
                    .any(|watcher| watcher.id == id)
            };
            entry.watcher_stops.retain(|id, _| active(*id));
            entry.open_watcher_log = entry.open_watcher_log.filter(|id| active(*id));
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
        let Some(saved) = self.settings.get(&bot).and_then(|s| s.model.shown()) else {
            return;
        };
        let saved = saved.as_ref().map(|p| p.choice.clone());
        self.open_model_editor(bot, ModelTarget::Bot, saved, window, cx);
    }

    /// Opens the bot's model picker with providers and the saved model read
    /// again, so a provider that just became unavailable shows why.
    pub(super) fn switch_model(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_open {
            self.toggle_settings(window, cx);
        }
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let (caps, saved) =
                futures_util::join!(client.clone().capabilities(), client.bot_profile(bot));
            let _ = this.update_in(cx, |app, window, cx| {
                if caps.is_ok() {
                    app.apply_caps(caps, window, cx);
                }
                if app.selected != Some(bot) {
                    return;
                }
                if let (Ok(saved), Some(entry)) = (saved, app.settings.get_mut(&bot)) {
                    entry.model = ModelState::Ready(saved);
                }
                app.open_model_dialog(bot, window, cx);
            });
        })
        .detach();
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
            self.settings
                .get(&bot)
                .and_then(|s| s.model.shown()?.as_ref())
                .map(|profile| &profile.choice)
        });
        let initial = initial.cloned();
        let picker = self.profile_picker(&caps, initial.as_ref(), cx);
        let picker = cx.new(|_| picker);
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
                        toggle("routine-inherit-model", form.inherits(), saving || !connected, cx)
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
                                if let Some(entry) = app.settings.get_mut(&bot) {
                                    entry.model.saved(saved);
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
            .overflow_y_scroll()
            .track_scroll(&self.details_scroll);
        if let Some(computer) = &self.computer {
            body = body.child(v_flex().flex_1().p_4().child(computer.clone()));
        }
        if self.details_tab == super::computer::DetailsTab::Details {
            if let Some(entry) = self.settings.get(&bot.id) {
                // Keep Watching at WATCHING_SECTION: the composer chip
                // scrolls the panel to that child.
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

    /// The panel's compact identity header: a 32 px avatar that opens the
    /// picker and the bot's 15 px name, which turns into a field when
    /// clicked. A full-width rule closes it like every section below.
    fn render_identity(&self, bot: Uuid, details: &Details, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let name = if details.editing {
            div()
                .id("bot-name-editor")
                .w_full()
                .on_mouse_down_out(cx.listener(move |app, _, _, cx| app.commit_name(bot, cx)))
                .child(
                    text_field(Input::new(&details.name), cx)
                        .text_left()
                        .text_size(px(15.))
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .into_any_element()
        } else {
            let hover = theme.sidebar_accent;
            h_flex()
                .id("bot-name")
                .max_w_full()
                .min_w_0()
                .gap_1p5()
                .px_2()
                .py_0p5()
                .rounded(theme.radius)
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .group("bot-name")
                .aria_description("Click to rename")
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(15.))
                        .line_height(px(20.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(details.sync.desired.name.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .opacity(0.)
                        .group_hover("bot-name", |style| style.opacity(1.))
                        .child(
                            Icon::new(gpui_kit::assets::IconName::PencilLine)
                                .size_3p5()
                                .text_color(theme.muted_foreground),
                        ),
                )
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
                        .control(Tier::Secondary, Scale::Compact, false, cx)
                        .label("Retry")
                        .on_click(cx.listener(move |app, _, _, cx| app.flush_details(bot, cx))),
                )
        });
        h_flex()
            .flex_none()
            .w_full()
            .px_4()
            .py_3()
            .gap_1()
            .items_center()
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
            .right(px(-3.))
            .bottom(px(-3.))
            .size(px(16.))
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
            .child(Icon::new(gpui_kit::assets::IconName::PencilLine).with_size(Size::Size(px(9.))));
        let hero = Button::new("edit-avatar")
            .focus_ring(false)
            .cursor_pointer()
            .ghost()
            .w(px(32.))
            .h(px(32.))
            .p_0()
            .rounded(theme.radius)
            .accessibility_label(editing)
            .toggled(details.picking)
            .child(
                div()
                    .relative()
                    .group("avatar-editor")
                    .child(bot_avatar(
                        ElementId::Name(format!("settings-avatar-{bot}").into()),
                        details.sync.desired.avatar,
                        px(32.),
                    ))
                    .child(badge),
            );
        let app = cx.entity().downgrade();
        let toggle = app.clone();
        // The header sits at the top of the panel, so the picker opens down.
        Popover::new("avatar-picker")
            .anchor(Anchor::TopLeft)
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
        let Some(saved) = state.shown() else {
            let ModelState::Failed(err) = state else {
                return body.child(muted("Loading…", cx)).into_any_element();
            };
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
                    .gap_0p5()
                    .child(item_name(self.model_name(choice)))
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
        let upgrade = saved.and_then(|saved| self.render_model_upgrade(bot, &saved.choice, cx));
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
            .my(-ROW_BLEED)
            .px(ROW_BLEED)
            .py(ROW_BLEED)
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
            .children(upgrade)
            .into_any_element()
    }

    /// The catalog entry of a saved choice, when its provider lists it.
    fn model_info(&self, choice: &ProfileChoice) -> Option<&ModelInfo> {
        self.caps
            .as_ref()?
            .providers
            .iter()
            .find(|provider| provider.id == choice.provider_id)?
            .catalog
            .model(&choice.model)
    }

    /// The provider's name for a model, else one read from its ID.
    pub(super) fn model_name(&self, choice: &ProfileChoice) -> String {
        self.model_info(choice)
            .map_or_else(|| model_label(&choice.model), |model| model.label.clone())
    }

    /// "Claude Fable 5.2 available" with a Switch button, when the saved
    /// model's family has a newer model. Never switches on its own.
    fn render_model_upgrade(
        &self,
        bot: Uuid,
        choice: &ProfileChoice,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let newer = self.model_info(choice)?.upgrade.clone()?;
        let target = ProfileChoice {
            model: newer,
            ..choice.clone()
        };
        let label = self.model_info(&target)?.label.clone();
        let saving =
            matches!(self.model_upgrade, Some(ModelUpgrade::Saving { bot: b }) if b == bot);
        let error = match &self.model_upgrade {
            Some(ModelUpgrade::Failed { bot: b, error }) if *b == bot => Some(error.clone()),
            _ => None,
        };
        let connected = self.connection == Connection::Connected;
        Some(
            v_flex()
                .w_full()
                .gap_1()
                .pt_2()
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            Icon::new(IconName::ArrowUp)
                                .size_3p5()
                                .flex_none()
                                .text_color(cx.theme().primary),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .child(format!("{label} available")),
                        )
                        .child(
                            Button::new("upgrade-model")
                                .control(Tier::Secondary, Scale::Compact, !connected || saving, cx)
                                .label("Switch")
                                .loading(saving)
                                .accessibility_label(format!("Switch to {label}"))
                                .on_click(cx.listener(move |app, _, _, cx| {
                                    app.upgrade_model(bot, target.clone(), cx)
                                })),
                        ),
                )
                .children(error.map(|error| {
                    alert(format!("Couldn't switch. {error}"), cx.theme().danger, cx)
                })),
        )
    }

    /// Save `target`, the newer model with the bot's effort and speed where
    /// it takes them.
    fn upgrade_model(&mut self, bot: Uuid, target: ProfileChoice, cx: &mut Context<Self>) {
        let Some(provider) = self
            .caps
            .as_ref()
            .and_then(|caps| caps.providers.iter().find(|p| p.id == target.provider_id))
        else {
            return;
        };
        let Some(choice) = crate::profile_picker::catalog_choice(provider, Some(&target)) else {
            return;
        };
        self.model_upgrade = Some(ModelUpgrade::Saving { bot });
        cx.notify();
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let result = client.set_bot_profile(bot, choice).await;
            let _ = this.update(cx, |app, cx| {
                if !matches!(app.model_upgrade, Some(ModelUpgrade::Saving { bot: b }) if b == bot) {
                    return;
                }
                app.model_upgrade = match result {
                    Ok(saved) => {
                        if let Some(entry) = app.settings.get_mut(&bot) {
                            entry.model.saved(saved);
                        }
                        None
                    }
                    Err(err) => Some(ModelUpgrade::Failed {
                        bot,
                        error: err.to_string(),
                    }),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// A model picker that badges models listed since the owner last
    /// opened one.
    pub(super) fn profile_picker(
        &mut self,
        caps: &Capabilities,
        initial: Option<&ProfileChoice>,
        cx: &mut Context<Self>,
    ) -> ProfilePicker {
        let new = self.preferences.see_models(caps);
        self.save_preferences(cx);
        ProfilePicker::new(caps, initial).with_new(new)
    }

    fn model_available(&self, choice: &ProfileChoice) -> bool {
        self.caps.as_ref().is_none_or(|caps| {
            caps.providers.iter().any(|provider| {
                provider.id == choice.provider_id
                    && provider.cli_available
                    && provider.catalog.applies(choice)
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
        body.child(list_rows(rows)).into_any_element()
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
            cx,
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
        // Same anatomy as a routine: the name over one meta line, beside a
        // centred switch. What the trigger asks for follows as its body.
        let mut tokens = vec![
            h_flex()
                .flex_none()
                .gap_1p5()
                .items_center()
                .text_color(theme.muted_foreground)
                .child(card.icon.clone().size_3().flex_none())
                .child(meta(facts.first().cloned().unwrap_or_default(), cx).whitespace_nowrap())
                .into_any_element(),
        ];
        tokens.extend(facts.into_iter().skip(1).map(|fact| {
            meta(fact, cx)
                .flex_none()
                .whitespace_nowrap()
                .into_any_element()
        }));
        v_flex()
            .w_full()
            .py_0()
            .gap_1()
            .group("automation-card")
            .child(
                // The switch lines up with the name, the row's first line.
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(item_name(card.title.clone()))
                            .child(meta_line(tokens, cx)),
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
            .child(meta(card.instructions, cx).pt_0p5())
            .children(error.map(|error| alert(error, theme.danger, cx)))
            .into_any_element()
    }

    fn render_watchers(
        &self,
        bot: Uuid,
        entry: &BotSettings,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let watchers = self.watchers(bot);
        if watchers.is_empty() {
            return None;
        }
        let now = Utc::now();
        let connected = self.connection == Connection::Connected;
        let rows = watchers
            .iter()
            .map(|watcher| {
                let id = watcher.id;
                let stop = entry.watcher_stops.get(&id);
                let stopping = matches!(stop, Some(WatcherStop::Stopping));
                let error = match stop {
                    Some(WatcherStop::Failed(message) | WatcherStop::Unconfirmed(message)) => {
                        Some(alert(message.clone(), cx.theme().danger, cx))
                    }
                    _ => None,
                };
                let stop =
                    Button::new(SharedString::from(format!("watcher-stop-{id}")))
                        .control(Tier::Ghost, Scale::Compact, stopping || !connected, cx)
                        .label(if stopping { "Stopping…" } else { "Stop" })
                        .loading(stopping)
                        .accessibility_label(format!(
                            "{} watching {}",
                            if stopping { "Stopping" } else { "Stop" },
                            watcher.purpose
                        ))
                        .on_click(cx.listener(move |app, _, window, cx| {
                            app.stop_watcher(bot, id, window, cx)
                        }));
                let open = entry.open_watcher_log == Some(id);
                let log = watcher.last_output.as_ref().filter(|text| !text.is_empty());
                let log_button = log.map(|_| {
                    Button::new(SharedString::from(format!("watcher-log-{id}")))
                        .control(Tier::Ghost, Scale::Compact, false, cx)
                        .flush_start(Scale::Compact)
                        .label(if open { "Hide log" } else { "View log" })
                        .accessibility_label(format!(
                            "{} log for {}",
                            if open { "Hide" } else { "View" },
                            watcher.purpose
                        ))
                        .on_click(cx.listener(move |app, _, _, cx| {
                            if let Some(entry) = app.settings.get_mut(&bot) {
                                entry.open_watcher_log =
                                    (entry.open_watcher_log != Some(id)).then_some(id);
                                cx.notify();
                            }
                        }))
                });
                let local = chrono::Local::now();
                let until = format!(
                    "until {}",
                    watcher_deadline(
                        watcher.until.with_timezone(&local.timezone()),
                        local.date_naive()
                    )
                );
                let timing = watcher_next(watcher, now);
                let mut labels = vec![meta(until, cx).into_any_element()];
                labels.extend(timing.map(|text| meta(text, cx).into_any_element()));
                v_flex()
                    .min_w_0()
                    .child(
                        h_flex()
                            .items_start()
                            .gap_2()
                            .child(item_name(watcher.purpose.clone()).flex_1().min_w_0())
                            .child(stop.flex_none()),
                    )
                    .child(meta(watcher_last_check(watcher, now), cx))
                    .child(meta_line(labels, cx))
                    .children(error)
                    .children(log_button.map(|button| h_flex().child(button)))
                    .children(log.filter(|_| open).map(|log| {
                        meta(log.clone(), cx)
                            .font_family(cx.theme().mono_font_family.clone())
                            .bg(cx.theme().secondary)
                            .rounded(cx.theme().radius)
                            .p_2()
                    }))
                    .into_any_element()
            })
            .collect();
        Some(section("Watching", cx).child(list_rows(rows)))
    }

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
        let progress = match run.status.as_str() {
            "queued" => "Retrying: waiting to start",
            "checking" | "running" => "Retrying: running now",
            _ => "Retrying: checking the result",
        };
        return Some((progress.to_owned(), false));
    }
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
        "no_news" => (format!("Last run {when}: no news"), false),
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

/// A titled group of rows in the Details panel: no box, just a label, 16 px
/// of padding and one full-width rule under the group. Rows inside never
/// draw their own rules.
pub(super) fn section_frame(cx: &App) -> Div {
    v_flex()
        .w_full()
        .px_4()
        .py_4()
        .border_b_1()
        .border_color(hairline(cx))
}

/// The gap from a section's label to its first row, and between rows.
pub(super) const ROW_GAP: Pixels = px(12.);

pub(super) fn section(title: &'static str, cx: &App) -> Div {
    section_frame(cx).child(crate::appearance::section_label(title, cx).pb(ROW_GAP))
}

/// A section's rows, 12 px apart.
fn list_rows(rows: Vec<AnyElement>) -> Div {
    v_flex().w_full().gap(ROW_GAP).children(rows)
}

/// The Watching section's index among the Details tab's sections.
const WATCHING_SECTION: usize = 5;

pub(super) fn watching_label(count: usize) -> String {
    let noun = if count == 1 { "thing" } else { "things" };
    format!("Watching {count} {noun}")
}

/// A watcher's deadline: its time when it falls on `today`, else its date.
pub(super) fn watcher_deadline<Tz: chrono::TimeZone>(
    until: DateTime<Tz>,
    today: chrono::NaiveDate,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    if until.date_naive() == today {
        until.format("%H:%M").to_string()
    } else {
        until.format("%b %-d").to_string()
    }
}

fn watcher_duration(minutes: i64) -> String {
    match minutes {
        ..=59 => format!("{minutes} min"),
        60..1440 => format!("{} hr", minutes / 60),
        1440..2880 => "1 day".to_owned(),
        _ => format!("{} days", minutes / 1440),
    }
}

fn watcher_last_check(watcher: &Watcher, now: DateTime<Utc>) -> String {
    let Some(at) = watcher.last_checked_at else {
        return match &watcher.last_status {
            Some(status) => format!("not checked yet: {status}"),
            None => "not checked yet".to_owned(),
        };
    };
    let minutes = (now - at).num_minutes();
    let when = if minutes <= 0 {
        "just now".to_owned()
    } else {
        format!("{} ago", watcher_duration(minutes))
    };
    match &watcher.last_status {
        Some(status) => format!("checked {when}: {status}"),
        None => format!("checked {when}"),
    }
}

fn watcher_next(watcher: &Watcher, now: DateTime<Utc>) -> Option<String> {
    let at = match watcher.status {
        WatcherStatus::Checking => return Some("checking now".to_owned()),
        WatcherStatus::Waiting { next_check_at } => next_check_at?,
    };
    Some(match (at - now).num_minutes() {
        _ if at <= now => "next check due now".to_owned(),
        0 => "next check in <1 min".to_owned(),
        minutes => format!("next check in {}", watcher_duration(minutes)),
    })
}

/// Secondary text in a row: a schedule, a model, a trigger's details. It
/// wraps instead of truncating.
pub(super) fn meta(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_size(px(12.))
        .line_height(px(18.))
        .text_color(cx.theme().muted_foreground)
        .whitespace_normal()
        .child(text.into())
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

/// A row's one meta line: whole tokens joined by "·". The separator stays
/// with the token before it, so a narrow panel wraps between tokens and
/// never starts a line with a dot.
fn meta_line(tokens: Vec<AnyElement>, cx: &App) -> Div {
    let last = tokens.len().saturating_sub(1);
    let separator = cx.theme().muted_foreground.opacity(0.6);
    h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .gap_x_1()
        .children(tokens.into_iter().enumerate().map(move |(index, token)| {
            h_flex()
                .min_w_0()
                .gap_x_1()
                .items_center()
                .child(token)
                .when(index < last, |token| {
                    token.child(
                        div()
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(separator)
                            .child("·"),
                    )
                })
        }))
}

/// An item's name in a Details row, over its one meta line.
pub(super) fn item_name(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(14.))
        .line_height(px(20.))
        .font_weight(FontWeight::MEDIUM)
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

pub(super) fn muted(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

pub(super) fn note(text: impl Into<SharedString>, error: bool, cx: &App) -> Div {
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
    icon: Icon,
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
            icon: Icon::new(gpui_kit::assets::IconName::Mail),
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
                Provider::Telegram => Icon::new(gpui_kit::assets::IconName::Send),
                Provider::Discord => Icon::empty().path(crate::DISCORD_ICON),
                Provider::WhatsApp => Icon::new(gpui_kit::assets::IconName::MessageSquare),
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
        icon: Icon::new(gpui_kit::assets::IconName::Webhook),
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
        Banner, Metadata, MetadataChange, MetadataSync, ModelState, RoutineSave, TriggerSave,
        WatcherStop, banner, describe_cron, describe_hours, last_run, pace, parse_name, retryable,
        still_pending, stop_result, trigger_cards, watcher_deadline, watcher_last_check,
        watcher_next,
    };
    use crate::api::ClientError;
    use crate::api::socials::Trigger;
    use crate::api::{BotProfile, ProfileChoice};
    use crate::avatar::{Avatar, AvatarColor, AvatarShape};
    use crate::model::{Conversation, Snapshot, Watcher};
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn watcher_labels_show_the_deadline_and_check_times() {
        let now = "2026-11-20T08:00:00Z".parse().unwrap();
        let watcher: Watcher = serde_json::from_value(json!({
            "id": "c7947c9e-2f91-4fb5-8fcc-ec4b20ca9307", "purpose": "Domain availability",
            "until": "2026-12-04T10:00:00Z", "status": "waiting",
            "last_status": "unavailable", "last_checked_at": "2026-11-20T07:58:00Z",
            "next_check_at": "2026-11-20T08:13:00Z", "last_output": null,
        }))
        .unwrap();
        let today = "2026-11-20".parse().unwrap();
        assert_eq!(watcher_deadline(watcher.until, today), "Dec 4");
        let tonight: chrono::DateTime<chrono::Utc> = "2026-11-20T18:30:00Z".parse().unwrap();
        assert_eq!(watcher_deadline(tonight, today), "18:30");
        assert_eq!(
            watcher_last_check(&watcher, now),
            "checked 2 min ago: unavailable"
        );
        assert_eq!(
            watcher_next(&watcher, now).as_deref(),
            Some("next check in 13 min")
        );
        assert_eq!(
            watcher_next(&watcher, "2026-11-20T08:13:01Z".parse().unwrap()).as_deref(),
            Some("next check due now")
        );
    }

    #[test]
    fn watcher_labels_show_a_first_check_in_progress() {
        let now = "2026-11-20T08:00:00Z".parse().unwrap();
        let watcher: Watcher = serde_json::from_value(json!({
            "id": "c7947c9e-2f91-4fb5-8fcc-ec4b20ca9307", "purpose": "Domain availability",
            "until": "2026-12-04T10:00:00Z", "status": "checking",
            "last_status": null, "last_checked_at": null, "next_check_at": null, "last_output": null,
        })).unwrap();
        assert_eq!(watcher_last_check(&watcher, now), "not checked yet");
        assert_eq!(watcher_next(&watcher, now).as_deref(), Some("checking now"));
    }

    #[test]
    fn a_watcher_that_never_ran_says_why() {
        let now = "2026-11-20T08:00:00Z".parse().unwrap();
        let watcher: Watcher = serde_json::from_value(json!({
            "id": "c7947c9e-2f91-4fb5-8fcc-ec4b20ca9307", "purpose": "Domain availability",
            "until": "2026-12-04T10:00:00Z", "status": "waiting",
            "last_status": "check did not run: the computer was not available",
            "last_checked_at": null, "next_check_at": "2026-11-20T08:01:00Z", "last_output": null,
        }))
        .unwrap();
        assert_eq!(
            watcher_last_check(&watcher, now),
            "not checked yet: check did not run: the computer was not available"
        );
    }

    #[test]
    fn a_stop_settles_on_its_result() {
        let api = |status: u16, code: &str| {
            Err(ClientError::Api {
                status,
                code: code.into(),
                message: "Nope.".into(),
                retryable: false,
            })
        };
        assert_eq!(stop_result(Ok(())), None);
        assert_eq!(
            stop_result(api(409, "not_active")),
            None,
            "it already ended"
        );
        assert_eq!(
            stop_result(api(404, "not_found")),
            Some(WatcherStop::Failed("Couldn't stop. Nope.".into()))
        );
        assert_eq!(
            stop_result(api(503, "unavailable")),
            Some(WatcherStop::Unconfirmed(
                "Couldn't confirm the stop. Nope.".into()
            ))
        );
    }

    fn profile(model: &str) -> Option<BotProfile> {
        Some(BotProfile {
            choice: ProfileChoice {
                provider_id: "codex".into(),
                model: model.into(),
                reasoning_effort: "medium".into(),
                service_tier: "priority".into(),
            },
            own: true,
        })
    }

    #[test]
    fn a_refresh_shows_the_profile_until_its_answer_lands() {
        let mut state = ModelState::Ready(profile("gpt-6.1-sol"));
        let attempt = Uuid::new_v4();
        state.reload(attempt);
        assert_eq!(state.shown(), Some(&profile("gpt-6.1-sol")));
        assert!(state.answer(attempt, Ok(profile("gpt-6.2-sol"))));
        assert_eq!(state.shown(), Some(&profile("gpt-6.2-sol")));
    }

    #[test]
    fn a_superseded_answer_is_dropped() {
        let mut state = ModelState::Ready(profile("gpt-6.1-sol"));
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        state.reload(first);
        state.reload(second);
        assert!(state.answer(second, Ok(profile("gpt-6.2-sol"))));
        assert!(!state.answer(first, Ok(profile("gpt-6.1-sol"))));
        assert_eq!(state.shown(), Some(&profile("gpt-6.2-sol")));
    }

    #[test]
    fn a_failed_refresh_keeps_the_profile_and_a_failed_first_load_shows_the_error() {
        let mut state = ModelState::Ready(profile("gpt-6.1-sol"));
        let attempt = Uuid::new_v4();
        state.reload(attempt);
        assert!(state.answer(attempt, Err("offline".into())));
        assert_eq!(state.shown(), Some(&profile("gpt-6.1-sol")));

        let mut state = ModelState::Loading(Uuid::nil());
        let attempt = Uuid::new_v4();
        state.reload(attempt);
        assert_eq!(state.shown(), None);
        assert!(state.answer(attempt, Err("offline".into())));
        assert!(matches!(&state, ModelState::Failed(err) if err == "offline"));
    }

    #[test]
    fn a_save_during_a_refresh_shows_at_once_and_the_refresh_still_lands() {
        let mut state = ModelState::Ready(profile("gpt-6.1-sol"));
        let attempt = Uuid::new_v4();
        state.reload(attempt);
        state.saved(profile("gpt-6.2-sol").unwrap());
        assert_eq!(state.shown(), Some(&profile("gpt-6.2-sol")));
        assert!(state.answer(attempt, Ok(profile("gpt-6.3-sol"))));
        assert_eq!(state.shown(), Some(&profile("gpt-6.3-sol")));
    }

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
