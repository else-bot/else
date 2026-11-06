use super::servers::{Editor, Health, ReleaseStatus};
use super::*;
use gpui_kit::assets::IconName;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, h_flex};

impl Shell {
    fn server_status(&self, id: Uuid, cx: &App) -> (&'static str, Hsla) {
        let theme = cx.theme();
        match self.servers.entries.get(&id).map(|entry| &entry.health) {
            Some(Health::Online) => ("Online", theme.success),
            Some(Health::Offline) => ("Offline", theme.muted_foreground),
            Some(Health::Unauthorized) => ("Sign in needed", theme.warning),
            Some(Health::Incompatible) => ("Unsupported API", theme.warning),
            Some(Health::Checking) | None => ("Checking…", theme.muted_foreground),
        }
    }

    pub(super) fn render_server_switcher(
        &self,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(active) = &self.active else {
            return div().into_any_element();
        };
        let name = self
            .registry
            .find(active.id)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| active.profile.origin.as_str().to_owned());
        let (_, observed_color) = self.server_status(active.id, cx);
        let color = if active.app.read(cx).connected() {
            observed_color
        } else if active.app.read(cx).status_text().1 {
            cx.theme().danger
        } else {
            cx.theme().warning
        };
        let icon = if self.managed_server(&active.profile) {
            IconName::Laptop
        } else {
            IconName::Server
        };
        let owner = cx.weak_entity();
        let toggle = owner.clone();
        Popover::new(id)
            .anchor(Anchor::TopLeft)
            .open(
                self.server_menu
                    && (id == "settings-server-switcher") == self.settings_section().is_some(),
            )
            .on_open_change(move |open, _, cx| {
                let _ = toggle.update(cx, |shell, cx| {
                    shell.server_menu = *open;
                    cx.notify();
                });
            })
            .trigger(
                Button::new(SharedString::from(format!("{id}-trigger")))
                    .focus_ring(false)
                    .cursor_pointer()
                    .ghost()
                    .xsmall()
                    .loading(self.switch_attempt.is_some())
                    .accessibility_label(format!("Switch server. Current server {name}"))
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_2()
                            .child(Icon::new(icon).size_3())
                            .child(div().max_w(px(170.)).truncate().child(name))
                            .child(div().size_1p5().rounded_full().bg(color))
                            .child(Icon::new(IconName::ChevronDown).size_3()),
                    ),
            )
            .content(move |_, _, cx| {
                owner
                    .update(cx, |shell, cx| shell.render_server_menu(cx))
                    .unwrap_or_else(|_| div().into_any_element())
            })
            .into_any_element()
    }

    fn render_server_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut menu = v_flex()
            .id("server-switcher-list")
            .w(px(282.))
            .max_h(px(460.))
            .overflow_y_scroll()
            .p_1()
            .gap_1()
            .child(
                div()
                    .px_2()
                    .py_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Your servers"),
            );
        for saved in &self.registry.servers {
            let id = saved.id;
            let selected = self.active.as_ref().is_some_and(|active| active.id == id);
            let (status, color) = self.server_status(id, cx);
            let release = self.server_release(id);
            let detail = if release == ReleaseStatus::Available {
                format!("{status} · Update available")
            } else {
                status.to_owned()
            };
            menu = menu.child(
                Button::new(SharedString::from(format!("switch-server-{id}")))
                    .focus_ring(false)
                    .cursor_pointer()
                    .ghost()
                    .small()
                    .w_full()
                    .h(px(48.))
                    .accessibility_label(format!("Switch to {}. {detail}", saved.name))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                Icon::new(if self.managed_server(&saved.profile) {
                                    IconName::Laptop
                                } else {
                                    IconName::Server
                                })
                                .size_4(),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(div().text_sm().truncate().child(saved.name.clone()))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(detail),
                                    ),
                            )
                            .child(if selected {
                                div().child(
                                    Icon::new(IconName::Check)
                                        .size_3()
                                        .text_color(theme.primary),
                                )
                            } else {
                                div().size_1p5().rounded_full().bg(color)
                            }),
                    )
                    .on_click(cx.listener(move |shell, _, window, cx| {
                        shell.server_menu = false;
                        shell.select_server(id, window, cx);
                    })),
            );
        }
        menu.into_any_element()
    }

    pub(super) fn render_server_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut content = v_flex()
            .gap_0()
            .child(
                h_flex()
                    .items_start()
                    .gap_3()
                    .child(div().flex_1().child(settings::section_heading(
                        "Servers",
                        "Manage where your bots run.",
                        cx,
                    )))
                    .child(
                        Button::new("servers-add")
                            .focus_ring(false)
                            .cursor_pointer()
                            .small()
                            .label("+ Add server")
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.open_server_editor(Editor::Add, window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .pt_6()
                    .pb_2()
                    .text_xs()
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
            let mut facts = h_flex()
                .gap_3()
                .flex_wrap()
                .text_xs()
                .child(
                    h_flex()
                        .gap_1p5()
                        .text_color(color)
                        .child(div().size_1p5().rounded_full().bg(color))
                        .child(status),
                )
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(shown_version),
                );
            if online {
                facts = facts.child(
                    div()
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
                facts = facts.child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(format!("{working} tasks running")),
                );
            }
            let mut actions = h_flex().gap_1().flex_wrap().justify_end();
            if !selected {
                actions = actions.child(
                    Button::new(SharedString::from(format!("use-server-{id}")))
                        .focus_ring(false)
                        .cursor_pointer()
                        .ghost()
                        .xsmall()
                        .label("Use server")
                        .on_click(cx.listener(move |shell, _, window, cx| {
                            shell.select_server(id, window, cx)
                        })),
                );
            }
            if (update_available && online)
                || observation.is_some_and(|entry| entry.health == Health::Incompatible)
            {
                let managed = self.managed_server(&saved.profile);
                actions = actions.child(
                    Button::new(SharedString::from(format!("update-server-{id}")))
                        .focus_ring(false)
                        .cursor_pointer()
                        .xsmall()
                        .label(if managed {
                            "Update server"
                        } else {
                            "Update instructions"
                        })
                        .on_click(cx.listener(move |shell, _, window, cx| {
                            shell.show_server_update(id, window, cx)
                        })),
                );
            } else if !online {
                actions = actions.child(
                    Button::new(SharedString::from(format!("retry-server-{id}")))
                        .focus_ring(false)
                        .cursor_pointer()
                        .ghost()
                        .xsmall()
                        .label("Retry")
                        .on_click(
                            cx.listener(|shell, _, window, cx| shell.refresh_servers(window, cx)),
                        ),
                );
            }
            let connection_actions = h_flex()
                .gap_1()
                .child(
                    Button::new(SharedString::from(format!("edit-server-{id}")))
                        .focus_ring(false)
                        .cursor_pointer()
                        .ghost()
                        .xsmall()
                        .icon(IconName::Pencil)
                        .accessibility_label(format!("Edit {}", saved.name))
                        .on_click(cx.listener(move |shell, _, window, cx| {
                            shell.open_server_editor(Editor::Edit(id), window, cx)
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("remove-server-{id}")))
                        .focus_ring(false)
                        .cursor_pointer()
                        .ghost()
                        .xsmall()
                        .icon(IconName::Trash)
                        .accessibility_label(format!("Remove connection to {}", saved.name))
                        .disabled(selected)
                        .on_click(cx.listener(move |shell, _, window, cx| {
                            shell.remove_server(id, window, cx)
                        })),
                );
            content = content.child(
                v_flex()
                    .py_4()
                    .gap_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                div()
                                    .size_8()
                                    .flex_none()
                                    .rounded(theme.radius)
                                    .bg(theme.secondary)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        Icon::new(if self.managed_server(&saved.profile) {
                                            IconName::Laptop
                                        } else {
                                            IconName::Server
                                        })
                                        .size_4()
                                        .text_color(theme.muted_foreground),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .child(saved.name.clone()),
                                            )
                                            .when(selected, |row| {
                                                row.child(
                                                    div()
                                                        .text_xs()
                                                        .px_1p5()
                                                        .rounded_sm()
                                                        .bg(theme.selection)
                                                        .text_color(theme.primary)
                                                        .child("In use"),
                                                )
                                            }),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_family(theme.mono_font_family.clone())
                                            .text_color(theme.muted_foreground)
                                            .truncate()
                                            .child(saved.profile.origin.as_str().to_owned()),
                                    ),
                            )
                            .child(connection_actions),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_3()
                            .flex_wrap()
                            .child(div().flex_1().min_w_0().child(facts))
                            .child(actions),
                    )
                    .when_some(
                        observation.and_then(|entry| entry.error.as_ref()),
                        |row, error| {
                            row.child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(error.clone()),
                            )
                        },
                    ),
            );
        }
        content
            .child(
                h_flex()
                    .justify_between()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Offline versions are from the last successful check."),
                    )
                    .child(
                        Button::new("servers-check-all")
                            .focus_ring(false)
                            .cursor_pointer()
                            .ghost()
                            .xsmall()
                            .icon(IconName::RefreshCw)
                            .label("Check all")
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.refresh_servers(window, cx)
                            })),
                    ),
            )
            .child(
                h_flex()
                    .pt_5()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Server updates live here."),
                    )
                    .child(
                        Button::new("servers-app-updates")
                            .focus_ring(false)
                            .cursor_pointer()
                            .ghost()
                            .xsmall()
                            .label("Desktop app updates")
                            .icon(IconName::ArrowRight)
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.select_settings(
                                    settings::SettingsSection::Updates,
                                    window,
                                    cx,
                                )
                            })),
                    ),
            )
            .into_any_element()
    }

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
        v_flex()
            .id("settings-server-context")
            .w(px(220.))
            .min_w_0()
            .gap_1()
            .px_3()
            .py_2()
            .rounded(theme.radius)
            .bg(theme.secondary.opacity(0.6))
            .aria_label(format!(
                "Editing settings on {name}. {}. {status}",
                active.profile.origin.as_str()
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Editing settings on"),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(if self.managed_server(&active.profile) {
                            IconName::Laptop
                        } else {
                            IconName::Server
                        })
                        .size_3()
                        .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .child(name.to_owned()),
                    )
                    .child(div().size_1p5().rounded_full().bg(color)),
            )
            .child(
                div()
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(active.profile.origin.as_str().to_owned()),
            )
            .into_any_element()
    }
}
