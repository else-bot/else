//! The window's root: which server the desktop uses, and the setup screens
//! that choose it. A connection is saved only after the server accepted its
//! owner token, and saving replaces the whole messaging view, so nothing
//! started for the previous server can reach the new one.

mod backups;
mod settings;
mod updates;
mod view;

use settings::Presentation;

use std::sync::Arc;

use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, TitleBar, WindowExt as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::{Client, ClientError};
use crate::app::ElseApp;
use crate::config::{Origin, Profile};
use crate::launch::{
    KeyState, Launch, Limits, LocalServer, NoLocal, Phase, RemoteSeed, Step, this_machine,
};

const STORY_MIN_WIDTH: f32 = 760.;

pub(crate) const SHELL_CONTEXT: &str = "AppShell";

actions!(else_desktop, [OpenAppSettings]);

struct Verified {
    profile: Profile,
    client: Arc<Client>,
}

enum Check {
    Unchecked,
    Checking(Phase),
    Failed(String),
}

struct Draft {
    revision: u64,
    check: Check,
}

impl Draft {
    fn new() -> Self {
        Self {
            revision: 0,
            check: Check::Unchecked,
        }
    }

    fn edited(&mut self) {
        self.revision += 1;
        self.check = Check::Unchecked;
    }

    fn begin(&mut self) -> u64 {
        self.revision += 1;
        self.check = Check::Checking(Phase::Connecting);
        self.revision
    }

    fn fail(&mut self, err: String) {
        self.revision += 1;
        self.check = Check::Failed(err);
    }

    fn progress(&mut self, revision: u64, phase: Phase) -> bool {
        if !self.accepts(revision) {
            return false;
        }
        self.check = Check::Checking(phase);
        true
    }

    fn finish(&mut self, revision: u64, outcome: Result<Verified, String>) -> Option<Verified> {
        if !self.accepts(revision) {
            return None;
        }
        match outcome {
            Ok(verified) => {
                self.check = Check::Unchecked;
                Some(verified)
            }
            Err(err) => {
                self.check = Check::Failed(err);
                None
            }
        }
    }

    fn busy(&self) -> Option<Phase> {
        match self.check {
            Check::Checking(phase) => Some(phase),
            _ => None,
        }
    }

    fn accepts(&self, revision: u64) -> bool {
        revision == self.revision && self.busy().is_some()
    }
}

async fn verify(profile: Profile, client: Arc<Client>) -> Result<Verified, String> {
    match client.clone().capabilities().await {
        Ok(_) => Ok(Verified { profile, client }),
        Err(ClientError::Unauthorized) => Err("The server rejected this owner token.".into()),
        Err(err) => Err(err.to_string()),
    }
}

struct Active {
    profile: Profile,
    client: Arc<Client>,
    app: Entity<ElseApp>,
    _app: Subscription,
}

struct RemoteForm {
    origin: Entity<InputState>,
    token: Entity<InputState>,
    _subscriptions: [Subscription; 2],
}

enum Page {
    Welcome,
    Remote(RemoteForm),
}

struct Setup {
    presentation: Presentation,
    page: Page,
    draft: Draft,
    error: Option<String>,
    focus: FocusHandle,
    navigation_focus: [FocusHandle; 4],
    _attempt: Option<Task<()>>,
}

pub struct Shell {
    launch: Launch,
    key: Option<KeyState>,
    runtime: tokio::runtime::Handle,
    active: Option<Active>,
    setup: Option<Setup>,
    settings_focus: FocusHandle,
    updates: updates::View,
    backups: Option<backups::View>,
}

impl Shell {
    /// Connect with the saved profile; without one (the first launch) or
    /// with an unusable one, open the welcome screen.
    pub fn new(
        launch: Launch,
        runtime: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let loaded = launch.store.clone().and_then(|store| store.load());
        // The platform keeps one handler for the whole process.
        let this = cx.weak_entity();
        let handle = window.window_handle();
        cx.on_system_notification_response(move |response, cx| {
            let this = this.clone();
            let _ = handle.update(cx, |_, window, cx| {
                let Some((scope, bot)) = crate::inbox::parse_tag(&response.tag) else {
                    return;
                };
                let _ = this.update(cx, |shell, cx| {
                    shell.open_notification(scope, bot, window, cx)
                });
            });
        });
        let key = launch
            .local
            .as_ref()
            .ok()
            .map(|local| LocalServer::key_state(&local.key()));
        let updates = updates::View::new(&launch, &runtime, window, cx);
        let mut shell = Self {
            launch,
            key,
            runtime,
            active: None,
            setup: None,
            settings_focus: cx.focus_handle(),
            updates,
            backups: None,
        };
        match loaded {
            Ok(Some(profile))
                if shell
                    .launch
                    .local
                    .as_ref()
                    .is_ok_and(|local| local.owns(&profile)) =>
            {
                shell.open_setup(None, window, cx);
                shell.start_local(window, cx);
            }
            Ok(Some(profile)) => {
                match Client::new(
                    profile.origin.clone(),
                    &profile.token,
                    shell.runtime.clone(),
                ) {
                    Ok(client) => shell.activate(profile, Arc::new(client), window, cx),
                    Err(err) => {
                        shell.open_setup(Some(err), window, cx);
                        shell.open_remote(Some(&profile), window, cx);
                    }
                }
            }
            Ok(None) => shell.open_first(None, window, cx),
            Err(err) => shell.open_first(Some(err), window, cx),
        }
        shell
    }

    fn open_first(&mut self, error: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.open_setup(error, window, cx);
        if self.launch.local.is_err() && self.launch.remote_seed.is_some() {
            self.open_remote(None, window, cx);
        }
    }

    fn open_setup(&mut self, error: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        self.setup = Some(Setup {
            presentation: Presentation::Onboarding,
            page: Page::Welcome,
            draft: Draft::new(),
            error,
            focus,
            navigation_focus: std::array::from_fn(|_| cx.focus_handle()),
            _attempt: None,
        });
        cx.notify();
    }

    fn activate(
        &mut self,
        profile: Profile,
        client: Arc<Client>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(previous) = self.active.take() {
            previous
                .app
                .update(cx, |app, cx| app.dismiss_notifications(cx));
        }
        let preferences = self
            .launch
            .store
            .as_ref()
            .map(|store| crate::preferences::Preferences::load(store.preferences_path()))
            .unwrap_or_default();
        self.backups = None;
        let app = cx.new(|cx| ElseApp::new(client.clone(), preferences, window, cx));
        let observer = cx.observe(&app, |_, _, cx| cx.notify());
        self.active = Some(Active {
            profile,
            client,
            app,
            _app: observer,
        });
        cx.notify();
    }

    fn open_notification(
        &mut self,
        scope: uuid::Uuid,
        bot: uuid::Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = &self.active else { return };
        let app = active.app.clone();
        if !app.update(cx, |app, cx| app.open_notification(scope, bot, window, cx)) {
            return;
        }
        cx.activate(true);
        window.activate_window();
        if self.setup.is_some() {
            if self.settings_section().is_some() {
                window.close_dialog(cx);
            }
            self.cancel(window, cx);
        }
    }

    fn open_remote(
        &mut self,
        profile: Option<&Profile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        let (origin, token) = remote_values(profile, self.launch.remote_seed.as_ref());
        let credential_origin = Origin::parse(&origin).ok();
        let origin = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://else.example.com")
                .default_value(origin)
        });
        let token = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Paste your owner key")
                .masked(true)
                .default_value(token)
        });
        let on_input = |shell: &mut Self,
                        _: &Entity<InputState>,
                        event: &InputEvent,
                        window: &mut Window,
                        cx: &mut Context<Self>| {
            match event {
                InputEvent::Change => {
                    if let Some(setup) = shell.setup.as_mut() {
                        setup.draft.edited();
                        setup._attempt = None;
                        cx.notify();
                    }
                }
                InputEvent::PressEnter { .. } => shell.connect_remote(window, cx),
                _ => {}
            }
        };
        let token_for_origin = token.clone();
        let subscriptions = [
            cx.subscribe_in(&origin, window, move |shell, field, event, window, cx| {
                if matches!(event, InputEvent::Change)
                    && prefill_origin_changed(credential_origin.as_ref(), &field.read(cx).value())
                {
                    token_for_origin.update(cx, |input, cx| input.set_value("", window, cx));
                }
                on_input(shell, field, event, window, cx);
            }),
            cx.subscribe_in(&token, window, on_input),
        ];
        origin.read(cx).focus_handle(cx).focus(window, cx);
        setup.draft.edited();
        setup._attempt = None;
        setup.page = Page::Remote(RemoteForm {
            origin,
            token,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    fn back_to_welcome(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        setup.draft.edited();
        setup._attempt = None;
        setup.page = Page::Welcome;
        setup.focus.focus(window, cx);
        cx.notify();
    }

    fn start_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(local) = &self.launch.local else {
            return;
        };
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        if setup.draft.busy().is_some() {
            return;
        }
        setup.error = None;
        let key = local.key();
        self.key = Some(LocalServer::key_state(&key));
        let prepared = key.map_err(|err| err.to_string()).and_then(|token| {
            let profile = Profile {
                origin: local.origin().clone(),
                token,
            };
            let client = Client::new(profile.origin.clone(), &profile.token, self.runtime.clone())?;
            Ok((profile, Arc::new(client)))
        });
        let (profile, client) = match prepared {
            Ok(prepared) => prepared,
            Err(err) => {
                setup.draft.fail(err);
                cx.notify();
                return;
            }
        };
        let revision = setup.draft.begin();
        let mut steps = local.connect(client.clone(), &self.runtime, Limits::default());
        setup._attempt = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = loop {
                match steps.recv().await {
                    Some(Step::Phase(phase)) => {
                        let _ = this.update(cx, |shell, cx| shell.progress(revision, phase, cx));
                    }
                    Some(Step::Done(result)) => {
                        break result
                            .map(|()| Verified { profile, client })
                            .map_err(|err| err.to_string());
                    }
                    None => break Err("The local connection was interrupted.".into()),
                }
            };
            let _ = this.update_in(cx, |shell, window, cx| {
                shell.finish(revision, outcome, window, cx)
            });
        }));
        cx.notify();
    }

    fn connect_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        let Page::Remote(form) = &setup.page else {
            return;
        };
        if setup.draft.busy().is_some() {
            return;
        }
        setup.error = None;
        let (origin, token) = (form.origin.read(cx).value(), form.token.read(cx).value());
        let prepared = Profile::parse(&origin, &token).and_then(|profile| {
            let client = Client::new(profile.origin.clone(), &profile.token, self.runtime.clone())?;
            Ok((profile, Arc::new(client)))
        });
        let (profile, client) = match prepared {
            Ok(prepared) => prepared,
            Err(err) => {
                setup.draft.fail(err);
                cx.notify();
                return;
            }
        };
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.profile == profile && active.app.read(cx).connected())
        {
            self.cancel(window, cx);
            window.close_dialog(cx);
            return;
        }
        let revision = setup.draft.begin();
        setup._attempt = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = verify(profile, client).await;
            let _ = this.update_in(cx, |shell, window, cx| {
                shell.finish(revision, outcome, window, cx)
            });
        }));
        cx.notify();
    }

    fn progress(&mut self, revision: u64, phase: Phase, cx: &mut Context<Self>) {
        if let Some(setup) = self.setup.as_mut()
            && setup.draft.progress(revision, phase)
        {
            cx.notify();
        }
    }

    fn finish(
        &mut self,
        revision: u64,
        outcome: Result<Verified, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        if !setup.draft.accepts(revision) {
            return;
        }
        let verified = setup.draft.finish(revision, outcome);
        setup._attempt = None;
        if let Ok(local) = &self.launch.local
            && matches!(setup.page, Page::Welcome)
        {
            self.key = Some(LocalServer::key_state(&local.key()));
        }
        cx.notify();
        let Some(Verified { profile, client }) = verified else {
            return;
        };
        if let Err(err) = self
            .launch
            .store
            .clone()
            .and_then(|store| store.save(&profile))
        {
            setup.error = Some(err);
            return;
        }
        if self.settings_section().is_some() {
            window.close_dialog(cx);
        }
        self.setup = None;
        self.activate(profile, client, window, cx);
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(active) = &self.active {
            self.updates.dismiss();
            self.backups = None;
            self.setup = None;
            active
                .app
                .update(cx, |app, cx| app.set_hidden(false, window, cx));
            cx.notify();
        }
    }
}

fn remote_values(profile: Option<&Profile>, seed: Option<&RemoteSeed>) -> (String, String) {
    if let Some(profile) = profile {
        let token = if profile.origin.is_loopback() {
            String::new()
        } else {
            profile.token.expose().to_owned()
        };
        return (profile.origin.as_str().to_owned(), token);
    }
    seed.map(|seed| {
        (
            seed.origin.clone(),
            seed.token
                .as_ref()
                .map(|token| token.expose().to_owned())
                .unwrap_or_default(),
        )
    })
    .unwrap_or_default()
}

fn prefill_origin_changed(bound: Option<&Origin>, raw: &str) -> bool {
    bound.is_some_and(|bound| Origin::parse(raw).ok().as_ref() != Some(bound))
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let root =
            v_flex()
                .key_context(SHELL_CONTEXT)
                .on_action(cx.listener(|shell, _: &OpenAppSettings, window, cx| {
                    shell.open_settings(window, cx)
                }))
                .size_full()
                .bg(theme.background)
                .text_color(theme.foreground);
        if let Some(setup) = &self.setup
            && setup.presentation == Presentation::Onboarding
        {
            let wide = window.viewport_size().width >= px(STORY_MIN_WIDTH);
            let start_on_enter = matches!(setup.page, Page::Welcome) && self.launch.local.is_ok();
            // macOS still needs a title bar to drag the window by.
            return root
                .when(cfg!(target_os = "macos"), |root| {
                    root.child(TitleBar::new())
                })
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .track_focus(&setup.focus)
                        .when(start_on_enter, |panel| {
                            panel.on_key_down(cx.listener(
                                |shell, event: &KeyDownEvent, window, cx| {
                                    if event.keystroke.key == "enter" {
                                        shell.start_local(window, cx);
                                    }
                                },
                            ))
                        })
                        .child(self.render_setup(setup, wide, cx)),
                );
        }
        let Some(active) = &self.active else {
            return root;
        };
        root.child(self.render_bar(active, window, cx))
            .child(div().flex_1().min_h_0().w_full().child(active.app.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests::serve_once;

    const TOKEN: &str = "TTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTT";
    const CAPABILITIES: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 50\r\n\r\n{\"providers\":[],\"limits\":{\"max_text_bytes\":32768}}";

    #[::core::prelude::v1::test]
    fn local_credentials_are_not_prefilled_when_changing_servers() {
        for address in [
            "http://127.0.0.1:7878",
            "https://localhost:7878",
            "http://[::1]:7878",
        ] {
            let profile = Profile::parse(address, TOKEN).unwrap();
            let (origin, token) = remote_values(Some(&profile), None);
            assert_eq!(origin, profile.origin.as_str());
            assert!(token.is_empty());
        }
        let remote = Profile::parse("https://server.example.com", TOKEN).unwrap();
        assert_eq!(remote_values(Some(&remote), None).1, TOKEN);
        assert!(prefill_origin_changed(
            Some(&remote.origin),
            "https://another.example.com"
        ));
        assert!(prefill_origin_changed(Some(&remote.origin), "invalid"));
        assert!(!prefill_origin_changed(
            Some(&remote.origin),
            "https://server.example.com/"
        ));
    }

    fn prepare(origin: &str, token: &str) -> Result<(Profile, Arc<Client>), String> {
        let profile = Profile::parse(origin, token)?;
        let client = Client::new(
            profile.origin.clone(),
            &profile.token,
            tokio::runtime::Handle::current(),
        )?;
        Ok((profile, Arc::new(client)))
    }

    #[tokio::test]
    async fn only_the_current_attempt_is_saved() {
        let (origin, server) = serve_once(CAPABILITIES).await;
        let mut draft = Draft::new();
        let (profile, client) = prepare(origin.as_str(), TOKEN).unwrap();
        let revision = draft.begin();
        assert_eq!(draft.busy(), Some(Phase::Connecting));
        assert!(draft.progress(revision, Phase::Starting));
        assert!(
            !draft.progress(revision - 1, Phase::Waiting),
            "an older attempt"
        );
        assert_eq!(draft.busy(), Some(Phase::Starting));
        let outcome = verify(profile.clone(), client).await;
        assert!(
            server
                .await
                .unwrap()
                .contains(&format!("authorization: Bearer {TOKEN}"))
        );
        let verified = draft
            .finish(revision, outcome)
            .expect("the current attempt");
        assert_eq!(verified.profile, profile);
        assert!(draft.busy().is_none());
        assert!(
            draft.finish(revision, Err("late".into())).is_none(),
            "an attempt finishes once"
        );
    }

    #[tokio::test]
    async fn a_stale_attempt_never_applies() {
        let (origin, server) = serve_once(CAPABILITIES).await;
        let mut draft = Draft::new();
        let (profile, client) = prepare(origin.as_str(), TOKEN).unwrap();
        let revision = draft.begin();
        let outcome = verify(profile.clone(), client.clone()).await;
        server.await.unwrap();
        draft.edited();
        assert!(
            draft.finish(revision, outcome).is_none(),
            "edited while checking"
        );
        assert!(matches!(draft.check, Check::Unchecked));
        assert!(!draft.progress(revision, Phase::Waiting));

        let newer = draft.begin();
        let late = Ok(Verified { profile, client });
        assert!(draft.finish(newer - 1, late).is_none(), "an older attempt");
        assert!(matches!(draft.check, Check::Checking(_)));
        assert!(draft.finish(newer, Err("refused".into())).is_none());
        assert!(matches!(&draft.check, Check::Failed(e) if e == "refused"));
    }

    #[tokio::test]
    async fn rejected_token_and_invalid_inputs_fail() {
        let (origin, _server) = serve_once(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        let mut draft = Draft::new();
        let (profile, client) = prepare(origin.as_str(), TOKEN).unwrap();
        let revision = draft.begin();
        let outcome = verify(profile, client).await;
        assert!(draft.finish(revision, outcome).is_none());
        assert!(matches!(&draft.check, Check::Failed(e) if e.contains("rejected")));

        let err = prepare("http://10.0.0.2:7878", TOKEN).err().unwrap();
        assert!(err.contains("unencrypted"), "{err}");
        let err = prepare(origin.as_str(), "short").err().unwrap();
        assert!(err.contains("malformed"), "{err}");
        let before = draft.revision;
        draft.fail(err);
        assert!(
            draft.revision > before,
            "a failure also ends older attempts"
        );
    }
}
