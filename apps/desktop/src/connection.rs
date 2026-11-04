//! The window's root: which server the desktop uses, and the editor that
//! changes it. A connection is saved only after the server accepted its
//! owner token, and saving replaces the whole messaging view, so nothing
//! started for the previous server can reach the new one.

use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, TitleBar, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::api::{Client, ClientError};
use crate::app::ElseApp;
use crate::config::{Origin, OwnerToken, Profile, ProfileStore, Seed};

/// A connection the server accepted, with the client that checked it.
struct Verified {
    profile: Profile,
    client: Arc<Client>,
}

enum Check {
    Unchecked,
    Checking,
    Ready(Verified),
    Failed(String),
}

/// The editor's validation state. Every edit and every new check advances
/// the revision, and a check's outcome applies only to its own revision.
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

    fn begin(
        &mut self,
        origin: &str,
        token: &str,
        runtime: &tokio::runtime::Handle,
    ) -> Option<(u64, Profile, Arc<Client>)> {
        self.revision += 1;
        let parsed = Origin::parse(origin).and_then(|origin| {
            let token = OwnerToken::parse(token)?;
            let client = Client::new(origin.clone(), &token, runtime.clone())?;
            Ok((Profile { origin, token }, Arc::new(client)))
        });
        match parsed {
            Ok((profile, client)) => {
                self.check = Check::Checking;
                Some((self.revision, profile, client))
            }
            Err(err) => {
                self.check = Check::Failed(err);
                None
            }
        }
    }

    fn finish(&mut self, revision: u64, outcome: Result<Verified, String>) -> bool {
        if revision != self.revision || !matches!(self.check, Check::Checking) {
            return false;
        }
        self.check = match outcome {
            Ok(verified) => Check::Ready(verified),
            Err(err) => Check::Failed(err),
        };
        true
    }

    fn ready_for(&self, origin: &str, token: &str) -> Option<&Verified> {
        let Check::Ready(verified) = &self.check else {
            return None;
        };
        let same = Origin::parse(origin).ok()? == verified.profile.origin
            && OwnerToken::parse(token).ok()? == verified.profile.token;
        same.then_some(verified)
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
    app: Entity<ElseApp>,
    _app: Subscription,
}

struct Editor {
    origin: Entity<InputState>,
    token: Entity<InputState>,
    draft: Draft,
    error: Option<String>,
    _probe: Option<Task<()>>,
    _subscriptions: [Subscription; 2],
}

pub struct Shell {
    store: Result<ProfileStore, String>,
    runtime: tokio::runtime::Handle,
    active: Option<Active>,
    editor: Option<Editor>,
}

impl Shell {
    /// Connect with the saved profile; without one (the first launch) or
    /// with an unusable one, open the editor with the launcher's settings.
    pub fn new(
        store: Result<ProfileStore, String>,
        seed: Seed,
        runtime: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let loaded = store.clone().and_then(|store| store.load());
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
        let mut shell = Self {
            store,
            runtime,
            active: None,
            editor: None,
        };
        let seed_token = seed
            .token
            .map(|t| t.expose().to_owned())
            .unwrap_or_default();
        match loaded {
            Ok(Some(profile)) => {
                match Client::new(
                    profile.origin.clone(),
                    &profile.token,
                    shell.runtime.clone(),
                ) {
                    Ok(client) => shell.activate(profile, Arc::new(client), window, cx),
                    Err(err) => shell.open_editor(
                        profile.origin.as_str().to_owned(),
                        profile.token.expose().to_owned(),
                        Some(err),
                        window,
                        cx,
                    ),
                }
            }
            Ok(None) => shell.open_editor(seed.origin, seed_token, None, window, cx),
            Err(err) => shell.open_editor(seed.origin, seed_token, Some(err), window, cx),
        }
        shell
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
        let app = cx.new(|cx| ElseApp::new(client, window, cx));
        let observer = cx.observe(&app, |_, _, cx| cx.notify());
        self.active = Some(Active {
            profile,
            app,
            _app: observer,
        });
        cx.notify();
    }

    fn change_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = &self.active else { return };
        let (origin, token) = (
            active.profile.origin.as_str().to_owned(),
            active.profile.token.expose().to_owned(),
        );
        // The messaging view's dialogs belong to the hidden view.
        window.close_dialog(cx);
        active
            .app
            .update(cx, |app, cx| app.set_hidden(true, window, cx));
        self.open_editor(origin, token, None, window, cx);
    }

    /// Show the notification's conversation, unless it belongs to another
    /// connection or its bot is gone.
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
        if self.editor.is_some() {
            self.cancel(window, cx);
        }
    }

    fn open_editor(
        &mut self,
        origin: String,
        token: String,
        error: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let origin = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://else.example.com or 127.0.0.1:7878")
                .default_value(origin)
        });
        let token = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Owner token")
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
                    if let Some(editor) = shell.editor.as_mut() {
                        editor.draft.edited();
                        editor.error = None;
                        editor._probe = None;
                        cx.notify();
                    }
                }
                InputEvent::PressEnter { .. } => shell.confirm(window, cx),
                _ => {}
            }
        };
        let subscriptions = [
            cx.subscribe_in(&origin, window, on_input),
            cx.subscribe_in(&token, window, on_input),
        ];
        origin.read(cx).focus_handle(cx).focus(window, cx);
        self.editor = Some(Editor {
            origin,
            token,
            draft: Draft::new(),
            error,
            _probe: None,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    fn inputs(editor: &Editor, cx: &App) -> (SharedString, SharedString) {
        (
            editor.origin.read(cx).value(),
            editor.token.read(cx).value(),
        )
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else { return };
        let (origin, token) = Self::inputs(editor, cx);
        if editor.draft.ready_for(&origin, &token).is_some() {
            self.save(window, cx);
        } else if !matches!(editor.draft.check, Check::Checking) {
            self.check(window, cx);
        }
    }

    fn check(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        let (origin, token) = Self::inputs(editor, cx);
        editor.error = None;
        let started = editor.draft.begin(&origin, &token, &self.runtime);
        editor._probe = started.map(|(revision, profile, client)| {
            cx.spawn_in(window, async move |this, cx| {
                let outcome = verify(profile, client).await;
                let _ = this.update(cx, |shell, cx| {
                    if let Some(editor) = shell.editor.as_mut()
                        && editor.draft.finish(revision, outcome)
                    {
                        cx.notify();
                    }
                });
            })
        });
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        let (origin, token) = Self::inputs(editor, cx);
        let Some(verified) = editor.draft.ready_for(&origin, &token) else {
            return;
        };
        let (profile, client) = (verified.profile.clone(), verified.client.clone());
        if let Err(err) = self.store.clone().and_then(|store| store.save(&profile)) {
            editor.error = Some(err);
            cx.notify();
            return;
        }
        self.editor = None;
        self.activate(profile, client, window, cx);
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(active) = &self.active {
            self.editor = None;
            active
                .app
                .update(cx, |app, cx| app.set_hidden(false, window, cx));
            cx.notify();
        }
    }

    /// The server and its connection state. On macOS this is the window's
    /// title bar, beside the traffic lights; elsewhere a slim strip under the
    /// desktop's own decorations.
    fn render_bar(&self, active: &Active, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let app = active.app.read(cx);
        let (connected, (status, _)) = (app.connected(), app.status_text());
        let origin = active.profile.origin.as_str().to_owned();
        let detail: SharedString = format!("{origin} · {status}").into();
        let tooltip = detail.clone();
        let label: SharedString = if connected { origin.into() } else { status };
        let content = h_flex()
            .flex_1()
            .min_w_0()
            .gap_2()
            .items_center()
            .child(
                h_flex()
                    .id("server-status")
                    .min_w_0()
                    .gap_1p5()
                    .items_center()
                    .aria_label(detail)
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .child(
                        div()
                            .flex_none()
                            .size_1p5()
                            .rounded_full()
                            .bg(if connected {
                                theme.success
                            } else {
                                theme.danger
                            }),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(label),
                    ),
            )
            .child(div().flex_1())
            .child(
                Button::new("server-change")
                    .label("Change server")
                    .xsmall()
                    .ghost()
                    .on_click(cx.listener(|shell, _, window, cx| shell.change_server(window, cx))),
            );
        if cfg!(target_os = "macos") {
            return TitleBar::new().pr_2().child(content).into_any_element();
        }
        h_flex()
            .flex_none()
            .h(px(30.))
            .px_3()
            .border_b_1()
            .border_color(theme.title_bar_border)
            .bg(theme.title_bar)
            .child(content)
            .into_any_element()
    }

    fn render_editor(&self, editor: &Editor, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let (origin, token) = Self::inputs(editor, cx);
        let ready = editor.draft.ready_for(&origin, &token).is_some();
        let checking = matches!(editor.draft.check, Check::Checking);
        let first = self.active.is_none();
        let (status, color) = match &editor.draft.check {
            Check::Unchecked => (
                "Check the connection before saving.".to_owned(),
                theme.muted_foreground,
            ),
            Check::Checking => ("Checking…".to_owned(), theme.muted_foreground),
            Check::Ready(_) => (
                "Connected: the server accepted the owner token.".to_owned(),
                theme.success,
            ),
            Check::Failed(err) => (err.clone(), theme.danger),
        };
        let field = |label: &'static str, input: Input| {
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(input)
        };
        let mut actions = h_flex().justify_end().gap_2();
        if !first {
            actions = actions.child(
                Button::new("connection-cancel")
                    .label("Cancel")
                    .ghost()
                    .on_click(cx.listener(|shell, _, window, cx| shell.cancel(window, cx))),
            );
        }
        actions = actions
            .child(
                Button::new("connection-check")
                    .label("Check connection")
                    .loading(checking)
                    .disabled(checking)
                    .on_click(cx.listener(|shell, _, window, cx| shell.check(window, cx))),
            )
            .child(
                Button::new("connection-save")
                    .label(if first { "Continue" } else { "Save" })
                    .primary()
                    .disabled(!ready)
                    .on_click(cx.listener(|shell, _, window, cx| shell.save(window, cx))),
            );
        let mut card = v_flex()
            .w(px(560.))
            .gap_4()
            .p_6()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(div().text_xl().child("Connect to your else.bot server"))
            .child(div().text_sm().text_color(theme.muted_foreground).child(
                "Enter the address of the machine that runs the else.bot server and its \
                 owner token. On that machine, show the token with: \
                 cat ~/.else/data/owner-token. Use https:// for a remote server; \
                 http:// works only for this machine or an SSH tunnel to it.",
            ))
            .child(field("Server address", Input::new(&editor.origin)))
            .child(field(
                "Owner token",
                Input::new(&editor.token).mask_toggle(),
            ));
        if let Some(error) = &editor.error {
            card = card.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        card = card
            .child(div().text_sm().text_color(color).child(status))
            .child(actions);
        h_flex()
            .size_full()
            .items_center()
            .justify_center()
            .bg(theme.muted)
            .child(card)
            .into_any_element()
    }
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let root = v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground);
        if let Some(editor) = &self.editor {
            // macOS still needs a title bar to drag the window by.
            return root
                .when(cfg!(target_os = "macos"), |root| {
                    root.child(TitleBar::new())
                })
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(self.render_editor(editor, cx)),
                );
        }
        let Some(active) = &self.active else {
            return root;
        };
        root.child(self.render_bar(active, cx))
            .child(div().flex_1().min_h_0().w_full().child(active.app.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests::serve_once;

    const TOKEN: &str = "TTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTT";
    const CAPABILITIES: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 50\r\n\r\n{\"providers\":[],\"limits\":{\"max_text_bytes\":32768}}";

    #[tokio::test]
    async fn only_the_current_verified_inputs_can_be_saved() {
        let (origin, server) = serve_once(CAPABILITIES).await;
        let runtime = tokio::runtime::Handle::current();
        let mut draft = Draft::new();
        let (revision, profile, client) = draft.begin(origin.as_str(), TOKEN, &runtime).unwrap();
        assert!(matches!(draft.check, Check::Checking));
        assert!(
            draft.ready_for(origin.as_str(), TOKEN).is_none(),
            "not while checking"
        );
        let outcome = verify(profile, client).await;
        assert!(
            server
                .await
                .unwrap()
                .contains(&format!("authorization: Bearer {TOKEN}"))
        );
        assert!(draft.finish(revision, outcome));
        assert!(draft.ready_for(origin.as_str(), TOKEN).is_some());
        assert!(
            draft.ready_for(origin.as_str(), &"U".repeat(43)).is_none(),
            "other token"
        );
        assert!(
            draft.ready_for("http://127.0.0.1:1", TOKEN).is_none(),
            "other origin"
        );
        draft.edited();
        assert!(matches!(draft.check, Check::Unchecked));
        assert!(
            draft.ready_for(origin.as_str(), TOKEN).is_none(),
            "an edit invalidates"
        );
    }

    #[tokio::test]
    async fn a_stale_check_never_applies() {
        let (origin, server) = serve_once(CAPABILITIES).await;
        let runtime = tokio::runtime::Handle::current();
        let mut draft = Draft::new();
        let (revision, profile, client) = draft.begin(origin.as_str(), TOKEN, &runtime).unwrap();
        let outcome = verify(profile.clone(), client.clone()).await;
        server.await.unwrap();
        draft.edited();
        assert!(!draft.finish(revision, outcome), "edited while checking");
        assert!(matches!(draft.check, Check::Unchecked));

        let (newer, ..) = draft.begin(origin.as_str(), TOKEN, &runtime).unwrap();
        let late = Ok(Verified { profile, client });
        assert!(!draft.finish(newer - 1, late), "an older check");
        assert!(matches!(draft.check, Check::Checking));
        assert!(draft.finish(newer, Err("refused".into())));
        assert!(matches!(&draft.check, Check::Failed(e) if e == "refused"));
        assert!(draft.ready_for(origin.as_str(), TOKEN).is_none());
    }

    #[tokio::test]
    async fn rejected_token_and_invalid_inputs_fail_the_check() {
        let (origin, _server) = serve_once(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        let runtime = tokio::runtime::Handle::current();
        let mut draft = Draft::new();
        let (revision, profile, client) = draft.begin(origin.as_str(), TOKEN, &runtime).unwrap();
        let outcome = verify(profile, client).await;
        assert!(draft.finish(revision, outcome));
        assert!(matches!(&draft.check, Check::Failed(e) if e.contains("rejected")));

        assert!(
            draft
                .begin("http://10.0.0.2:7878", TOKEN, &runtime)
                .is_none()
        );
        assert!(matches!(&draft.check, Check::Failed(e) if e.contains("unencrypted")));
        assert!(draft.begin(origin.as_str(), "short", &runtime).is_none());
        assert!(matches!(&draft.check, Check::Failed(e) if e.contains("malformed")));
    }
}
