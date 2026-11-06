use super::*;
use crate::updates::{Check as UpdateCheck, Commands, Operation, Snapshot, Work};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Disableable as _, FocusableExt as _, Icon, Sizable as _, Theme, h_flex};

pub(super) struct View {
    pub(super) commands: Commands,
    pub(super) snapshot: Snapshot,
    logs: Entity<TextareaState>,
    scroll: ScrollHandle,
    page_scroll: ScrollHandle,
    show_output: bool,
    confirm_restart: bool,
    feedback: Option<String>,
    _watch: Task<()>,
}

#[derive(Clone, Copy)]
pub(super) enum Action {
    Check,
    RefreshWork,
    Install,
    Restart,
    ConfirmRestart,
    CancelRestart,
    Later,
    ToggleOutput,
    Copy,
    Follow,
}

impl View {
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
        let (commands, mut states) = if server {
            crate::updates::start_server(launch, runtime)
        } else {
            crate::updates::start(launch, runtime)
        };

        let snapshot = states.borrow().clone();
        Self {
            commands,
            snapshot,
            logs: cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 100_000)),
            scroll: ScrollHandle::new(),
            page_scroll: ScrollHandle::new(),
            show_output: false,
            confirm_restart: false,
            feedback: None,
            _watch: cx.spawn_in(window, async move |this, cx| {
                while states.changed().await.is_ok() {
                    let snapshot = states.borrow_and_update().clone();
                    if this
                        .update_in(cx, |shell, window, cx| {
                            shell.update_snapshot(snapshot, server, window, cx)
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

    fn latest(&self) -> Option<&str> {
        match &self.snapshot.check {
            UpdateCheck::Available { latest } | UpdateCheck::Current { latest } => Some(latest),
            _ => None,
        }
    }

    fn can_install(&self) -> bool {
        self.snapshot.unavailable.is_none()
            && matches!(self.snapshot.check, UpdateCheck::Available { .. })
            && matches!(self.snapshot.work, Work::Idle)
            && matches!(
                self.snapshot.operation,
                Operation::Idle | Operation::Failed(_)
            )
    }
}

impl Shell {
    fn update_snapshot(
        &mut self,
        snapshot: Snapshot,
        server: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = if server {
            &mut self.server_updates
        } else {
            &mut self.updates
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
                Operation::Installing | Operation::Preparing
            )
            && matches!(
                snapshot.operation,
                Operation::Idle | Operation::Ready { .. } | Operation::Failed(_)
            );
        view.snapshot = snapshot;
        if changed {
            self.servers
                .refresh(&self.registry, &self.runtime, window, cx);
        }
        cx.notify();
    }

    pub(super) fn update_action(&mut self, action: Action, cx: &mut Context<Self>) {
        if matches!(action, Action::Install) && (self.updates.busy() || self.server_updates.busy())
        {
            return;
        }
        let view = if self.server_update_target.is_some() {
            &mut self.server_updates
        } else {
            &mut self.updates
        };
        if matches!(action, Action::Install) && !view.can_install()
            || matches!(action, Action::RefreshWork) && view.snapshot.unavailable.is_some()
        {
            return;
        }
        let outcome = match action {
            Action::Check => view.commands.check(),
            Action::RefreshWork => view.commands.refresh_work(),
            Action::Install => {
                view.show_output = true;
                view.commands.install()
            }
            Action::Restart => {
                view.confirm_restart = true;
                Ok(())
            }
            Action::ConfirmRestart => {
                view.confirm_restart = false;
                view.commands.restart()
            }
            Action::CancelRestart | Action::Later => {
                view.confirm_restart = false;
                Ok(())
            }
            Action::ToggleOutput => {
                view.show_output = !view.show_output;
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
        if self.server_update_target.is_some() {
            &self.server_updates
        } else {
            &self.updates
        }
    }

    pub(super) fn render_updates(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let server = self.server_update_target.is_some();
        let view = self.update_view();
        let snapshot = &view.snapshot;
        let output_visible = view.show_output || !snapshot.output.is_empty();
        let (title, description, icon, tone) = match &snapshot.operation {
            Operation::Preparing => (
                "Checking for ongoing work",
                "The update starts only when this installation is idle.".to_owned(),
                IconName::ShieldCheck,
                theme.muted_foreground,
            ),
            Operation::Installing => (
                "Updating else.bot",
                "Follow the output below. Keep the app open while updating.".to_owned(),
                IconName::RefreshCw,
                theme.foreground,
            ),
            Operation::Ready { version } => (
                "Update installed",
                if server {
                    format!("{version} is installed. The server has restarted.")
                } else {
                    format!("{version} is installed. Restart the app to start using it.")
                },
                IconName::Check,
                theme.success,
            ),
            Operation::Failed(error) => (
                "The update didn't finish",
                if snapshot.output.is_empty() {
                    error.clone()
                } else {
                    "The command stopped. Review the output below, then try again.".to_owned()
                },
                IconName::TriangleAlert,
                theme.danger,
            ),
            Operation::Restarting | Operation::RestartScheduled => (
                "Restarting else.bot",
                "Reopening the installed app.".to_owned(),
                IconName::RotateCw,
                theme.foreground,
            ),
            Operation::RestartFailed(error) => (
                "Couldn't restart the app",
                error.clone(),
                IconName::TriangleAlert,
                theme.danger,
            ),
            Operation::Idle => match &snapshot.check {
                UpdateCheck::Checking => (
                    "Checking for updates",
                    "Looking for the latest stable git tag.".to_owned(),
                    IconName::RefreshCw,
                    theme.muted_foreground,
                ),
                UpdateCheck::Current { .. } => (
                    "You're up to date",
                    "You're running the latest stable release.".to_owned(),
                    IconName::Check,
                    theme.success,
                ),
                UpdateCheck::Available { .. } => (
                    "A new version is available",
                    "Install the latest stable release when you're ready.".to_owned(),
                    IconName::Download,
                    theme.foreground,
                ),
                UpdateCheck::Failed(_) if snapshot.unavailable.is_some() => (
                    "Updates managed outside this app",
                    "Automatic installation is unavailable for this session.".to_owned(),
                    IconName::Laptop,
                    theme.muted_foreground,
                ),
                UpdateCheck::Failed(error) => (
                    "Couldn't check for updates",
                    error.clone(),
                    IconName::TriangleAlert,
                    theme.warning,
                ),
            },
        };
        let last_check = snapshot.checked_at.map(|at| {
            let at: chrono::DateTime<chrono::Local> = at.into();
            format!("Stable releases · Checked {}", at.format("%H:%M"))
        });
        let mut content = v_flex()
            .gap_5()
            .when(output_visible, |content| content.gap_3())
            .when(!output_visible, |content| {
                content
                    .child(super::settings::section_heading(
                        if server {
                            "Server updates"
                        } else {
                            "App updates"
                        },
                        if server {
                            "Updates for this managed local server."
                        } else {
                            "The desktop app on this computer. Your servers keep running."
                        },
                        cx,
                    ))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        last_check.unwrap_or_else(|| "Stable releases from git tags".into()),
                    ))
            })
            .child(
                h_flex()
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
                            .child(Icon::new(icon).size_5().text_color(tone)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(description),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .gap_3()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if let Some(id) = self.server_update_target {
                        self.registry
                            .find(id)
                            .map(|s| s.name.clone())
                            .unwrap_or_default()
                    } else {
                        format!("Running v{}", env!("CARGO_PKG_VERSION"))
                    })
                    .when_some(view.latest(), |row, version| {
                        row.child(Icon::new(IconName::ArrowRight).size_3())
                            .child(format!("Latest {version}"))
                    }),
            );
        if let Some(reason) = &snapshot.unavailable {
            content = content.child(update_notice(
                "Installation unavailable",
                reason.clone(),
                IconName::Laptop,
                theme.muted_foreground,
                theme,
            ));
        } else if server
            && !output_visible
            && !view.busy()
            && !matches!(
                snapshot.operation,
                Operation::Ready { .. } | Operation::RestartFailed(_)
            )
        {
            let (label, detail, icon, color) = match &snapshot.work {
                Work::Checking => (
                    "Checking ongoing work",
                    "Installation waits until the work check finishes.".to_owned(),
                    IconName::Clock,
                    theme.muted_foreground,
                ),
                Work::Idle => (
                    "No ongoing work",
                    "We'll check again before the update starts.".to_owned(),
                    IconName::ShieldCheck,
                    theme.success,
                ),
                Work::Busy { turns, commands } => (
                    "Work is still running",
                    format!(
                        "{turns} active turns · {commands} running commands. Updates wait until they finish."
                    ),
                    IconName::Clock,
                    theme.warning,
                ),
                Work::Unavailable(error) => (
                    "Couldn't check ongoing work",
                    format!("{error} Installation stays unavailable until we can check."),
                    IconName::TriangleAlert,
                    theme.warning,
                ),
            };
            content = content.child(update_notice(label, detail, icon, color, theme));
        }
        if output_visible {
            content = content.child(self.render_update_output(theme, cx));
        }
        let mut panel = v_flex().size_full().min_h_0().gap_4().child(
            div()
                .id("updates-status-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&view.page_scroll)
                .vertical_scrollbar(&view.page_scroll)
                .child(content.flex_none().w_full()),
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
        } else {
            panel = panel.child(self.render_update_actions(cx));
        }
        if let Some(error) = &view.feedback {
            panel = panel.child(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        panel
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .pt_4()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Icon::new(IconName::RefreshCw)
                            .size_3()
                            .mt_1()
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(div().text_xs().child("Automatic update checks"))
                            .child(div().text_xs().text_color(theme.muted_foreground).child(
                                if snapshot.unavailable.is_some() {
                                    "Available for installed copies of else.bot."
                                } else {
                                    "On launch and every 4 hours. Installation always waits for you."
                                },
                            )),
                    )
                    .child(self.update_button(
                        "updates-check",
                        "Check again",
                        Action::Check,
                        false,
                        view.busy() || snapshot.unavailable.is_some() || matches!(snapshot.check, UpdateCheck::Checking),
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_update_actions(&self, cx: &Context<Self>) -> AnyElement {
        let view = self.update_view();
        let server = self.server_update_target.is_some();
        if server && matches!(view.snapshot.operation, Operation::Ready { .. }) {
            return div().into_any_element();
        }
        let (label, action, disabled) = match &view.snapshot.operation {
            Operation::Preparing => ("Checking work…".to_owned(), Action::Install, true),
            Operation::Installing => ("Updating…".to_owned(), Action::Install, true),
            Operation::Restarting | Operation::RestartScheduled => {
                ("Restarting…".to_owned(), Action::Restart, true)
            }
            Operation::Ready { .. } | Operation::RestartFailed(_) => {
                ("Restart app".to_owned(), Action::Restart, false)
            }
            Operation::Idle | Operation::Failed(_) => {
                let label = if matches!(view.snapshot.work, Work::Busy { .. }) {
                    "Waiting for work to finish".to_owned()
                } else if matches!(view.snapshot.operation, Operation::Failed(_)) {
                    "Try update again".to_owned()
                } else {
                    view.latest().map_or_else(
                        || "Update".to_owned(),
                        |version| format!("Update to {version}"),
                    )
                };
                (label, Action::Install, !view.can_install())
            }
        };
        let no_update = matches!(view.snapshot.operation, Operation::Idle)
            && matches!(
                view.snapshot.check,
                UpdateCheck::Current { .. } | UpdateCheck::Failed(_)
            );
        h_flex()
            .flex_wrap()
            .gap_2()
            .when(!no_update, |row| {
                row.child(self.update_button("updates-install", label, action, true, disabled, cx))
            })
            .when(
                matches!(
                    view.snapshot.operation,
                    Operation::Ready { .. } | Operation::RestartFailed(_)
                ),
                |row| {
                    row.child(self.update_button(
                        "updates-later",
                        "Later",
                        Action::Later,
                        false,
                        false,
                        cx,
                    ))
                },
            )
            .when(view.snapshot.output.is_empty() && !view.busy(), |row| {
                row.child(self.update_button(
                    "updates-output-toggle",
                    if view.show_output {
                        "Hide output"
                    } else {
                        "View output"
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
            Operation::Installing | Operation::Preparing => "LIVE",
            Operation::Ready { .. } => "COMPLETE",
            Operation::Failed(_) => "FAILED",
            _ => "OUTPUT",
        };
        v_flex()
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
                    .h(px(144.))
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
            .small()
            .focus_ring(false)
            .when(primary, |button| button.primary())
            .when(!primary, |button| button.ghost())
            .when(!disabled, |button| button.cursor_pointer())
            .disabled(disabled)
            .on_click(cx.listener(move |shell, _, window, cx| {
                if matches!(action, Action::Later) {
                    shell.cancel(window, cx);
                    window.close_dialog(cx);
                } else {
                    shell.update_action(action, cx);
                }
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
