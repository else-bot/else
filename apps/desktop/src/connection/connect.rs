//! Connecting to a server: the onboarding form and the settings editor
//! that adds or edits one, the check before saving, and saving it.

use super::*;

pub(super) struct Verified {
    pub profile: Profile,
    pub client: Arc<Client>,
}

pub(super) enum Check {
    Unchecked,
    Checking(Phase),
    Failed(String),
}

pub(super) struct Draft {
    revision: u64,
    pub check: Check,
}

impl Draft {
    pub(super) fn new() -> Self {
        Self {
            revision: 0,
            check: Check::Unchecked,
        }
    }

    pub(super) fn edited(&mut self) {
        self.revision += 1;
        self.check = Check::Unchecked;
    }

    pub(super) fn begin(&mut self) -> u64 {
        self.revision += 1;
        self.check = Check::Checking(Phase::Connecting);
        self.revision
    }

    pub(super) fn fail(&mut self, err: String) {
        self.revision += 1;
        self.check = Check::Failed(err);
    }

    pub(super) fn progress(&mut self, revision: u64, phase: Phase) -> bool {
        if !self.accepts(revision) {
            return false;
        }
        self.check = Check::Checking(phase);
        true
    }

    pub(super) fn finish(
        &mut self,
        revision: u64,
        outcome: Result<Verified, String>,
    ) -> Option<Verified> {
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

    pub(super) fn busy(&self) -> Option<Phase> {
        match self.check {
            Check::Checking(phase) => Some(phase),
            _ => None,
        }
    }

    pub(super) fn accepts(&self, revision: u64) -> bool {
        revision == self.revision && self.busy().is_some()
    }
}

pub(super) async fn verify(profile: Profile, client: Arc<Client>) -> Result<Verified, String> {
    match client.clone().capabilities().await {
        Ok(caps) if caps.api_version.as_deref().is_some_and(|version| version != "v1") => Err("This server uses an unsupported API version. Update the desktop app before connecting.".into()),
        Ok(_) => Ok(Verified { profile, client }),
        Err(ClientError::Unauthorized) => Err("The server rejected this owner token.".into()),
        Err(err) => Err(err.to_string()),
    }
}

pub(super) struct RemoteForm {
    pub origin: Entity<InputState>,
    pub token: Entity<InputState>,
    _subscriptions: [Subscription; 2],
}

pub(super) enum Page {
    Welcome,
    Remote(RemoteForm),
}

impl Page {
    /// Wipes the typed owner token and its undo history. The last drawn
    /// frame still holds the field, and a hidden window draws no other.
    fn close(self, window: &mut Window, cx: &mut App) {
        if let Self::Remote(form) = self {
            form.token
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
    }
}

/// The form that connects to a server: onboarding's local or remote
/// choice, and the settings editor that adds or edits one.
pub(super) struct ConnectForm {
    pub id: Uuid,
    pub page: Page,
    pub draft: Draft,
    pub name: Entity<InputState>,
    pub error: Option<String>,
    /// Where focus returns when the form goes back to its first page.
    pub focus: FocusHandle,
    _attempt: Option<Task<()>>,
}

impl ConnectForm {
    pub(super) fn new(
        error: Option<String>,
        focus: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Shell>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            page: Page::Welcome,
            draft: Draft::new(),
            name: cx.new(|cx| InputState::new(window, cx).placeholder("Server name")),
            error,
            focus,
            _attempt: None,
        }
    }

    /// Outdates the attempt in flight, so its reply never lands.
    pub(super) fn edited(&mut self) {
        self.draft.edited();
        self._attempt = None;
    }

    /// Replaces the page, wiping the owner token typed on the old one.
    pub(super) fn set_page(&mut self, page: Page, window: &mut Window, cx: &mut App) {
        std::mem::replace(&mut self.page, page).close(window, cx);
    }

    pub(super) fn close(self, window: &mut Window, cx: &mut App) {
        self.page.close(window, cx);
    }

    pub(super) fn typed_name(&self, cx: &App) -> Option<String> {
        Some(self.name.read(cx).value().trim().to_owned()).filter(|name| !name.is_empty())
    }
}

impl Shell {
    pub(super) fn open_remote(
        &mut self,
        profile: Option<&Profile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.front.form().is_none() {
            return;
        }
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
                    if let Some(form) = shell.front.form_mut() {
                        form.edited();
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
        let Some(form) = self.front.form_mut() else {
            return;
        };
        form.edited();
        form.set_page(
            Page::Remote(RemoteForm {
                origin,
                token,
                _subscriptions: subscriptions,
            }),
            window,
            cx,
        );
        cx.notify();
    }

    pub(super) fn back_to_welcome(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.front.form_mut() else {
            return;
        };
        form.edited();
        form.set_page(Page::Welcome, window, cx);
        form.focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn start_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(local) = &self.launch.local else {
            return;
        };
        let Some(setup) = self.front.form_mut() else {
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
        let setup_id = setup.id;
        let mut steps = local.connect(client.clone(), &self.runtime, Limits::default());
        setup._attempt = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = loop {
                match steps.recv().await {
                    Some(Step::Phase(phase)) => {
                        let _ = this.update(cx, |shell, cx| {
                            shell.progress(setup_id, revision, phase, cx)
                        });
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
                shell.finish(setup_id, revision, outcome, window, cx)
            });
        }));
        cx.notify();
    }

    pub(super) fn connect_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editing = self
            .front
            .settings()
            .and_then(|settings| settings.servers().editing());
        let onboarding = matches!(self.front, Front::Onboarding(_));
        let Some(setup) = self.front.form_mut() else {
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
        let token = if token.trim().is_empty() {
            editing
                .and_then(|id| self.registry.find(id))
                .filter(|saved| Origin::parse(&origin).ok().as_ref() == Some(&saved.profile.origin))
                .map(|saved| saved.profile.token.expose().to_owned())
                .unwrap_or_else(|| token.to_string())
        } else {
            token.to_string()
        };
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
        if onboarding
            && self
                .active
                .as_ref()
                .is_some_and(|active| active.profile == profile && active.app.read(cx).connected())
        {
            self.cancel(window, cx);
            window.close_dialog(cx);
            return;
        }
        let revision = setup.draft.begin();
        let setup_id = setup.id;
        setup._attempt = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = verify(profile, client).await;
            let _ = this.update_in(cx, |shell, window, cx| {
                shell.finish(setup_id, revision, outcome, window, cx)
            });
        }));
        cx.notify();
    }

    fn progress(&mut self, setup_id: Uuid, revision: u64, phase: Phase, cx: &mut Context<Self>) {
        if let Some(form) = self.front.form_mut()
            && form.id == setup_id
            && form.draft.progress(revision, phase)
        {
            cx.notify();
        }
    }

    fn finish(
        &mut self,
        setup_id: Uuid,
        revision: u64,
        outcome: Result<Verified, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.front.form_mut() else {
            return;
        };
        if form.id != setup_id || !form.draft.accepts(revision) {
            return;
        }
        let verified = form.draft.finish(revision, outcome);
        form._attempt = None;
        let name = form.typed_name(cx);
        if let Ok(local) = &self.launch.local
            && matches!(form.page, Page::Welcome)
        {
            self.key = Some(LocalServer::key_state(&local.key()));
        }
        cx.notify();
        let Some(verified) = verified else {
            return;
        };
        match &self.front {
            Front::Onboarding(_) => self.finish_onboarding(verified, name, window, cx),
            Front::Settings(settings) => match settings.servers() {
                ServerPage::Add(_) => self.finish_add(verified, name, window, cx),
                ServerPage::Edit { id, opened, .. } => {
                    let name = name.filter(|name| name != opened);
                    self.finish_edit(*id, verified, name, window, cx)
                }
                ServerPage::List | ServerPage::Update(_) => {}
            },
            Front::Session => {}
        }
        cx.notify();
    }

    /// The first server, or the saved one checked again on launch, opens.
    fn finish_onboarding(
        &mut self,
        Verified { profile, client }: Verified,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.save_verified(&profile, name, None, false, window, cx) {
            Ok(_) => self.adopt(profile, client, false, window, cx),
            Err(error) => self.refuse(error),
        }
    }

    /// A new server is saved beside the active one.
    fn finish_add(
        &mut self,
        Verified { profile, client }: Verified,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .registry
            .servers
            .iter()
            .any(|saved| saved.profile.origin == profile.origin)
        {
            self.refuse(
                "This server address is already saved. Edit its existing connection instead."
                    .into(),
            );
            return;
        }
        match self.save_verified(&profile, name, None, true, window, cx) {
            Ok(true) => self.adopt(profile, client, true, window, cx),
            Ok(false) => self.show_server_list(window, cx),
            Err(error) => self.refuse(error),
        }
    }

    /// A saved server's new name, address or token is saved; the active
    /// server reconnects with it.
    fn finish_edit(
        &mut self,
        id: Uuid,
        Verified { profile, client }: Verified,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.save_verified(&profile, name, Some(id), true, window, cx) {
            Ok(true) => self.adopt(profile, client, true, window, cx),
            Ok(false) => self.show_server_list(window, cx),
            Err(error) => self.refuse(error),
        }
    }

    /// Saves a checked connection to disk before anything uses it, and
    /// returns whether it becomes the active server. A server with unsent
    /// drafts keeps its address and token.
    fn save_verified(
        &mut self,
        profile: &Profile,
        name: Option<String>,
        editing: Option<Uuid>,
        settings_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let typed = name.is_some();
        let before = editing
            .and_then(|id| self.registry.find(id))
            .map(|saved| saved.name.clone());
        let mut registry = self.registry.clone();
        let id = registry.upsert(profile.clone(), name, editing)?;
        let previous = self
            .active
            .as_ref()
            .filter(|session| session.id == id)
            .or_else(|| self.inactive.get(&id));
        if previous.is_some_and(|session| {
            session.profile != *profile && session.app.read(cx).has_unsaved_work(cx)
        }) {
            return Err("This server has unsent drafts or pending messages. Finish them before changing its address or owner token. You can still rename it.".into());
        }
        let activate = !settings_open || self.active.as_ref().is_some_and(|active| active.id == id);
        if activate {
            registry.selected = Some(id);
        }
        self.launch
            .store
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|store| store.save(&registry))?;
        if self
            .inactive
            .get(&id)
            .is_some_and(|session| session.profile != *profile)
            && let Some(retired) = self.inactive.remove(&id)
        {
            retired
                .app
                .update(cx, |app, cx| app.dismiss_notifications(cx));
        }
        let renamed = registry
            .find(id)
            .map(|saved| saved.name.clone())
            .filter(|name| typed && before.as_ref() != Some(name));
        self.registry = registry;
        if let Some(name) = renamed {
            self.name_server(
                id,
                servers::NameWrite {
                    name,
                    carried: false,
                },
                cx,
            );
        }
        self.refresh_servers(window, cx);
        Ok(activate)
    }

    /// Makes a saved connection the active server, reopening settings
    /// over it when they were open.
    fn adopt(
        &mut self,
        profile: Profile,
        client: Arc<Client>,
        settings_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if settings_open {
            window.close_dialog(cx);
        }
        self.set_front(Front::Session, window, cx);
        self.activate(profile, client, window, cx);
        if settings_open {
            self.open_settings(window, cx);
        }
    }

    fn refuse(&mut self, error: String) {
        if let Some(form) = self.front.form_mut() {
            form.error = Some(error);
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
        let pending = draft.begin();
        draft.fail(err);
        assert!(
            !draft.progress(pending, Phase::Waiting),
            "a failure also ends older attempts"
        );
        assert!(draft.finish(pending, Err("late".into())).is_none());
        assert!(matches!(&draft.check, Check::Failed(e) if e.contains("malformed")));
    }
}
