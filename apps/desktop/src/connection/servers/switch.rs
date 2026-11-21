//! Switching the shown server: a confirmation when settings hold unsaved
//! work, the switch itself, and its connection check behind it.

use super::*;

const PREFETCH_DWELL: Duration = Duration::from_millis(200);

impl Shell {
    pub(in crate::connection) fn select_server(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active.as_ref().is_some_and(|active| active.id == id) {
            self.arrival = None;
            cx.notify();
            return;
        }
        let dirty = self
            .front
            .settings()
            .is_some_and(|settings| settings.has_unsaved_work(cx));
        if dirty {
            let shell = cx.weak_entity();
            window.open_dialog(cx, move |dialog, _, cx| {
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
                                    .control(Tier::Ghost, Scale::Standard, false, cx)
                                    .label("Keep editing")
                                    .on_click(move |_, window, cx| {
                                        let _ = cancel.update(cx, |shell, _| shell.arrival = None);
                                        window.close_dialog(cx);
                                    })
                            )
                            .child(
                                Button::new("switch-confirm")
                                    .control(Tier::Secondary, Scale::Standard, false, cx)
                                    .label("Discard and switch")
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

    /// Shows the server at once, from its live session when it has one,
    /// and checks its connection behind it.
    fn switch_server(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(form) = self.front.form_mut() {
            form.edited();
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
                self.arrival = None;
                self.connection_error = Some(err);
                cx.notify();
                return;
            }
        };
        if let Err(err) = self.commit_registry(|registry| {
            registry.selected = Some(id);
            Ok(())
        }) {
            self.arrival = None;
            self.connection_error = Some(err);
            cx.notify();
            return;
        }
        self.connection_error = None;
        self._prefetch = None;
        let from = self.active.as_ref().map(|active| active.id);
        let section = self.settings_section();
        self.show_server_list();
        self.activate(saved.profile.clone(), client, window, cx);
        let Some(active) = self.active.as_ref().filter(|active| active.id == id) else {
            return;
        };
        let (client, connected) = (active.client.clone(), active.app.read(cx).connected());
        if let Some(section) = section {
            active
                .app
                .update(cx, |app, cx| app.set_hidden(true, window, cx));
            self.select_settings(section, window, cx);
        }
        if let Some(arrival) = self.arrival.take()
            && arrival.server == id
        {
            match arrival.open {
                Open::Notification { scope, bot } => self.open_notification(scope, bot, window, cx),
                Open::Bot(bot) => self.open_active_bot(bot, window, cx),
                Open::Server => {
                    if let Some(active) = &self.active {
                        active
                            .app
                            .update(cx, |app, cx| app.focus_sidebar(window, cx));
                    }
                }
            }
        }
        if connected {
            self.switch.settle();
        } else {
            self.check_switch(id, from, client, window, cx);
        }
        self.reclaim_switcher_focus(window, cx);
        cx.notify();
    }

    /// Checks the shown server's connection, starting it first when this
    /// desktop runs it.
    fn check_switch(
        &mut self,
        id: Uuid,
        from: Option<Uuid>,
        client: Arc<Client>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self.registry.find(id).cloned() else {
            return;
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
        self.switch.begin(id, from, |attempt| {
            cx.spawn_in(window, async move |this, cx| {
                let started = match local {
                    Some(local) => {
                        let mut steps = local.connect(client.clone(), &runtime, Limits::default());
                        loop {
                            match steps.recv().await {
                                Some(Step::Phase(_)) => {}
                                Some(Step::Done(result)) => {
                                    break result.map_err(|error| error.to_string());
                                }
                                None => break Err("The local connection was interrupted.".into()),
                            }
                        }
                    }
                    None => Ok(()),
                };
                let outcome = match started {
                    Ok(()) => verify(saved.profile, client).await.map(drop),
                    Err(error) => Err(error),
                };
                let _ = this.update(cx, |shell, cx| {
                    let live = shell
                        .active
                        .as_ref()
                        .filter(|active| active.id == id)
                        .is_some_and(|active| active.app.read(cx).connected());
                    if shell.switch.finish(attempt, outcome, live) {
                        cx.notify();
                    }
                });
            })
        });
    }

    /// Checks the failed server again.
    pub(in crate::connection) fn retry_switch(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(failure) = self.switch.failure() else {
            return;
        };
        let (id, from) = (failure.server, failure.from);
        let Some(active) = self.active.as_ref().filter(|active| active.id == id) else {
            self.switch.settle();
            cx.notify();
            return;
        };
        let client = active.client.clone();
        active.app.read(cx).reconnect_now();
        self.check_switch(id, from, client, window, cx);
        cx.notify();
    }

    /// Starts opening a hovered server's session after a short dwell.
    pub(in crate::connection) fn hover_server(
        &mut self,
        id: Uuid,
        hovered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !hovered {
            // Entering the next row can arrive before leaving this one.
            if self
                ._prefetch
                .as_ref()
                .is_some_and(|(owner, _)| *owner == id)
            {
                self._prefetch = None;
            }
            return;
        }
        let dwell = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(PREFETCH_DWELL).await;
            let _ = this.update_in(cx, |shell, window, cx| shell.prefetch(id, window, cx));
        });
        self._prefetch = Some((id, dwell));
    }

    /// Why the shown server could not be reached, with ways out.
    pub(in crate::connection) fn render_switch_failure(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let failure = self.switch.failure()?;
        let shown = self.active.as_ref()?;
        if shown.id != failure.server {
            return None;
        }
        let name = self.registry.find(failure.server)?.name.clone();
        let back = failure
            .from
            .and_then(|from| self.registry.find(from))
            .map(|saved| (saved.id, saved.name.clone()));
        let theme = cx.theme();
        Some(
            h_flex()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_center()
                .text_xs()
                .text_color(theme.danger)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(format!("Can't reach {name}. {}", failure.error)),
                )
                .child(
                    Button::new("switch-retry")
                        .control(Tier::Secondary, Scale::Compact, false, cx)
                        .label("Retry")
                        .on_click(
                            cx.listener(|shell, _, window, cx| shell.retry_switch(window, cx)),
                        ),
                )
                .children(back.map(|(id, name)| {
                    Button::new("switch-back")
                        .control(Tier::Ghost, Scale::Compact, false, cx)
                        .label(format!("Switch back to {name}"))
                        .on_click(cx.listener(move |shell, _, window, cx| {
                            shell.select_server(id, window, cx)
                        }))
                }))
                .into_any_element(),
        )
    }
}
