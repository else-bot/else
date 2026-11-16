use super::servers::{Editor, Health, ReleaseStatus};
use super::*;
use crate::controls::{Control as _, Scale, Tier};
use crate::updates::{Operation, Snapshot, Work};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::{Icon, h_flex};

impl Shell {
    pub(super) fn server_status(&self, id: Uuid, cx: &App) -> (&'static str, Hsla) {
        let theme = cx.theme();
        match self.servers.entries.get(&id).map(|entry| &entry.health) {
            Some(Health::Online) => ("Online", theme.success),
            Some(Health::Offline) => ("Offline", theme.muted_foreground),
            Some(Health::Unauthorized) => ("Sign in needed", theme.warning),
            Some(Health::Incompatible) => ("Unsupported API", theme.warning),
            Some(Health::Checking) | None => ("Checking…", theme.muted_foreground),
        }
    }

    pub(super) fn render_server_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let can_update = self
            .registry
            .servers
            .iter()
            .any(|saved| self.can_update_server(saved.id));
        let updating = self.registry.servers.iter().any(|saved| {
            self.server_update_view(saved.id)
                .is_some_and(updates::View::busy)
        });
        let mut actions = vec![
            Button::new("servers-check-all")
                .control(Tier::Secondary, Scale::Compact, false, cx)
                .icon(IconName::RefreshCw)
                .label("Check all")
                .on_click(cx.listener(|shell, _, window, cx| shell.refresh_servers(window, cx)))
                .into_any_element(),
        ];
        if can_update || updating {
            actions.push(
                Button::new("servers-update-all")
                    .control(Tier::Secondary, Scale::Compact, updating, cx)
                    .label(if updating {
                        "Updating…"
                    } else {
                        "Update all"
                    })
                    .loading(updating)
                    .on_click(cx.listener(|shell, _, _, cx| shell.update_all_servers(cx)))
                    .into_any_element(),
            );
        }
        actions.push(
            Button::new("servers-add")
                .control(Tier::Secondary, Scale::Compact, false, cx)
                .icon(IconName::Plus)
                .label("Add server")
                .on_click(cx.listener(|shell, _, window, cx| {
                    shell.open_server_editor(Editor::Add, window, cx)
                }))
                .into_any_element(),
        );
        let mut content = v_flex()
            .gap_0()
            .child(crate::panel_kit::page_heading(
                "Servers",
                "Manage where your bots run.",
                actions,
                cx,
            ))
            .child(
                div()
                    .pt_6()
                    .pb_3()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "Saved connections · {}",
                        self.registry.servers.len()
                    )),
            );
        if let Some(error) = &self.connection_error {
            content = content.child(
                div()
                    .py_3()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        let mut rows = Vec::new();
        for saved in &self.registry.servers {
            let id = saved.id;
            let selected = self.active.as_ref().is_some_and(|active| active.id == id);
            let (status, color) = self.server_status(id, cx);
            let observation = self.servers.entries.get(&id);
            let health_state = observation.map(|entry| entry.health);
            let online = health_state == Some(Health::Online);
            let release = self.server_release(id);
            let installed = observation
                .and_then(|entry| entry.version.as_deref())
                .unwrap_or("Version unavailable");
            let working = observation
                .and_then(|entry| entry.working)
                .filter(|n| *n > 0);
            let view = self.server_update_view(id);
            let snapshot = view.map(|view| &view.snapshot);
            let latest = view
                .and_then(updates::View::latest)
                .unwrap_or("the update")
                .to_owned();
            let can_update = self.can_update_server(id);
            // Health reads beside the name only when it is not the usual
            // Online; the meta line below carries version and update state.
            let health = (!online).then(|| {
                h_flex()
                    .flex_none()
                    .gap_1p5()
                    .items_center()
                    .text_xs()
                    .text_color(color)
                    .child(
                        if health_state.is_none_or(|health| health == Health::Checking) {
                            updates::checking_icon(
                                SharedString::from(format!("server-health-{id}")),
                                px(12.),
                                color,
                            )
                        } else {
                            div().size_1p5().rounded_full().bg(color).into_any_element()
                        },
                    )
                    .child(status)
            });
            // Status text that opens this server's update details.
            let details_link = |key: &str, label: String, color: Hsla| {
                div()
                    .id(SharedString::from(format!("{key}-{id}")))
                    .role(Role::Button)
                    .aria_label(format!("{label} Show update details for {}", saved.name))
                    .cursor_pointer()
                    .text_color(color)
                    .hover(|style| style.text_color(theme.foreground))
                    .child(label)
                    .on_click(cx.listener(move |shell, _, window, cx| {
                        shell.show_server_update(id, window, cx)
                    }))
            };
            let muted = |text: String| div().text_color(theme.muted_foreground).child(text);
            let spinner = |key: &str| {
                updates::checking_icon(
                    SharedString::from(format!("{key}-{id}")),
                    px(12.),
                    theme.muted_foreground,
                )
            };
            let mut facts = h_flex()
                .gap_x_2()
                .gap_y_0p5()
                .flex_wrap()
                .items_center()
                .text_xs();
            let mut working_shown = false;
            let mut primary: Option<(&'static str, bool, ServerAction)> = None;
            match snapshot.map(|snapshot| &snapshot.operation) {
                Some(
                    operation @ (Operation::Preparing
                    | Operation::Installing
                    | Operation::Restarting
                    | Operation::RestartScheduled),
                ) => {
                    let label = match operation {
                        Operation::Preparing => "Checking for running work…".to_owned(),
                        Operation::Installing => format!("Installing {latest}…"),
                        _ => format!("Waiting for {} to reconnect…", saved.name),
                    };
                    facts = facts.child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(spinner("server-update"))
                            .child(details_link("server-phase", label, theme.muted_foreground)),
                    );
                }
                Some(Operation::Unconfirmed(_)) => {
                    facts = facts.child(details_link(
                        "server-unconfirmed",
                        "Couldn't confirm the update ›".to_owned(),
                        theme.warning,
                    ));
                    primary = Some(("Check status", true, ServerAction::Check));
                }
                _ if !online => {
                    facts = facts.child(muted(match observation.and_then(|entry| entry.seen) {
                        Some(seen) => format!(
                            "{installed} · last seen {}",
                            seen.with_timezone(&chrono::Local).format("%b %d, %H:%M")
                        ),
                        None => installed.to_owned(),
                    }));
                }
                Some(Operation::Ready { version }) => {
                    facts = facts
                        .child(muted(version.clone()))
                        .child(dot_separator(&theme))
                        .child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .text_color(theme.success)
                                .child(Icon::new(IconName::Check).size_3())
                                .child("Updated"),
                        );
                }
                Some(Operation::Failed(_)) => {
                    facts = facts
                        .child(muted(installed.to_owned()))
                        .child(dot_separator(&theme))
                        .child(details_link(
                            "server-failed",
                            "Update failed ›".to_owned(),
                            theme.danger,
                        ));
                    primary = Some(("Try again", can_update, ServerAction::Install));
                }
                operation if release == ReleaseStatus::Available => {
                    facts = facts
                        .child(muted(installed.to_owned()))
                        .child(dot_separator(&theme))
                        .child(
                            div()
                                .text_color(theme.warning)
                                .child(format!("{latest} available")),
                        );
                    let reason = match snapshot {
                        _ if can_update => {
                            selected.then(|| "Updating restarts this server".to_owned())
                        }
                        Some(snapshot) if snapshot.unavailable.is_some() => None,
                        Some(Snapshot {
                            work: Work::Busy { .. },
                            ..
                        }) => {
                            working_shown = true;
                            Some(match working {
                                Some(1) => "Can't update while 1 task is running".to_owned(),
                                Some(n) => format!("Can't update while {n} tasks are running"),
                                None => "Can't update while bots are working".to_owned(),
                            })
                        }
                        Some(Snapshot {
                            work: Work::Checking,
                            ..
                        }) => Some("Checking for running work…".to_owned()),
                        Some(Snapshot {
                            work: Work::Unavailable(_),
                            ..
                        }) => Some("Couldn't check for running work".to_owned()),
                        _ => None,
                    };
                    if let Some(reason) = reason {
                        facts = facts.child(dot_separator(&theme)).child(muted(reason));
                    }
                    if snapshot.is_some_and(|snapshot| snapshot.unavailable.is_some()) {
                        facts = facts.child(dot_separator(&theme)).child(details_link(
                            "server-terminal",
                            "Update from a terminal ›".to_owned(),
                            theme.muted_foreground,
                        ));
                    } else {
                        if matches!(operation, Some(Operation::PastFailure(_))) {
                            facts = facts.child(dot_separator(&theme)).child(details_link(
                                "server-past-failure",
                                "Previous attempt failed ›".to_owned(),
                                theme.muted_foreground,
                            ));
                        }
                        primary = Some(("Update", can_update, ServerAction::Install));
                    }
                }
                _ => {
                    facts = facts
                        .child(muted(installed.to_owned()))
                        .child(dot_separator(&theme))
                        .child(
                            h_flex()
                                .gap_1p5()
                                .items_center()
                                .text_color(theme.muted_foreground)
                                .when(release == ReleaseStatus::Checking, |row| {
                                    row.child(spinner("server-release"))
                                })
                                .child(release.label()),
                        );
                }
            }
            if let Some(working) = working.filter(|_| online && !working_shown) {
                facts = facts
                    .child(dot_separator(&theme))
                    .child(muted(if working == 1 {
                        "1 task running".to_owned()
                    } else {
                        format!("{working} tasks running")
                    }));
            }
            let mut actions = h_flex().gap_1().items_center();
            if !selected {
                actions = actions.child(
                    Button::new(SharedString::from(format!("use-server-{id}")))
                        .control(Tier::Secondary, Scale::Standard, false, cx)
                        .label("Use server")
                        .on_click(cx.listener(move |shell, _, window, cx| {
                            shell.select_server(id, window, cx)
                        })),
                );
            }
            if let Some((label, enabled, action)) = primary {
                actions = actions.child(
                    Button::new(SharedString::from(format!("update-server-{id}")))
                        .control(
                            if enabled {
                                Tier::Primary
                            } else {
                                Tier::Secondary
                            },
                            Scale::Standard,
                            !enabled,
                            cx,
                        )
                        .label(label)
                        .accessibility_label(format!("{label}: {}", saved.name))
                        .on_click(cx.listener(move |shell, _, _, cx| {
                            shell.server_update_action(id, action, cx)
                        })),
                );
            } else if health_state == Some(Health::Incompatible) {
                actions = actions.child(
                    Button::new(SharedString::from(format!("update-server-{id}")))
                        .control(Tier::Secondary, Scale::Standard, false, cx)
                        .label("Update server")
                        .on_click(cx.listener(move |shell, _, window, cx| {
                            shell.show_server_update(id, window, cx)
                        })),
                );
            } else if !online {
                actions = actions.child(
                    Button::new(SharedString::from(format!("retry-server-{id}")))
                        .control(Tier::Secondary, Scale::Standard, false, cx)
                        .label("Retry")
                        .on_click(
                            cx.listener(|shell, _, window, cx| shell.refresh_servers(window, cx)),
                        ),
                );
            }
            // Both row icons: 16 px at 60% in 28 px targets. The server in
            // use can't be removed, so it shows no trash at all.
            let row_icon_button = |id: SharedString, icon: IconName, label: String| {
                Button::new(id)
                    .control(Tier::Ghost, Scale::Compact, false, cx)
                    .w(Scale::Compact.height())
                    .px_0()
                    .text_color(theme.foreground.opacity(0.6))
                    .icon(Icon::new(icon).size_4())
                    .accessibility_label(label)
            };
            let connection_actions = h_flex()
                .gap_1()
                .child(
                    row_icon_button(
                        SharedString::from(format!("edit-server-{id}")),
                        IconName::Pencil,
                        format!("Edit {}", saved.name),
                    )
                    .on_click(cx.listener(move |shell, _, window, cx| {
                        shell.open_server_editor(Editor::Edit(id), window, cx)
                    })),
                )
                .when(!selected, |actions| {
                    actions.child(
                        row_icon_button(
                            SharedString::from(format!("remove-server-{id}")),
                            IconName::Trash,
                            format!("Remove connection to {}", saved.name),
                        )
                        .on_click(cx.listener(
                            move |shell, _, window, cx| shell.remove_server(id, window, cx),
                        )),
                    )
                });
            // One grouped-list row: the server, its address and status
            // tokens, with every action at the trailing edge.
            rows.push(
                v_flex()
                    .w_full()
                    .child(
                        crate::panel_kit::list_row()
                            .child(crate::panel_kit::row_icon(
                                Icon::new(if self.managed_server(&saved.profile) {
                                    IconName::Laptop
                                } else {
                                    IconName::Server
                                }),
                                cx,
                            ))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .items_center()
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_sm()
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .child(saved.name.clone()),
                                            )
                                            .when(selected, |row| {
                                                row.child(crate::panel_kit::tag("In use", cx))
                                            })
                                            .children(health),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .truncate()
                                            .child(saved.profile.origin.as_str().to_owned()),
                                    )
                                    .child(facts),
                            )
                            .child(
                                h_flex()
                                    .flex_none()
                                    .gap_2()
                                    .items_center()
                                    .child(actions)
                                    .child(connection_actions),
                            ),
                    )
                    .when_some(
                        observation.and_then(|entry| entry.error.as_ref()),
                        |row, error| {
                            row.child(
                                div()
                                    .px_4()
                                    .pb_3()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(error.clone()),
                            )
                        },
                    )
                    .into_any_element(),
            );
        }
        content
            .child(crate::panel_kit::grouped_list(rows, cx))
            .into_any_element()
    }

    /// The server these settings edit, as one 12 px line under the sheet's
    /// title: its name, health and address.
    pub(super) fn render_server_identity(&self, cx: &App) -> AnyElement {
        let Some(active) = &self.active else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let name = self
            .registry
            .find(active.id)
            .map(|s| s.name.as_str())
            .unwrap_or("Server");
        let (status, color) = self.server_status(active.id, cx);
        h_flex()
            .id("settings-server-context")
            .w_full()
            .min_w_0()
            .gap_1p5()
            .items_center()
            .text_xs()
            .line_height(px(16.))
            .text_color(theme.muted_foreground)
            .aria_label(format!(
                "Editing settings on {name}. {}. {status}",
                active.profile.origin.as_str()
            ))
            .child(div().flex_none().child("Editing settings on"))
            .child(
                div()
                    .flex_none()
                    .max_w(px(220.))
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .child(name.to_owned()),
            )
            .child(div().flex_none().size_1p5().rounded_full().bg(color))
            .child(
                div()
                    .min_w_0()
                    .pl_1()
                    .truncate()
                    .text_color(theme.foreground.opacity(0.6))
                    .child(active.profile.origin.as_str().to_owned()),
            )
            .into_any_element()
    }
}

/// What a server row's update button asks for.
#[derive(Clone, Copy)]
pub(super) enum ServerAction {
    Install,
    Check,
}

impl Shell {
    pub(super) fn server_update_action(
        &mut self,
        id: Uuid,
        action: ServerAction,
        cx: &mut Context<Self>,
    ) {
        match action {
            ServerAction::Install => self.install_server_update(id, cx),
            ServerAction::Check => {
                if let Some(view) = self.server_update_view(id) {
                    view.refresh();
                }
            }
        }
        cx.notify();
    }
}

/// The "·" between facts in a server's status line.
fn dot_separator(theme: &gpui_kit::component::Theme) -> Div {
    div()
        .text_color(theme.muted_foreground.opacity(0.6))
        .child("·")
}
