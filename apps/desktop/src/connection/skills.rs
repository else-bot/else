mod view;

use super::*;
use crate::controls::{Control as _, Scale, Tier, text_field};
use crate::skill_ui::{SkillFiles, SkillKeys, chip, source};
use else_wire::skills::InstalledSkill;
use else_wire::skills::SkillScope;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::{
    Disableable as _, FocusableExt as _, Icon, Sizable as _, StyledExt as _, h_flex,
};
use std::time::Duration;

use super::loaded::{Guard, Loaded};
use super::pane::PaneEvent;

pub(super) struct View {
    client: Arc<Client>,
    /// Whether the server is reachable; key inputs are wiped when it is not.
    connected: bool,
    entries: Loaded<Vec<InstalledSkill>>,
    search: Entity<InputState>,
    expanded: Option<String>,
    files: HashMap<String, SkillFiles>,
    keys: Option<Entity<SkillKeys>>,
    replacing: bool,
    busy: Option<Guard>,
    error: Option<String>,
    _search: Subscription,
    _poll: Task<()>,
}

impl EventEmitter<PaneEvent> for View {}

impl View {
    pub(super) fn new(
        client: Arc<Client>,
        connected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let poll_client = client.clone();
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search installed skills"));
        let subscription = cx.subscribe_in(&search, window, |_, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let poll = Loaded::poll(
            window,
            cx,
            |view: &mut Self| &mut view.entries,
            |_| Duration::from_secs(5),
            move || poll_client.clone().installed_skills(),
            Self::apply,
        );
        Self {
            client,
            connected,
            entries: Loaded::default(),
            search,
            expanded: None,
            files: HashMap::new(),
            keys: None,
            replacing: false,
            busy: None,
            error: None,
            _search: subscription,
            _poll: poll,
        }
    }

    pub(super) fn has_unsaved_work(&self, cx: &App) -> bool {
        self.busy.is_some()
            || self
                .keys
                .as_ref()
                .is_some_and(|keys| !keys.read(cx).is_empty(cx))
    }

    pub(super) fn clear_keys(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(keys) = self.keys.take() {
            keys.update(cx, |keys, cx| keys.clear(window, cx));
        }
        self.replacing = false;
    }

    /// Losing the connection wipes typed keys.
    pub(super) fn set_connected(
        &mut self,
        connected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !connected {
            self.clear_keys(window, cx);
        }
        if self.connected != connected {
            self.connected = connected;
            cx.notify();
        }
    }

    fn apply(
        &mut self,
        attempt: Uuid,
        result: Result<Vec<InstalledSkill>, ClientError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(entries) => {
                let old = self.entries.value();
                self.files.retain(|id, _| {
                    old.into_iter()
                        .flatten()
                        .find(|old| old.id == *id)
                        .zip(entries.iter().find(|new| new.id == *id))
                        .is_some_and(|(old, new)| {
                            old.installed_at == new.installed_at && old.source == new.source
                        })
                });
                if let Some(expanded) = self.expanded.clone() {
                    let replaced = old
                        .into_iter()
                        .flatten()
                        .find(|old| old.id == expanded)
                        .zip(entries.iter().find(|new| new.id == expanded))
                        .is_some_and(|(old, new)| {
                            old.installed_at != new.installed_at || old.source != new.source
                        });
                    if replaced {
                        self.clear_keys(window, cx);
                        self.expanded = None;
                    }
                }
                if let Some(expanded) = &self.expanded {
                    let current = entries.iter().find(|entry| entry.id == *expanded);
                    if current.is_none()
                        || current.is_some_and(|entry| entry.key_stored) && !self.replacing
                    {
                        self.clear_keys(window, cx);
                    }
                    if self.connected
                        && let Some(entry) =
                            current.filter(|entry| !entry.key_stored && !entry.env.is_empty())
                        && self.keys.is_none()
                    {
                        self.keys = Some(cx.new(|cx| SkillKeys::new(&entry.env, window, cx)));
                    }
                    if current.is_none() {
                        self.expanded = None;
                    }
                }
                self.entries.settle(attempt, Ok(entries));
            }
            Err(error) => {
                self.entries.settle(attempt, Err(error.to_string()));
            }
        }
        cx.notify();
    }

    fn expand(&mut self, entry: InstalledSkill, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.clear_keys(window, cx);
        if self.expanded.as_ref() == Some(&entry.id) {
            self.expanded = None;
            cx.notify();
            return;
        }
        self.expanded = Some(entry.id.clone());
        self.error = None;
        if !entry.env.is_empty() && !entry.key_stored {
            self.keys = Some(cx.new(|cx| SkillKeys::new(&entry.env, window, cx)));
        }
        let files = self.files.entry(entry.id.clone()).or_default();
        if files.selected.is_none() {
            files.selected = entry
                .files
                .iter()
                .find(|file| file.path == "SKILL.md")
                .or(entry.files.first())
                .map(|file| file.path.clone());
        }
        if files.contents.is_some() || files.loading {
            cx.notify();
            return;
        }
        files.loading = true;
        files.error = None;
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.skill_detail(entry.id.clone()).await;
            let _ = this.update(cx, |view, cx| {
                if !view.entries.value().into_iter().flatten().any(|current| {
                    current.id == entry.id
                        && current.installed_at == entry.installed_at
                        && current.source == entry.source
                }) {
                    return;
                }
                let Some(files) = view.files.get_mut(&entry.id) else {
                    return;
                };
                files.loading = false;
                match result {
                    Ok(detail) => {
                        files.contents = Some(detail.contents);
                    }
                    Err(error) => files.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn edit_key(&mut self, entry: InstalledSkill, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.clear_keys(window, cx);
        self.replacing = true;
        self.keys = Some(cx.new(|cx| SkillKeys::new(&entry.env, window, cx)));
        cx.notify();
    }

    fn write_key(
        &mut self,
        entry: InstalledSkill,
        save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let env = if save {
            let Some(keys) = &self.keys else { return };
            match keys.read(cx).values(false, cx) {
                Ok(env) => env,
                Err(error) => {
                    self.error = Some(error);
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        self.busy = Some(self.entries.operate());
        self.error = None;
        if let Some(keys) = &self.keys {
            keys.update(cx, |keys, cx| {
                keys.busy = true;
                cx.notify();
            });
        }
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.save_skill_key(entry.secret, env).await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.busy = None;
                match result {
                    Ok(()) => {
                        view.clear_keys(window, cx);
                        if let Some(current) = view
                            .entries
                            .value_mut()
                            .into_iter()
                            .flatten()
                            .find(|current| current.id == entry.id)
                        {
                            current.key_stored = save;
                        }
                        if !save && view.connected {
                            view.keys = Some(cx.new(|cx| SkillKeys::new(&entry.env, window, cx)));
                        }
                    }
                    Err(error) => {
                        view.error = Some(error.to_string());
                        view.clear_keys(window, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn confirm_remove(
        &mut self,
        entry: InstalledSkill,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        self.clear_keys(window, cx);
        let view = cx.weak_entity();
        let shared = entry.scope == SkillScope::All;
        let (title, confirm) = if shared {
            (
                format!("Delete {} for every bot?", entry.name),
                "Delete for all bots",
            )
        } else {
            (format!("Delete {}?", entry.name), "Delete")
        };
        let id = entry.id;
        crate::panel_kit::confirm_dialog(
            window,
            cx,
            title,
            "This deletes the skill's files and its stored key. Bots using it lose access. You can ask a bot to install it again later.",
            confirm,
            Tier::Danger,
            move |window, cx| {
                let _ = view.update(cx, |view, cx| view.remove(id.clone(), window, cx));
            },
        );
    }

    fn remove(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(self.entries.operate());
        self.error = None;
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.remove_skill(id.clone()).await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.busy = None;
                match result {
                    Ok(()) => {
                        if let Some(entries) = view.entries.value_mut() {
                            entries.retain(|entry| entry.id != id);
                        }
                        view.files.remove(&id);
                        if view.expanded.as_ref() == Some(&id) {
                            view.clear_keys(window, cx);
                            view.expanded = None;
                        }
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_page(cx)
    }
}
