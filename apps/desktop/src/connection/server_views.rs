use super::servers::{Editor, Health, ReleaseStatus};
use super::*;
use crate::app::Link;
use crate::controls::{Control as _, Scale, Tier};
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

    /// The current server's name and connection dot in the sidebar header.
    /// The server rail switches servers, so this stays as quiet as
    /// secondary text.
    pub(super) fn render_server_label(&self, link: Link, cx: &mut Context<Self>) -> AnyElement {
        let Some(active) = &self.active else {
            return div().into_any_element();
        };
        let name = self
            .registry
            .find(active.id)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| active.profile.origin.as_str().to_owned());
        let theme = cx.theme();
        let color = match link {
            Link::Connected => self.server_status(active.id, cx).1,
            Link::Failed => theme.danger,
            Link::Connecting => theme.warning,
        };
        h_flex()
            .min_w_0()
            .px_2()
            .gap(px(6.))
            .items_center()
            .text_size(px(13.))
            .text_color(theme.muted_foreground)
            .child(div().flex_none().size(px(6.)).rounded_full().bg(color))
            .child(div().min_w_0().truncate().child(name))
            .into_any_element()
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
            let online = observation.is_some_and(|entry| entry.health == Health::Online);
            let release = self.server_release(id);
            let update_available = release == ReleaseStatus::Available;
            let installed = observation
                .and_then(|entry| entry.version.as_deref())
                .unwrap_or("Version unavailable");
            let shown_version = if online {
                installed.to_owned()
            } else if let Some(seen) = observation.and_then(|entry| entry.seen) {
                format!(
                    "{installed} · last seen {}",
                    seen.with_timezone(&chrono::Local).format("%b %d, %H:%M")
                )
            } else {
                installed.to_owned()
            };
            // Health reads beside the name; the meta line below carries only
            // the version, release and work facts.
            let health = h_flex()
                .flex_none()
                .gap_1p5()
                .items_center()
                .text_xs()
                .text_color(color)
                .child(
                    if observation.is_none_or(|entry| entry.health == Health::Checking) {
                        updates::checking_icon(
                            SharedString::from(format!("server-health-{id}")),
                            px(12.),
                            color,
                        )
                    } else {
                        div().size_1p5().rounded_full().bg(color).into_any_element()
                    },
                )
                .child(status);
            let mut facts = h_flex().gap_x_2().flex_wrap().text_xs().child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(shown_version),
            );
            if online {
                facts = facts.child(dot_separator(&theme)).child(
                    h_flex()
                        .gap_1p5()
                        .when(release == ReleaseStatus::Checking, |row| {
                            row.child(updates::checking_icon(
                                SharedString::from(format!("server-release-{id}")),
                                px(12.),
                                theme.muted_foreground,
                            ))
                        })
                        .text_color(if update_available {
                            theme.warning
                        } else {
                            theme.muted_foreground
                        })
                        .child(release.label()),
                );
            }
            if let Some(working) = observation
                .and_then(|entry| entry.working)
                .filter(|n| *n > 0)
            {
                facts = facts.child(dot_separator(&theme)).child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(if working == 1 {
                            "1 task running".to_owned()
                        } else {
                            format!("{working} tasks running")
                        }),
                );
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
            let update_view = self.server_update_view(id);
            let update_activity = update_view.is_some_and(updates::View::has_activity);
            if let Some(view) = update_view {
                use crate::updates::Operation;
                let progress = match &view.snapshot.operation {
                    Operation::Preparing => {
                        Some(("Checking ongoing work…", theme.muted_foreground))
                    }
                    Operation::Installing => Some(("Updating…", theme.muted_foreground)),
                    Operation::Restarting | Operation::RestartScheduled => {
                        Some(("Restarting…", theme.muted_foreground))
                    }
                    Operation::Ready { .. } => Some(("Update installed", theme.success)),
                    Operation::Failed(_) | Operation::RestartFailed(_) => {
                        Some(("Update failed", theme.danger))
                    }
                    Operation::Idle => None,
                };
                if let Some((label, color)) = progress {
                    facts = facts
                        .child(dot_separator(&theme))
                        .child(div().text_color(color).child(label));
                }
            }
            if (update_available && online)
                || observation.is_some_and(|entry| entry.health == Health::Incompatible)
                || update_activity
            {
                actions = actions.child(
                    Button::new(SharedString::from(format!("update-server-{id}")))
                        .control(Tier::Secondary, Scale::Standard, false, cx)
                        .label(if update_activity {
                            "View update"
                        } else {
                            "Update server"
                        })
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
                                            .child(health),
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

/// The "·" between facts in a server's status line.
fn dot_separator(theme: &gpui_kit::component::Theme) -> Div {
    div()
        .text_color(theme.muted_foreground.opacity(0.6))
        .child("·")
}
