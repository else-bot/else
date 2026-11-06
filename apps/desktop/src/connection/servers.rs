use super::*;
use chrono::{DateTime, Utc};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{FocusableExt as _, h_flex};
use reqwest::Method;
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

#[derive(Clone, Copy)]
pub(super) enum Editor {
    Add,
    Edit(Uuid),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Health {
    Checking,
    Online,
    Offline,
    Unauthorized,
    Incompatible,
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

#[derive(Clone)]
pub(super) struct Observation {
    pub health: Health,
    pub version: Option<String>,
    pub seen: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub working: Option<u64>,
}

impl Default for Observation {
    fn default() -> Self {
        Self {
            health: Health::Checking,
            version: None,
            seen: None,
            error: None,
            working: None,
        }
    }
}

struct Monitor {
    profile: Profile,
    commands: mpsc::Sender<()>,
    _watch: Task<()>,
}

pub(super) struct Connections {
    pub entries: HashMap<Uuid, Observation>,
    monitors: HashMap<Uuid, Monitor>,
}

#[derive(Deserialize)]
struct Report {
    version: String,
    active_turns: u64,
    running_commands: u64,
}

async fn observe(client: Arc<Client>) -> Result<(Option<String>, Option<u64>), (Health, String)> {
    let this = client.clone();
    client
        .run(async move {
            let (caps, readiness, report) = tokio::join!(
                this.send_json::<crate::api::Capabilities>(
                    this.request(Method::GET, "/v1/capabilities")
                ),
                this.send_json::<serde_json::Value>(this.request(Method::GET, "/readyz")),
                this.send_json::<Report>(this.request(Method::GET, "/v1/maintenance/update")),
            );
            let (_, caps) = caps.map_err(|err| match err {
                ClientError::Unauthorized => (
                    Health::Unauthorized,
                    "The server rejected the saved owner token.".into(),
                ),
                other => (Health::Offline, other.to_string()),
            })?;
            if caps
                .api_version
                .as_deref()
                .is_some_and(|version| version != "v1")
            {
                return Err((
                    Health::Incompatible,
                    "Unsupported API version. Update the desktop app.".into(),
                ));
            }
            if let Err(err) = readiness
                && !matches!(&err, ClientError::Protocol(why) if why.starts_with("HTTP 404"))
            {
                return Err((
                    Health::Offline,
                    format!("Server readiness check failed. {err}"),
                ));
            }
            Ok(match report {
                Ok((_, report)) => (
                    Some(report.version),
                    Some(report.active_turns.saturating_add(report.running_commands)),
                ),
                Err(_) => (None, None),
            })
        })
        .await
}

fn monitor(
    profile: Profile,
    runtime: &tokio::runtime::Handle,
) -> (mpsc::Sender<()>, watch::Receiver<Observation>) {
    let (commands, mut requests) = mpsc::channel(1);
    let (states, snapshots) = watch::channel(Observation::default());
    runtime.spawn(async move {
        let client = Client::new(profile.origin, &profile.token, tokio::runtime::Handle::current()).map(Arc::new);
        let mut timer = tokio::time::interval(Duration::from_secs(30));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = states.closed() => break,
                request = requests.recv() => if request.is_none() { break },
                _ = timer.tick() => {},
            }
            let result = match &client {
                Ok(client) => tokio::select! { _ = states.closed() => break, result = observe(client.clone()) => result },
                Err(error) => Err((Health::Offline, error.clone())),
            };
            states.send_modify(|state| match result {
                Ok((version, working)) => {
                    state.health = Health::Online;
                    state.version = version;
                    state.working = working;
                    state.seen = Some(Utc::now());
                    state.error = None;
                }
                Err((health, error)) => { state.health = health; state.error = Some(error); state.working = None; }
            });
        }
    });
    (commands, snapshots)
}

impl Connections {
    pub(super) fn new(
        registry: &ServerRegistry,
        runtime: &tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Shell>,
    ) -> Self {
        let mut this = Self {
            entries: HashMap::new(),
            monitors: HashMap::new(),
        };
        this.refresh(registry, runtime, window, cx);
        this
    }

    pub(super) fn refresh(
        &mut self,
        registry: &ServerRegistry,
        runtime: &tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Shell>,
    ) {
        self.entries.retain(|id, _| registry.find(*id).is_some());
        self.monitors.retain(|id, monitor| {
            registry
                .find(*id)
                .is_some_and(|saved| saved.profile == monitor.profile)
        });
        for saved in &registry.servers {
            if let Some(monitor) = self.monitors.get(&saved.id) {
                let _ = monitor.commands.try_send(());
                continue;
            }
            self.entries.insert(saved.id, Observation::default());
            let id = saved.id;
            let profile = saved.profile.clone();
            let (commands, mut states) = monitor(profile.clone(), runtime);
            let expected = profile.clone();
            let task = cx.spawn_in(window, async move |this, cx| {
                while states.changed().await.is_ok() {
                    let snapshot = states.borrow_and_update().clone();
                    let accepted = this.update(cx, |shell, cx| {
                        if !shell
                            .registry
                            .find(id)
                            .is_some_and(|s| s.profile == expected)
                        {
                            return false;
                        }
                        shell.servers.entries.insert(id, snapshot);
                        cx.notify();
                        true
                    });
                    if !matches!(accepted, Ok(true)) {
                        break;
                    }
                }
            });
            self.monitors.insert(
                id,
                Monitor {
                    profile,
                    commands,
                    _watch: task,
                },
            );
        }
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
        let latest = match &self.updates.snapshot.check {
            crate::updates::Check::Available { latest }
            | crate::updates::Check::Current { latest } => Some(latest.as_str()),
            _ => None,
        };
        ReleaseStatus::compare(observed.version.as_deref(), latest)
    }

    pub(super) fn refresh_servers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.servers
            .refresh(&self.registry, &self.runtime, window, cx);
        cx.notify();
    }

    pub(super) fn open_server_editor(
        &mut self,
        editor: Editor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.server_menu = false;
        self.switch_attempt = None;
        self._switch = None;
        self.server_update_target = None;
        if self.settings_section().is_none() && self.active.is_some() {
            self.open_settings(window, cx);
        }
        let profile = match editor {
            Editor::Edit(id) => self.registry.find(id).map(|s| s.profile.clone()),
            _ => None,
        };
        if self.setup.is_none() {
            self.open_setup(None, window, cx);
        }
        self.server_editor = Some(editor);
        self.select_settings(settings::SettingsSection::Server, window, cx);
        let name = match editor {
            Editor::Edit(id) => self
                .registry
                .find(id)
                .map(|s| s.name.clone())
                .unwrap_or_default(),
            Editor::Add => String::new(),
        };
        self.setup
            .as_ref()
            .unwrap()
            .name
            .update(cx, |field, cx| field.set_value(name, window, cx));
        self.open_remote(profile.as_ref(), window, cx);
        cx.notify();
    }

    pub(super) fn close_server_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.server_editor = None;
        if let Some(setup) = &mut self.setup {
            setup._attempt = None;
            setup.draft.edited();
            setup.error = None;
            setup.page = Page::Welcome;
            setup.focus.focus(window, cx);
        }
        cx.notify();
    }

    pub(super) fn select_server(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.server_menu = false;
        if self.active.as_ref().is_some_and(|active| active.id == id) {
            self.switch_attempt = None;
            self._switch = None;
            self.notification_target = None;
            cx.notify();
            return;
        }
        let dirty = self.server_editor.is_some()
            || self
                .mcp
                .as_ref()
                .is_some_and(|view| view.has_unsaved_work())
            || self
                .backups
                .as_ref()
                .is_some_and(|view| view.has_unsaved_work())
            || self
                .email
                .as_ref()
                .is_some_and(|(view, _)| view.read(cx).has_unsaved_work(cx))
            || self
                .socials
                .as_ref()
                .is_some_and(|view| view.read(cx).has_unsaved_work());
        if dirty {
            let shell = cx.weak_entity();
            window.open_dialog(cx, move |dialog, _, _| {
                let owner = shell.clone();
                let cancel = shell.clone();
                dialog
                    .title("Switch server?")
                    .close_button(false)
                    .child("Unsaved server settings will be discarded. Chat drafts stay with their server.")
                    .footer(
                        h_flex().justify_end().gap_2()
                            .child(
                                Button::new("switch-cancel")
                                    .focus_ring(false).cursor_pointer().label("Keep editing").ghost()
                                    .on_click(move |_, window, cx| {
                                        let _ = cancel.update(cx, |shell, _| shell.notification_target = None);
                                        window.close_dialog(cx);
                                    })
                            )
                            .child(
                                Button::new("switch-confirm")
                                    .focus_ring(false).cursor_pointer().label("Discard and switch")
                                    .on_click(move |_, window, cx| {
                                        window.close_dialog(cx);
                                        let _ = owner.update(cx, |shell, cx| shell.switch_server(id, window, cx));
                                    })
                            )
                    )
            });
        } else {
            self.switch_server(id, window, cx);
        }
    }

    fn switch_server(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(setup) = &mut self.setup {
            setup._attempt = None;
            setup.draft.edited();
        }
        let Some(saved) = self.registry.find(id).cloned() else {
            return;
        };
        let client = match Client::new(
            saved.profile.origin.clone(),
            &saved.profile.token,
            self.runtime.clone(),
        ) {
            Ok(client) => Arc::new(client),
            Err(err) => {
                self.connection_error = Some(err);
                cx.notify();
                return;
            }
        };
        let local = self
            .launch
            .updates
            .as_ref()
            .ok()
            .and_then(|installation| installation.server())
            .filter(|local| local.owns(&saved.profile))
            .cloned();
        let runtime = self.runtime.clone();
        let attempt = Uuid::new_v4();
        self.switch_attempt = Some(attempt);
        self.connection_error = None;
        self._switch = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = if let Some(local) = local {
                let mut steps = local.connect(client.clone(), &runtime, Limits::default());
                let started = loop {
                    match steps.recv().await {
                        Some(Step::Phase(_)) => {}
                        Some(Step::Done(result)) => {
                            break result.map_err(|error| error.to_string());
                        }
                        None => break Err("The local connection was interrupted.".into()),
                    }
                };
                match started {
                    Ok(()) => verify(saved.profile.clone(), client).await,
                    Err(error) => Err(error),
                }
            } else {
                verify(saved.profile.clone(), client).await
            };
            let _ = this.update_in(cx, |shell, window, cx| {
                if shell.switch_attempt != Some(attempt)
                    || !shell
                        .registry
                        .find(id)
                        .is_some_and(|s| s.profile == saved.profile)
                {
                    return;
                }
                shell.switch_attempt = None;
                let verified = match outcome {
                    Ok(verified) => verified,
                    Err(err) => {
                        shell.notification_target = None;
                        shell.connection_error = Some(format!(
                            "Could not use {}. {err} Your current server is unchanged.",
                            saved.name
                        ));
                        cx.notify();
                        return;
                    }
                };
                let mut registry = shell.registry.clone();
                registry.selected = Some(id);
                if let Err(err) = shell
                    .launch
                    .store
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(|store| store.save(&registry))
                {
                    shell.connection_error = Some(err);
                    cx.notify();
                    return;
                }
                let section = shell.settings_section();
                shell.registry = registry;
                shell.server_editor = None;
                shell.server_update_target = None;
                shell.activate(verified.profile, verified.client, window, cx);
                if let Some(section) = section {
                    shell
                        .active
                        .as_ref()
                        .unwrap()
                        .app
                        .update(cx, |app, cx| app.set_hidden(true, window, cx));
                    if let Some(setup) = &mut shell.setup {
                        setup.page = Page::Welcome;
                        setup.draft.edited();
                        setup._attempt = None;
                    }
                    shell.select_settings(section, window, cx);
                }
                if let Some((target, scope, bot)) = shell.notification_target.take()
                    && target == id
                {
                    shell.open_notification(scope, bot, window, cx);
                }
                cx.notify();
            });
        }));
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
        window.open_dialog(cx, move |dialog, _, _| {
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
                                .focus_ring(false)
                                .cursor_pointer()
                                .label("Keep connection")
                                .ghost()
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("remove-server-confirm")
                                .focus_ring(false)
                                .cursor_pointer()
                                .label("Remove connection")
                                .danger()
                                .on_click({
                                    let shell = shell.clone();
                                    move |_, window, cx| {
                                        window.close_dialog(cx);
                                        let _ = shell.update(cx, |shell, cx| {
                                            let mut registry = shell.registry.clone();
                                            let result = registry.remove(id).and_then(|()| {
                                                shell
                                                    .launch
                                                    .store
                                                    .as_ref()
                                                    .map_err(Clone::clone)
                                                    .and_then(|store| store.save(&registry))
                                            });
                                            match result {
                                                Ok(()) => {
                                                    shell.registry = registry;
                                                    shell.inactive.remove(&id);
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
        if self.managed_server(&saved.profile) {
            self.remote_updates.remove(&id);
        } else if !self
            .remote_updates
            .get(&id)
            .is_some_and(|view| view.remote_profile.as_ref() == Some(&saved.profile))
        {
            match updates::View::new_remote(id, saved.profile.clone(), &self.runtime, window, cx) {
                Ok(view) => {
                    self.remote_updates.insert(id, view);
                }
                Err(error) => {
                    self.connection_error = Some(error);
                    cx.notify();
                    return;
                }
            }
        }
        self.server_update_target = Some(id);
        let commands = self
            .remote_updates
            .get(&id)
            .unwrap_or(&self.server_updates)
            .commands
            .clone();
        commands.check().ok();
        commands.refresh_work().ok();
        cx.notify();
    }

    pub(super) fn close_server_update(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.server_update_target = None;
        cx.notify();
    }
}

fn version_numbers(value: &str) -> Option<(u64, u64, u64)> {
    let mut parts = value.strip_prefix('v').unwrap_or(value).split('.');
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
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        routing::get,
    };
    use serde_json::json;

    const TOKEN: &str = "TTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTT";

    async fn fixture(
        version: &str,
        api_version: &str,
        ready: StatusCode,
        report: bool,
        expected_token: &str,
    ) -> (Profile, tokio::task::JoinHandle<()>) {
        let version = version.to_owned();
        let api_version = api_version.to_owned();
        let expected = format!("Bearer {expected_token}");
        let app = Router::new()
            .route("/v1/capabilities", get(move |headers: HeaderMap| {
                let api_version = api_version.clone();
                let expected = expected.clone();
                async move {
                    let allowed = headers.get("authorization").and_then(|value| value.to_str().ok()) == Some(expected.as_str());
                    let status = if allowed { StatusCode::OK } else { StatusCode::UNAUTHORIZED };
                    let body = json!({"api_version":api_version,"providers":[],"limits":{"max_text_bytes":32768}});
                    (status, Json(body))
                }
            }))
            .route("/readyz", get(move || async move {
                (ready, Json(json!({"ready":ready.is_success()})))
            }))
            .route("/v1/maintenance/update", get(move || {
                let version = version.clone();
                async move {
                    let status = if report { StatusCode::OK } else { StatusCode::NOT_FOUND };
                    (status, Json(json!({"version":version,"active_turns":2,"running_commands":1})))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (Profile::parse(&origin, TOKEN).unwrap(), server)
    }

    async fn probe(profile: Profile) -> Result<(Option<String>, Option<u64>), (Health, String)> {
        let client = Arc::new(
            Client::new(
                profile.origin,
                &profile.token,
                tokio::runtime::Handle::current(),
            )
            .unwrap(),
        );
        observe(client).await
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

    #[tokio::test]
    async fn saved_servers_report_their_own_authenticated_version_and_work() {
        let (a, a_server) = fixture("0.0.20", "v1", StatusCode::OK, true, TOKEN).await;
        let (b, b_server) = fixture(
            "0.0.24",
            "v1",
            StatusCode::OK,
            true,
            "UUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUU",
        )
        .await;
        let b = Profile::parse(
            b.origin.as_str(),
            "UUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUU",
        )
        .unwrap();
        let (first, second) = tokio::join!(probe(a), probe(b));
        assert_eq!(first.unwrap(), (Some("0.0.20".into()), Some(3)));
        assert_eq!(second.unwrap(), (Some("0.0.24".into()), Some(3)));
        a_server.abort();
        b_server.abort();
    }

    #[tokio::test]
    async fn connection_health_distinguishes_authentication_readiness_and_protocol() {
        for (api_version, ready, token, expected) in [
            ("v1", StatusCode::OK, "wrong", Health::Unauthorized),
            (
                "v1",
                StatusCode::SERVICE_UNAVAILABLE,
                TOKEN,
                Health::Offline,
            ),
            ("v2", StatusCode::OK, TOKEN, Health::Incompatible),
        ] {
            let (profile, server) = fixture("0.0.24", api_version, ready, true, token).await;
            let (health, _) = probe(profile).await.unwrap_err();
            assert!(health == expected);
            server.abort();
        }
    }

    #[tokio::test]
    async fn older_server_without_version_reporting_remains_online() {
        let (profile, server) = fixture("0.0.24", "v1", StatusCode::OK, false, TOKEN).await;
        assert_eq!(probe(profile).await.unwrap(), (None, None));
        server.abort();
    }
}
