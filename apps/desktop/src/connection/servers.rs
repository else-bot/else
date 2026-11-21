mod monitor;
mod switch;

pub(super) use monitor::{Connections, Health};

use else_wire::settings::ServerInfo;

use super::*;
use crate::controls::{Control as _, Scale, Tier};
use chrono::{DateTime, Utc};
use gpui_kit::component::button::Button;
use gpui_kit::component::h_flex;
use reqwest::Method;
use serde::Deserialize;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

#[derive(Clone, Copy)]
pub(super) enum Editor {
    Add,
    Edit(Uuid),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReleaseStatus {
    Checking,
    NotChecked,
    VersionUnavailable,
    ReleaseUnavailable,
    Available,
    Current,
    Newer,
}

impl ReleaseStatus {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Checking => "Checking…",
            Self::NotChecked => "Version not checked",
            Self::VersionUnavailable => "Version unavailable",
            Self::ReleaseUnavailable => "Release check unavailable",
            Self::Available => "Update available",
            Self::Current => "Up to date",
            Self::Newer => "Newer than stable",
        }
    }

    fn compare(observed: Option<&str>, latest: Option<&str>) -> Self {
        let Some(version) = observed.and_then(version_numbers) else {
            return Self::VersionUnavailable;
        };
        let Some(latest) = latest.and_then(version_numbers) else {
            return Self::ReleaseUnavailable;
        };
        match version.cmp(&latest) {
            std::cmp::Ordering::Less => Self::Available,
            std::cmp::Ordering::Equal => Self::Current,
            std::cmp::Ordering::Greater => Self::Newer,
        }
    }
}

/// What a desktop does with the name a saved server reports.
#[derive(Debug, PartialEq, Eq)]
enum NameAction {
    Adopt(String),
    /// Offer this desktop's name to a server nobody named yet.
    Carry(String),
    Keep,
}

impl NameAction {
    fn of(saved: &crate::config::SavedServer, server: ServerInfo) -> Self {
        match server.name {
            Some(name) if name != saved.name => Self::Adopt(name),
            None if !saved.has_default_name() => Self::Carry(saved.name.clone()),
            _ => Self::Keep,
        }
    }
}

pub(super) struct NameWrite {
    pub name: String,
    /// Offered to a server nobody named yet; a failure waits for the next
    /// check instead of being shown.
    pub carried: bool,
}

/// Writes of one shared setting per server: the one in flight with the
/// latest value waiting behind it, and when the last write landed.
pub(super) struct Writes<W> {
    pending: HashMap<Uuid, Option<W>>,
    landed: HashMap<Uuid, Instant>,
}

impl<W> Default for Writes<W> {
    fn default() -> Self {
        Self {
            pending: HashMap::new(),
            landed: HashMap::new(),
        }
    }
}

impl<W> Writes<W> {
    /// The write to send now, or `None` when it waits behind one in flight.
    pub(super) fn begin(&mut self, id: Uuid, write: W) -> Option<W> {
        match self.pending.get_mut(&id) {
            Some(next) => {
                *next = Some(write);
                None
            }
            None => {
                self.pending.insert(id, None);
                Some(write)
            }
        }
    }

    /// End the write in flight; returns the one waiting behind it.
    pub(super) fn finish(&mut self, id: Uuid, landed: bool) -> Option<W> {
        if landed {
            self.landed.insert(id, Instant::now());
        }
        self.pending.remove(&id).flatten()
    }

    /// Whether a check that started at `asked` can change the setting: no
    /// write is in flight and it started after the last one landed.
    pub(super) fn settled(&self, id: Uuid, asked: Option<Instant>) -> bool {
        !self.pending.contains_key(&id)
            && self
                .landed
                .get(&id)
                .is_none_or(|landed| asked.is_some_and(|asked| asked > *landed))
    }
}

impl Shell {
    pub(super) fn managed_server(&self, profile: &Profile) -> bool {
        self.launch
            .updates
            .as_ref()
            .ok()
            .and_then(|installation| installation.server())
            .is_some_and(|server| server.owns(profile))
    }

    pub(super) fn server_release(&self, id: Uuid) -> ReleaseStatus {
        let Some(observed) = self.servers.entries.get(&id) else {
            return ReleaseStatus::Checking;
        };
        if observed.health != Health::Online {
            return ReleaseStatus::NotChecked;
        }
        let view = if self
            .registry
            .find(id)
            .is_some_and(|saved| self.managed_server(&saved.profile))
        {
            Some(&self.server_updates)
        } else {
            self.remote_updates.get(&id)
        };
        if view.is_some_and(|view| matches!(view.snapshot.check, crate::updates::Check::Checking)) {
            return ReleaseStatus::Checking;
        }
        let latest = view.and_then(|view| view.snapshot.latest.as_deref());
        ReleaseStatus::compare(observed.version.as_deref(), latest)
    }

    pub(super) fn sync_server_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remote_updates.retain(|id, view| {
            self.registry
                .find(*id)
                .is_some_and(|saved| view.remote_profile.as_ref() == Some(&saved.profile))
        });
        for saved in &self.registry.servers {
            if self.managed_server(&saved.profile) || self.remote_updates.contains_key(&saved.id) {
                continue;
            }
            match updates::View::new_remote(
                saved.id,
                saved.profile.clone(),
                &self.runtime,
                window,
                cx,
            ) {
                Ok(view) => {
                    self.remote_updates.insert(saved.id, view);
                }
                Err(error) => self.connection_error = Some(error),
            }
        }
    }

    pub(super) fn refresh_servers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_server_updates(window, cx);
        self.server_updates.refresh();
        for view in self.remote_updates.values() {
            view.refresh();
        }
        self.servers
            .refresh(&self.registry, &self.runtime, window, cx);
        cx.notify();
    }

    /// Use the name the server keeps for every desktop. A server nobody
    /// named yet gets this desktop's name, unless it is the made-up one.
    pub(super) fn adopt_server_name(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let (Some(saved), Some(observed)) = (self.registry.find(id), self.servers.entries.get(&id))
        else {
            return;
        };
        if !self.names.settled(id, observed.asked) {
            return;
        }
        let Some(server) = observed.server.clone() else {
            return;
        };
        match NameAction::of(saved, server) {
            NameAction::Adopt(name) => self.rename_saved(id, &name),
            NameAction::Carry(name) => self.name_server(
                id,
                NameWrite {
                    name,
                    carried: true,
                },
                cx,
            ),
            NameAction::Keep => {}
        }
    }

    /// Apply `change` to a copy of the registry, save it, and adopt it only
    /// once saved, so memory never shows a change the disk refused.
    pub(super) fn commit_registry(
        &mut self,
        change: impl FnOnce(&mut ServerRegistry) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut registry = self.registry.clone();
        change(&mut registry)?;
        self.launch
            .store
            .as_ref()
            .map_err(Clone::clone)?
            .save(&registry)?;
        self.registry = registry;
        Ok(())
    }

    fn rename_saved(&mut self, id: Uuid, name: &str) {
        if let Err(err) = self.commit_registry(|registry| registry.rename(id, name)) {
            self.connection_error = Some(err);
        }
    }

    /// Save the name on the server, which every desktop picks up on its
    /// next check. Writes to one server go one at a time, and only the
    /// latest waiting name is sent next.
    pub(super) fn name_server(&mut self, id: Uuid, write: NameWrite, cx: &mut Context<Self>) {
        let Some(saved) = self.registry.find(id) else {
            return;
        };
        let profile = saved.profile.clone();
        let client = match Client::new(profile.origin.clone(), &profile.token, self.runtime.clone())
        {
            Ok(client) => Arc::new(client),
            Err(err) => {
                if !write.carried {
                    self.connection_error = Some(err);
                }
                return;
            }
        };
        let Some(write) = self.names.begin(id, write) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = client.name_server(write.name.clone(), write.carried).await;
            let _ = this.update(cx, |shell, cx| {
                let next = shell.names.finish(id, result.is_ok());
                match result {
                    Ok(kept) => {
                        let current = shell.registry.find(id).filter(|s| s.profile == profile);
                        if let Some(kept) = kept
                            && next.is_none()
                            && current.is_some_and(|s| s.name != kept)
                        {
                            shell.rename_saved(id, &kept);
                        }
                        shell.servers.recheck(id);
                    }
                    // A server from before shared names keeps the name on this desktop only.
                    Err(ClientError::Api { status: 404, .. }) => {}
                    Err(err) if !write.carried => {
                        shell.connection_error = Some(format!(
                            "Could not save the name {} on the server. {err}",
                            write.name
                        ));
                    }
                    Err(_) => {}
                }
                if let Some(next) = next {
                    shell.name_server(id, next, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn server_update_view(&self, id: Uuid) -> Option<&updates::View> {
        self.registry.find(id).and_then(|saved| {
            if self.managed_server(&saved.profile) {
                Some(&self.server_updates)
            } else {
                self.remote_updates.get(&id)
            }
        })
    }

    pub(super) fn can_update_server(&self, id: Uuid) -> bool {
        self.servers
            .entries
            .get(&id)
            .is_some_and(|entry| entry.health == Health::Online)
            && self.server_update_view(id).is_some_and(|view| {
                view.can_install()
                    && (self.remote_updates.contains_key(&id)
                        || !(self.updates.busy() || self.server_updates.busy()))
            })
    }

    pub(super) fn update_all_servers(&mut self, cx: &mut Context<Self>) {
        let targets: Vec<_> = self
            .registry
            .servers
            .iter()
            .filter(|saved| self.can_update_server(saved.id))
            .map(|saved| (saved.id, saved.name.clone()))
            .collect();
        let errors: Vec<_> = targets
            .into_iter()
            .filter_map(|(id, name)| {
                self.start_server_update(id)
                    .err()
                    .map(|error| format!("{name}: {error}"))
            })
            .collect();
        self.connection_error = (!errors.is_empty()).then(|| errors.join("\n"));
        cx.notify();
    }

    pub(super) fn install_server_update(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if !self.can_update_server(id) {
            return;
        }
        let name = self.registry.find(id).map(|saved| saved.name.clone());
        self.connection_error = self
            .start_server_update(id)
            .err()
            .map(|error| format!("{}: {error}", name.unwrap_or_default()));
        cx.notify();
    }

    fn start_server_update(&mut self, id: Uuid) -> Result<(), String> {
        self.remote_updates
            .get_mut(&id)
            .unwrap_or(&mut self.server_updates)
            .install()
    }

    pub(super) fn open_server_editor(
        &mut self,
        editor: Editor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_section().is_none() && self.active.is_some() {
            self.open_settings(window, cx);
        }
        let saved = match editor {
            Editor::Edit(id) => self.registry.find(id).cloned(),
            Editor::Add => None,
        };
        let Some(settings) = self.front.settings() else {
            return;
        };
        let form = ConnectForm::new(None, settings.focus.clone(), window, cx);
        let name = saved.as_ref().map(|s| s.name.clone()).unwrap_or_default();
        form.name
            .update(cx, |field, cx| field.set_value(name.clone(), window, cx));
        let page = match editor {
            Editor::Add => ServerPage::Add(form),
            Editor::Edit(id) => ServerPage::Edit {
                id,
                opened: name,
                form,
            },
        };
        if let Some(settings) = self.front.settings_mut() {
            settings.servers = page;
        }
        self.select_settings(SettingsSection::Server, window, cx);
        self.open_remote(saved.as_ref().map(|s| &s.profile), window, cx);
        cx.notify();
    }

    /// Back to the server list, dropping an open editor and its attempt.
    pub(super) fn show_server_list(&mut self) {
        if let Some(settings) = self.front.settings_mut() {
            settings.servers = ServerPage::List;
        }
    }

    pub(super) fn close_server_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(settings) = self.front.settings_mut() else {
            return;
        };
        if settings.servers.form().is_some() {
            settings.servers = ServerPage::List;
        }
        settings.focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn remove_server(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(saved) = self.registry.find(id) else {
            return;
        };
        if self.registry.selected == Some(id) {
            self.connection_error =
                Some("Switch to another server before removing this connection.".into());
            cx.notify();
            return;
        }
        if self
            .inactive
            .get(&id)
            .is_some_and(|session| session.app.read(cx).has_unsaved_work(cx))
        {
            self.connection_error=Some("This server has unsent drafts or pending messages. Open it and finish them before removing the connection.".into());
            cx.notify();
            return;
        }
        let name = saved.name.clone();
        let shell = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            dialog
                .title(format!("Remove {name}?"))
                .close_button(false)
                .child(
                    "Only the saved connection is removed. The server and its bots keep running.",
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("remove-server-cancel")
                                .control(Tier::Ghost, Scale::Standard, false, cx)
                                .label("Keep connection")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("remove-server-confirm")
                                .control(Tier::Danger, Scale::Standard, false, cx)
                                .label("Remove connection")
                                .on_click({
                                    let shell = shell.clone();
                                    move |_, window, cx| {
                                        window.close_dialog(cx);
                                        let _ = shell.update(cx, |shell, cx| {
                                            match shell.commit_registry(|r| r.remove(id)) {
                                                Ok(()) => {
                                                    if let Ok(store) = &shell.launch.store
                                                        && let Err(err) =
                                                            shell.rail.icons.forget(store, id).and(
                                                                crate::saved_conversations::SavedConversations::forget(
                                                                    &store.saved_conversations_dir(id),
                                                                ),
                                                            )
                                                    {
                                                        shell.connection_error = Some(err);
                                                    }
                                                    shell.inactive.remove(&id);
                                                    shell.switch.forget(id);
                                                    shell.remote_updates.remove(&id);
                                                    shell.refresh_servers(window, cx);
                                                }
                                                Err(err) => shell.connection_error = Some(err),
                                            }
                                            cx.notify();
                                        });
                                    }
                                }),
                        ),
                )
        });
    }

    pub(super) fn show_server_update(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self.registry.find(id) else {
            return;
        };
        let managed = self.managed_server(&saved.profile);
        self.sync_server_updates(window, cx);
        if !managed && !self.remote_updates.contains_key(&id) {
            cx.notify();
            return;
        }
        if let Some(settings) = self.front.settings_mut() {
            settings.servers = ServerPage::Update(id);
        }
        self.remote_updates
            .get(&id)
            .unwrap_or(&self.server_updates)
            .refresh();
        cx.notify();
    }

    pub(super) fn close_server_update(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(settings) = self.front.settings_mut()
            && matches!(settings.servers, ServerPage::Update(_))
        {
            settings.servers = ServerPage::List;
        }
        cx.notify();
    }

    /// The saved server whose update card shows instead of the list.
    pub(super) fn server_update_target(&self) -> Option<Uuid> {
        match self.front.settings()?.servers {
            ServerPage::Update(id) => Some(id),
            _ => None,
        }
    }
}

fn version_numbers(value: &str) -> Option<(u64, u64, u64)> {
    let mut parts = value.trim_prefix('v').split('.');
    let result = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "TTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTT";

    #[::core::prelude::v1::test]
    fn desktops_show_the_servers_name_and_carry_their_own_once() {
        let saved = |origin: &str, name: &str| crate::config::SavedServer {
            id: Uuid::new_v4(),
            name: name.into(),
            profile: Profile::parse(origin, TOKEN).unwrap(),
        };
        let named = |name: Option<&str>| ServerInfo {
            name: name.map(Into::into),
            icon: Default::default(),
        };
        let studio = saved("https://studio.example.com", "Studio");
        assert_eq!(
            NameAction::of(&studio, named(Some("Office"))),
            NameAction::Adopt("Office".into())
        );
        assert_eq!(
            NameAction::of(&studio, named(Some("Studio"))),
            NameAction::Keep
        );
        assert_eq!(
            NameAction::of(&studio, named(None)),
            NameAction::Carry("Studio".into())
        );
        let made_up = saved("https://studio.example.com", "studio.example.com");
        assert_eq!(NameAction::of(&made_up, named(None)), NameAction::Keep);
        let local = saved("http://127.0.0.1:7878", "This computer");
        assert_eq!(NameAction::of(&local, named(None)), NameAction::Keep);
    }

    #[::core::prelude::v1::test]
    fn name_writes_go_one_at_a_time_and_outdate_earlier_checks() {
        let id = Uuid::new_v4();
        let write = |name: &str| NameWrite {
            name: name.into(),
            carried: false,
        };
        let mut writes = Writes::default();
        let before = Instant::now();
        assert!(writes.settled(id, Some(before)), "nothing written yet");
        assert_eq!(writes.begin(id, write("A")).unwrap().name, "A");
        assert!(writes.begin(id, write("B")).is_none(), "B waits behind A");
        assert!(writes.begin(id, write("C")).is_none(), "C replaces B");
        assert!(!writes.settled(id, Some(Instant::now())), "A is in flight");
        assert_eq!(writes.finish(id, true).unwrap().name, "C");
        assert!(
            !writes.settled(id, Some(before)),
            "a check from before A landed is outdated"
        );
        assert!(writes.settled(id, Some(Instant::now())));
        assert_eq!(writes.begin(id, write("D")).unwrap().name, "D");
        assert!(writes.finish(id, false).is_none());
        assert!(
            !writes.settled(id, None),
            "an unanswered check never renames"
        );
    }

    #[::core::prelude::v1::test]
    fn release_status_requires_a_known_version_and_release() {
        assert_eq!(
            ReleaseStatus::compare(None, Some("v0.0.25")),
            ReleaseStatus::VersionUnavailable
        );
        assert_eq!(
            ReleaseStatus::compare(Some("0.0.24"), None),
            ReleaseStatus::ReleaseUnavailable
        );
        assert_eq!(
            ReleaseStatus::compare(Some("0.0.24"), Some("v0.0.25")),
            ReleaseStatus::Available
        );
        assert_eq!(
            ReleaseStatus::compare(Some("0.0.25"), Some("v0.0.25")),
            ReleaseStatus::Current
        );
        assert_eq!(
            ReleaseStatus::compare(Some("0.0.26"), Some("v0.0.25")),
            ReleaseStatus::Newer
        );
    }
}
