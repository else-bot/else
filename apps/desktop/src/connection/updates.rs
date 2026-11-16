use super::*;
use crate::controls::{Control as _, Scale, Tier};
use crate::updates::{
    Channel as UpdateChannel, Check as UpdateCheck, Commands, Operation, Snapshot, Work,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Disableable as _, FocusableExt as _, Icon, Sizable as _, Theme, h_flex};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    App,
    LocalServer,
    RemoteServer(Uuid),
}

pub(super) struct View {
    pub(super) remote_profile: Option<Profile>,
    pub(super) commands: Commands,
    pub(super) snapshot: Snapshot,
    logs: Entity<TextareaState>,
    scroll: ScrollHandle,
    page_scroll: ScrollHandle,
    /// The reader's choice to show or hide the output for the current
    /// operation; `None` follows the default for that operation.
    output_open: Option<bool>,
    confirm_restart: bool,
    feedback: Option<String>,
    _watch: Task<()>,
}

#[derive(Clone, Copy)]
pub(super) enum Action {
    Check,
    Install,
    Restart,
    ConfirmRestart,
    CancelRestart,
    ToggleOutput,
    Copy,
    Follow,
}

impl View {
    pub(super) fn refresh(&self) {
        self.commands.check().ok();
    }

    pub(super) fn dismiss(&mut self) {
        self.confirm_restart = false;
        self.feedback = None;
    }

    pub(super) fn new(
        launch: &Launch,
        runtime: &tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Shell>,
    ) -> Self {
        Self::new_for(launch, runtime, false, window, cx)
    }

    pub(super) fn new_server(
        launch: &Launch,
        runtime: &tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Shell>,
    ) -> Self {
        Self::new_for(launch, runtime, true, window, cx)
    }

    fn new_for(
        launch: &Launch,
        runtime: &tokio::runtime::Handle,
        server: bool,
        window: &mut Window,
        cx: &mut Context<Shell>,
    ) -> Self {
        let (commands, states) = if server {
            crate::updates::start_server(launch, runtime)
        } else {
            crate::updates::start(launch, runtime)
        };

        let target = if server {
            Target::LocalServer
        } else {
            Target::App
        };
        Self::from_states(commands, states, target, None, window, cx)
    }

    pub(super) fn new_remote(
        id: Uuid,
        profile: Profile,
        runtime: &tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Shell>,
    ) -> Result<Self, String> {
        let client = Arc::new(Client::new(
            profile.origin.clone(),
            &profile.token,
            runtime.clone(),
        )?);
        let (commands, states) = crate::updates::start_remote(client, runtime);
        Ok(Self::from_states(
            commands,
            states,
            Target::RemoteServer(id),
            Some(profile),
            window,
            cx,
        ))
    }

    fn from_states(
        commands: Commands,
        mut states: tokio::sync::watch::Receiver<Snapshot>,
        target: Target,
        remote_profile: Option<Profile>,
        window: &mut Window,
        cx: &mut Context<Shell>,
    ) -> Self {
        let snapshot = states.borrow().clone();
        Self {
            remote_profile,
            commands,
            snapshot,
            logs: cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 100_000)),
            scroll: ScrollHandle::new(),
            page_scroll: ScrollHandle::new(),
            output_open: None,
            confirm_restart: false,
            feedback: None,
            _watch: cx.spawn_in(window, async move |this, cx| {
                while states.changed().await.is_ok() {
                    let snapshot = states.borrow_and_update().clone();
                    if this
                        .update_in(cx, |shell, window, cx| {
                            shell.update_snapshot(snapshot, target, window, cx)
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }),
        }
    }

    fn at_bottom(&self) -> bool {
        (self.scroll.offset().y + self.scroll.max_offset().y).abs() < px(8.)
    }

    pub(super) fn busy(&self) -> bool {
        matches!(
            self.snapshot.operation,
            Operation::Preparing | Operation::Installing | Operation::Restarting
        )
    }

    /// Output belongs to the attempt in progress or one that went wrong. A
    /// finished success has nothing left to read.
    fn output_relevant(&self) -> bool {
        match self.snapshot.operation {
            Operation::Preparing | Operation::Installing | Operation::Restarting => true,
            Operation::Failed(_) | Operation::Unconfirmed(_) | Operation::PastFailure(_) => {
                !self.snapshot.output.is_empty()
            }
            Operation::Idle
            | Operation::Ready { .. }
            | Operation::RestartScheduled
            | Operation::RestartFailed(_) => false,
        }
    }

    fn output_visible(&self) -> bool {
        self.output_relevant()
            && self
                .output_open
                .unwrap_or(matches!(self.snapshot.operation, Operation::Failed(_)))
    }

    pub(super) fn latest(&self) -> Option<&str> {
        match &self.snapshot.check {
            UpdateCheck::Available { latest } | UpdateCheck::Current { latest } => Some(latest),
            _ => None,
        }
    }

    pub(super) fn can_install(&self) -> bool {
        self.snapshot.install_refusal().is_none()
    }

    pub(super) fn install(&mut self) -> Result<(), String> {
        self.commands.install()
    }
}

impl Shell {
    fn update_snapshot(
        &mut self,
        snapshot: Snapshot,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let server = target != Target::App;
        let view = match target {
            Target::App => &mut self.updates,
            Target::LocalServer => &mut self.server_updates,
            Target::RemoteServer(id) => {
                let Some(view) = self.remote_updates.get_mut(&id) else {
                    return;
                };
                view
            }
        };
        if snapshot.output != view.snapshot.output {
            if view.snapshot.output.is_empty() {
                view.page_scroll.scroll_to_bottom();
            }
            let follow = view.at_bottom() && view.logs.read(cx).selected_range().is_empty();
            let selection = view.logs.read(cx).selected_range();
            let appended = snapshot.output.starts_with(&view.snapshot.output);
            view.logs.update(cx, |logs, cx| {
                logs.set_value(snapshot.output.clone(), window, cx);
                if appended {
                    logs.set_selected_range(selection, cx);
                }
            });
            if follow {
                view.scroll.scroll_to_bottom();
            }
        }
        if !server && matches!(snapshot.operation, Operation::RestartScheduled) {
            cx.quit();
        }
        let changed = server
            && matches!(
                view.snapshot.operation,
                Operation::Installing | Operation::Preparing | Operation::Restarting
            )
            && matches!(
                snapshot.operation,
                Operation::Idle
                    | Operation::Ready { .. }
                    | Operation::Failed(_)
                    | Operation::Unconfirmed(_)
            );
        if std::mem::discriminant(&view.snapshot.operation)
            != std::mem::discriminant(&snapshot.operation)
        {
            view.output_open = None;
        }
        view.snapshot = snapshot;
        if changed {
            self.servers
                .refresh(&self.registry, &self.runtime, window, cx);
        }
        cx.notify();
    }

    pub(super) fn update_action(&mut self, action: Action, cx: &mut Context<Self>) {
        if matches!(action, Action::Install)
            && !self
                .server_update_target
                .is_some_and(|id| self.remote_updates.contains_key(&id))
            && (self.updates.busy() || self.server_updates.busy())
        {
            return;
        }
        let target = self.server_update_target;
        let view = match target.and_then(|id| self.remote_updates.get_mut(&id)) {
            Some(view) => view,
            None if target.is_some() => &mut self.server_updates,
            None => &mut self.updates,
        };
        if matches!(action, Action::Install) && !view.can_install() {
            return;
        }
        let outcome = match action {
            Action::Check => view.commands.check(),
            Action::Install => view.install(),
            Action::Restart => {
                view.confirm_restart = true;
                Ok(())
            }
            Action::ConfirmRestart => {
                view.confirm_restart = false;
                view.commands.restart()
            }
            Action::CancelRestart => {
                view.confirm_restart = false;
                Ok(())
            }
            Action::ToggleOutput => {
                view.output_open = Some(!view.output_visible());
                Ok(())
            }
            Action::Copy => {
                cx.write_to_clipboard(ClipboardItem::new_string(view.snapshot.output.clone()));
                Ok(())
            }
            Action::Follow => {
                view.scroll.scroll_to_bottom();
                Ok(())
            }
        };
        view.feedback = outcome.err();
        cx.notify();
    }

    fn update_view(&self) -> &View {
        match self
            .server_update_target
            .and_then(|id| self.remote_updates.get(&id))
        {
            Some(view) => view,
            None if self.server_update_target.is_some() => &self.server_updates,
            None => &self.updates,
        }
    }

    pub(super) fn render_updates(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let server = self.server_update_target;
        let view = self.update_view();
        let snapshot = &view.snapshot;
        let output_visible = view.output_visible();
        let name = server
            .and_then(|id| self.registry.find(id))
            .map_or_else(|| "The server".to_owned(), |saved| saved.name.clone());
        let installed = match server {
            Some(id) => self
                .servers
                .entries
                .get(&id)
                .and_then(|entry| entry.version.clone()),
            None => Some(format!("v{}", env!("ELSE_VERSION"))),
        };
        let latest = view.latest().unwrap_or("the update");
        let (target, source) = match snapshot.channel {
            UpdateChannel::Stable => ("latest stable release", "Stable releases"),
            UpdateChannel::Dev => ("newest commit of main", "Dev builds of main"),
        };
        let (title, description, icon, tone) = match &snapshot.operation {
            Operation::Preparing => (
                "Checking for running work".to_owned(),
                "The update starts only when nothing is running.".to_owned(),
                IconName::ShieldCheck,
                theme.muted_foreground,
            ),
            Operation::Installing => (
                format!("Installing {latest}"),
                if server.is_some() {
                    format!(
                        "{name} restarts when the install finishes. The app tries to reconnect on its own."
                    )
                } else {
                    "Keep the app open while it installs.".to_owned()
                },
                IconName::RefreshCw,
                theme.foreground,
            ),
            Operation::Ready { version } if server.is_some() => (
                format!("Updated to {version}"),
                format!("{name} restarted and is running {version}."),
                IconName::Check,
                theme.success,
            ),
            Operation::Ready { version } => (
                "Update installed".to_owned(),
                format!("{version} is installed. Restart the app to use it."),
                IconName::Check,
                theme.success,
            ),
            Operation::Failed(error) => (
                "Update failed".to_owned(),
                error.clone(),
                IconName::TriangleAlert,
                theme.danger,
            ),
            Operation::Unconfirmed(why) => (
                "Couldn't confirm the update".to_owned(),
                format!("{why} Check its status before trying again."),
                IconName::TriangleAlert,
                theme.warning,
            ),
            Operation::Restarting | Operation::RestartScheduled if server.is_some() => (
                format!("Waiting for {name} to reconnect"),
                "It restarts after installing. The app tries to reconnect on its own.".to_owned(),
                IconName::RotateCw,
                theme.foreground,
            ),
            Operation::Restarting | Operation::RestartScheduled => (
                "Restarting else.bot".to_owned(),
                "Reopening the installed app.".to_owned(),
                IconName::RotateCw,
                theme.foreground,
            ),
            Operation::RestartFailed(error) => (
                "Couldn't restart the app".to_owned(),
                format!("{error} Quit and reopen else.bot to use the update."),
                IconName::TriangleAlert,
                theme.danger,
            ),
            Operation::Idle | Operation::PastFailure(_) => match &snapshot.check {
                UpdateCheck::Checking => (
                    "Checking for updates".to_owned(),
                    format!("Looking for the {target}."),
                    IconName::RefreshCw,
                    theme.muted_foreground,
                ),
                UpdateCheck::Current { .. } => (
                    "You're up to date".to_owned(),
                    format!("You're running the {target}."),
                    IconName::Check,
                    theme.success,
                ),
                UpdateCheck::Available { latest } => (
                    format!("{latest} is available"),
                    if server.is_some() {
                        format!("Updating restarts {name}. The app tries to reconnect on its own.")
                    } else {
                        "Your servers keep running while the app updates.".to_owned()
                    },
                    IconName::Download,
                    theme.foreground,
                ),
                UpdateCheck::Failed(_) if snapshot.unavailable.is_some() => (
                    "Updates managed outside this app".to_owned(),
                    snapshot.unavailable.clone().unwrap_or_else(|| {
                        "Automatic installation is unavailable for this session.".to_owned()
                    }),
                    IconName::Laptop,
                    theme.muted_foreground,
                ),
                UpdateCheck::Failed(error) => (
                    "Couldn't check for updates".to_owned(),
                    error.clone(),
                    IconName::TriangleAlert,
                    theme.warning,
                ),
            },
        };
        let loading = view.busy() || matches!(snapshot.check, UpdateCheck::Checking);
        let versions = match (&installed, &snapshot.check) {
            (Some(installed), UpdateCheck::Available { latest })
                if !matches!(snapshot.operation, Operation::Ready { .. }) =>
            {
                Some(format!("{installed} → {latest}"))
            }
            (Some(installed), _) => Some(format!("Running {installed}")),
            (None, _) => None,
        };
        let card = h_flex()
            .w_full()
            .p_4()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .items_start()
            .gap_3()
            .child(
                div()
                    .size_10()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme.radius)
                    .bg(theme.secondary.opacity(0.45))
                    .child(if loading {
                        checking_icon("update-status-loading", px(20.), tone)
                    } else {
                        Icon::new(icon).size_5().text_color(tone).into_any_element()
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(15.))
                            .line_height(px(20.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .max_w(crate::panel_kit::PARAGRAPH)
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(description),
                    )
                    .when_some(versions, |card, versions| {
                        card.child(
                            div()
                                .pt_1()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(versions),
                        )
                    })
                    .child(div().pt_3().child(self.render_update_actions(cx))),
            );
        let mut content = v_flex()
            .gap_6()
            .when(output_visible, |content| content.gap_3())
            .when(!output_visible, |content| {
                content.child(super::settings::section_heading(
                    if server.is_some() {
                        "Server updates"
                    } else {
                        "App updates"
                    },
                    if server.is_some() {
                        "Update this server and reconnect when it restarts."
                    } else {
                        "The desktop app on this computer. Your servers keep running."
                    },
                    cx,
                ))
            })
            .child(card);
        if let Some((label, detail, icon, color)) = self.update_blocker(view, theme) {
            content = content.child(update_notice(label, detail, icon, color, theme));
        }
        if output_visible {
            content = content.child(self.render_update_output(theme, cx));
        }
        let mut panel = v_flex()
            .w_full()
            .when(output_visible, |panel| panel.h_full())
            .min_h_0()
            .gap_4()
            .child(
                div()
                    .id("updates-status-scroll")
                    .when(output_visible, |area| area.flex_1())
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .track_scroll(&view.page_scroll)
                    .vertical_scrollbar(&view.page_scroll)
                    .child(
                        content
                            .w_full()
                            .when(output_visible, |content| content.flex_1())
                            .when(!output_visible, |content| content.flex_none()),
                    ),
            );
        if view.confirm_restart {
            panel = panel.child(
                v_flex()
                    .gap_3()
                    .p_4()
                    .rounded(theme.radius)
                    .bg(theme.secondary)
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Restart else.bot?"))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        "The app will close and reopen with the installed update. Your conversations stay saved. Bot work continues on the server.",
                    ))
                    .child(h_flex().justify_end().gap_2()
                        .child(self.update_button("update-restart-later", "Later", Action::CancelRestart, false, false, cx))
                        .child(self.update_button("update-restart-confirm", "Restart app", Action::ConfirmRestart, true, view.busy(), cx))),
            );
        }
        if let Some(error) = &view.feedback {
            panel = panel.child(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        // A copy whose updates are managed elsewhere has nothing to check
        // or schedule, so the footer would only describe a disabled check.
        let checks_apply = snapshot.unavailable.is_none() || view.remote_profile.is_some();
        let checked = snapshot.checked_at.map(|at| {
            let at: chrono::DateTime<chrono::Local> = at.into();
            format!("Checked {}", at.format("%H:%M"))
        });
        let idle = !view.busy() && !matches!(snapshot.operation, Operation::RestartScheduled);
        panel
            .when(checks_apply, |panel| {
                panel.child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .pt_3()
                        .border_t_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(
                                    [
                                        Some(source.to_owned()),
                                        checked,
                                        Some("Checks on launch and every 4 hours".to_owned()),
                                    ]
                                    .into_iter()
                                    .flatten()
                                    .collect::<Vec<_>>()
                                    .join(" · "),
                                ),
                        )
                        .when(idle, |row| {
                            row.child(
                                Button::new("updates-check")
                                    .control(
                                        Tier::Ghost,
                                        Scale::Compact,
                                        matches!(snapshot.check, UpdateCheck::Checking),
                                        cx,
                                    )
                                    .label("Check again")
                                    .flex_none()
                                    .on_click(cx.listener(|shell, _, _, cx| {
                                        shell.update_action(Action::Check, cx)
                                    })),
                            )
                        }),
                )
            })
            .into_any_element()
    }

    /// Why an available update cannot install right now, when the reason is
    /// not already the card's headline.
    fn update_blocker(
        &self,
        view: &View,
        theme: &Theme,
    ) -> Option<(&'static str, String, IconName, Hsla)> {
        let snapshot = &view.snapshot;
        let waiting = self.server_update_target.is_some()
            && snapshot.unavailable.is_none()
            && matches!(snapshot.check, UpdateCheck::Available { .. })
            && matches!(
                snapshot.operation,
                Operation::Idle | Operation::Failed(_) | Operation::PastFailure(_)
            );
        if let Operation::PastFailure(error) = &snapshot.operation
            && matches!(snapshot.check, UpdateCheck::Available { .. })
            && matches!(snapshot.work, Work::Idle)
        {
            return Some((
                "Previous attempt failed",
                error.clone(),
                IconName::Clock,
                theme.muted_foreground,
            ));
        }
        if !waiting {
            return None;
        }
        match &snapshot.work {
            Work::Idle => None,
            Work::Checking => Some((
                "Checking for running work",
                "The update can start once this check finishes.".to_owned(),
                IconName::Clock,
                theme.muted_foreground,
            )),
            Work::Busy { turns, commands } => Some((
                "Can't update while bots are working",
                work_summary(*turns, *commands),
                IconName::Clock,
                theme.warning,
            )),
            Work::Unavailable(error) => Some((
                "Couldn't check for running work",
                format!("{error} The update stays unavailable until this check succeeds."),
                IconName::TriangleAlert,
                theme.warning,
            )),
        }
    }

    fn render_update_actions(&self, cx: &Context<Self>) -> AnyElement {
        let view = self.update_view();
        let server = self.server_update_target.is_some();
        let snapshot = &view.snapshot;
        let primary = match &snapshot.operation {
            Operation::Preparing
            | Operation::Installing
            | Operation::Restarting
            | Operation::RestartScheduled => None,
            Operation::Ready { .. } if server => None,
            Operation::Ready { .. } => Some(("Restart app".to_owned(), Action::Restart, false)),
            Operation::RestartFailed(_) => {
                Some(("Retry restart".to_owned(), Action::Restart, false))
            }
            Operation::Unconfirmed(_) => Some(("Check status".to_owned(), Action::Check, false)),
            Operation::Failed(_) => {
                Some(("Try again".to_owned(), Action::Install, !view.can_install()))
            }
            Operation::Idle | Operation::PastFailure(_) => view
                .latest()
                .filter(|_| matches!(snapshot.check, UpdateCheck::Available { .. }))
                .map(|latest| {
                    (
                        format!("Update to {latest}"),
                        Action::Install,
                        !view.can_install(),
                    )
                }),
        };
        let toggle = view.output_relevant();
        if primary.is_none() && !toggle {
            return div().into_any_element();
        }
        h_flex()
            .flex_wrap()
            .gap_2()
            .when_some(primary, |row, (label, action, disabled)| {
                row.child(self.update_button("updates-install", label, action, true, disabled, cx))
            })
            .when(toggle, |row| {
                row.child(self.update_button(
                    "updates-output-toggle",
                    if view.output_visible() {
                        "Hide output"
                    } else {
                        "Show output"
                    },
                    Action::ToggleOutput,
                    false,
                    false,
                    cx,
                ))
            })
            .into_any_element()
    }

    fn render_update_output(&self, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let view = self.update_view();
        let status = match view.snapshot.operation {
            Operation::Installing | Operation::Preparing => "Live",
            Operation::Ready { .. } => "Complete",
            Operation::Failed(_) => "Failed",
            _ => "Output",
        };
        v_flex()
            .flex_1()
            .min_h(px(144.))
            .rounded(theme.radius)
            .overflow_hidden()
            .bg(theme.secondary.opacity(0.45))
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Terminal)
                            .size_3()
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .font_family(theme.mono_font_family.clone())
                            .text_xs()
                            .child("elsebot update"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(status),
                    ),
            )
            .child(
                div()
                    .id("updates-output-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&view.scroll)
                    .vertical_scrollbar(&view.scroll)
                    .on_scroll(cx.listener(|_, _, _, cx| cx.notify()))
                    .child(
                        div()
                            .p_3()
                            .font_family(theme.mono_font_family.clone())
                            .text_xs()
                            .when(view.snapshot.output.is_empty(), |area| {
                                area.child(
                                    div()
                                        .text_color(theme.muted_foreground)
                                        .child("Output will appear here when the update starts."),
                                )
                            })
                            .when(!view.snapshot.output.is_empty(), |area| {
                                area.child(
                                    Textarea::new(&view.logs)
                                        .readonly(true)
                                        .appearance(false)
                                        .bordered(false)
                                        .aria_label("Update command output")
                                        .accessibility_id("updates-output")
                                        .font_family(theme.mono_font_family.clone())
                                        .text_xs(),
                                )
                            }),
                    ),
            )
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(if view.at_bottom() {
                                "Following output"
                            } else {
                                "Scroll paused"
                            }),
                    )
                    .when(!view.at_bottom(), |row| {
                        row.child(self.update_button(
                            "updates-output-follow",
                            "Jump to latest",
                            Action::Follow,
                            false,
                            false,
                            cx,
                        ))
                    })
                    .child(
                        Button::new("updates-output-copy")
                            .icon(IconName::Copy)
                            .ghost()
                            .small()
                            .focus_ring(false)
                            .when(!view.snapshot.output.is_empty(), |button| {
                                button.cursor_pointer()
                            })
                            .disabled(view.snapshot.output.is_empty())
                            .accessibility_label("Copy update output")
                            .on_click(
                                cx.listener(|shell, _, _, cx| {
                                    shell.update_action(Action::Copy, cx)
                                }),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn update_button(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        action: Action,
        primary: bool,
        disabled: bool,
        cx: &Context<Self>,
    ) -> Button {
        Button::new(id)
            .label(label)
            .control(
                if primary {
                    Tier::Primary
                } else {
                    Tier::Secondary
                },
                Scale::Standard,
                disabled,
                cx,
            )
            .on_click(cx.listener(move |shell, _, _, cx| {
                shell.update_action(action, cx);
            }))
    }
}

fn update_notice(
    label: &'static str,
    detail: String,
    icon: IconName,
    color: Hsla,
    theme: &Theme,
) -> Div {
    h_flex()
        .items_start()
        .gap_2()
        .p_3()
        .rounded(theme.radius)
        .bg(color.opacity(0.08))
        .child(Icon::new(icon).size_4().mt_0p5().text_color(color))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(label))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(detail),
                ),
        )
}

/// Running work in plain counts, for a disabled update's explanation.
pub(super) fn work_summary(turns: u64, commands: u64) -> String {
    let count = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    format!(
        "{} and {} on this server. Update once they finish.",
        count(turns, "active turn", "active turns"),
        count(commands, "running command", "running commands")
    )
}

pub(super) fn checking_icon(id: impl Into<ElementId>, size: Pixels, color: Hsla) -> AnyElement {
    let icon = Icon::new(IconName::LoaderCircle)
        .size(size)
        .text_color(color);
    icon.with_animation(
        id,
        Animation::new(std::time::Duration::from_millis(900))
            .repeat()
            .with_max_fps(60.),
        |icon, delta| icon.transform(Transformation::rotate(percentage(delta))),
    )
    .into_any_element()
}
