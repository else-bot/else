use super::view::{missing_local, notice};
use super::*;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogEntrance, DialogTitle};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    Disableable as _, FocusableExt as _, Icon, Selectable as _, Sizable as _, Theme, h_flex,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsSection {
    Server,
    Email,
    Updates,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Presentation {
    Onboarding,
    Settings(SettingsSection),
}

impl Shell {
    pub(super) fn settings_section(&self) -> Option<SettingsSection> {
        match self.setup.as_ref()?.presentation {
            Presentation::Onboarding => None,
            Presentation::Settings(section) => Some(section),
        }
    }

    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        if self.settings_section().is_some() {
            self.cancel(window, cx);
        }
        let Some(active) = &self.active else { return };
        let profile = active.profile.clone();
        let local = self
            .launch
            .local
            .as_ref()
            .is_ok_and(|local| local.origin() == &profile.origin);
        active
            .app
            .update(cx, |app, cx| app.set_hidden(true, window, cx));
        self.open_setup(None, window, cx);
        if !local {
            self.open_remote(Some(&profile), window, cx);
        }
        self.setup.as_mut().unwrap().presentation = Presentation::Settings(SettingsSection::Server);
        self.settings_focus.focus(window, cx);
        let shell = cx.weak_entity();
        let shell_close = shell.clone();
        let first = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false)
                && let Some(shell) = shell.upgrade()
                && let Some(setup) = &shell.read(cx).setup
            {
                let focus = setup.navigation_focus[0].clone();
                focus.focus(window, cx);
            }
            let shell = shell.clone();
            let shell_close = shell_close.clone();
            let viewport = window.viewport_size();
            dialog
                .w((viewport.width - px(64.)).min(px(1040.)))
                .h((viewport.height - px(64.)).min(px(560.)))
                .centered()
                .entrance(DialogEntrance::None)
                .p_0()
                .gap_0()
                .close_button(false)
                .overlay_closable(false)
                .on_ok(|_, _, _| false)
                .on_close(move |_, window, cx| {
                    let _ = shell_close.update(cx, |shell, cx| shell.cancel(window, cx));
                })
                .content(move |content, window, cx| {
                    content.size_full().min_h_0().overflow_hidden().children(
                        shell
                            .update(cx, |shell, cx| shell.render_app_settings(window, cx))
                            .ok(),
                    )
                })
        });
        cx.notify();
    }

    fn select_settings(
        &mut self,
        section: SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(setup) = &mut self.setup {
            setup.presentation = Presentation::Settings(section);
            setup.navigation_focus[match section {
                SettingsSection::Server => 0,
                SettingsSection::Email => 1,
                SettingsSection::Updates => 2,
            }]
            .focus(window, cx);
            cx.notify();
        }
        if section == SettingsSection::Updates {
            self.update_action(super::updates::Action::RefreshWork, cx);
        }
    }

    fn render_app_settings(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(setup) = &self.setup else {
            return div().into_any_element();
        };
        let Some(section) = self.settings_section() else {
            return div().into_any_element();
        };
        let nav_item = |id, label, icon, target, focus: &FocusHandle| {
            Button::new(id)
                .focus_ring(false)
                .track_focus(focus)
                .cursor_pointer()
                .accessibility_label(label)
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(Icon::new(icon).size_4())
                        .child(label),
                )
                .ghost()
                .selected(section == target)
                .toggled(section == target)
                .w_full()
                .on_click(cx.listener(move |shell, _, window, cx| {
                    shell.select_settings(target, window, cx)
                }))
        };
        let navigation = v_flex()
            .flex_none()
            .w(px(170.))
            .h_full()
            .p_3()
            .gap_1()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.secondary.opacity(0.45))
            .child(
                div()
                    .px_2()
                    .pt_1()
                    .pb_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("WORKSPACE"),
            )
            .child(nav_item(
                "settings-server",
                "Server",
                IconName::Server,
                SettingsSection::Server,
                &setup.navigation_focus[0],
            ))
            .child(nav_item(
                "settings-email",
                "Email",
                IconName::Mail,
                SettingsSection::Email,
                &setup.navigation_focus[1],
            ))
            .child(nav_item(
                "settings-updates",
                "Updates",
                IconName::RefreshCw,
                SettingsSection::Updates,
                &setup.navigation_focus[2],
            ));
        let body = match section {
            SettingsSection::Server => self.render_server_settings(setup, window, cx),
            SettingsSection::Email => email_settings(cx),
            SettingsSection::Updates => self.render_updates(cx),
        };
        let body = if section == SettingsSection::Updates {
            div().size_full().p_6().child(body).into_any_element()
        } else {
            div()
                .h_full()
                .child(div().p_8().w_full().child(body))
                .overflow_y_scrollbar()
                .into_any_element()
        };
        v_flex()
            .id("app-settings")
            .track_focus(&setup.focus)
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .child(
                h_flex()
                    .flex_none()
                    .h(px(64.))
                    .px_5()
                    .gap_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Icon::new(IconName::Settings)
                            .size_4()
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        DialogTitle::new()
                            .flex_1()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Settings"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Esc"),
                    )
                    .child(
                        Button::new("app-settings-close")
                            .focus_ring(false)
                            .cursor_pointer()
                            .icon(IconName::Close)
                            .ghost()
                            .small()
                            .accessibility_label("Close settings")
                            .tooltip("Close settings")
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.cancel(window, cx);
                                window.close_dialog(cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(navigation)
                    .child(
                        div()
                            .id(match section {
                                SettingsSection::Server => "settings-server-content",
                                SettingsSection::Email => "settings-email-content",
                                SettingsSection::Updates => "settings-updates-content",
                            })
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(body),
                    ),
            )
            .into_any_element()
    }

    fn render_server_settings(
        &self,
        setup: &Setup,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let stacked = window.viewport_size().width < px(800.);
        let busy = setup.draft.busy();
        let origin = match &setup.page {
            Page::Welcome => self
                .launch
                .local
                .as_ref()
                .ok()
                .map(|local| local.origin().clone()),
            Page::Remote(form) => Origin::parse(&form.origin.read(cx).value()).ok(),
        };
        let active = self.active.as_ref().filter(|active| {
            let local = self
                .launch
                .local
                .as_ref()
                .is_ok_and(|local| local.origin() == &active.profile.origin);
            local == matches!(setup.page, Page::Welcome)
                && Some(&active.profile.origin) == origin.as_ref()
        });
        let connected = active.is_some_and(|active| active.app.read(cx).connected());
        let status = active
            .map(|active| active.app.read(cx).status_text().0)
            .unwrap_or_else(|| "Not connected".into());
        let status_color = if connected {
            theme.success
        } else if active.is_some() {
            theme.warning
        } else {
            theme.muted_foreground
        };
        let status = h_flex()
            .gap_1p5()
            .text_xs()
            .text_color(status_color)
            .child(div().size_1p5().rounded_full().bg(status_color))
            .child(status);
        let location = h_flex()
            .p_1()
            .gap_1()
            .rounded(theme.radius)
            .bg(theme.secondary.opacity(0.45))
            .when(self.launch.local.is_ok(), |row| {
                row.child(
                    Button::new("settings-local-server")
                        .focus_ring(false)
                        .cursor_pointer()
                        .label("This machine")
                        .ghost()
                        .small()
                        .flex_1()
                        .min_w_0()
                        .selected(matches!(setup.page, Page::Welcome))
                        .toggled(matches!(setup.page, Page::Welcome))
                        .on_click(cx.listener(|shell, _, window, cx| {
                            if shell
                                .setup
                                .as_ref()
                                .is_some_and(|setup| !matches!(setup.page, Page::Welcome))
                            {
                                shell.back_to_welcome(window, cx);
                            }
                        })),
                )
            })
            .child(
                Button::new("settings-remote-server")
                    .focus_ring(false)
                    .cursor_pointer()
                    .label("Remote server")
                    .ghost()
                    .small()
                    .flex_1()
                    .min_w_0()
                    .selected(matches!(setup.page, Page::Remote(_)))
                    .toggled(matches!(setup.page, Page::Remote(_)))
                    .on_click(cx.listener(|shell, _, window, cx| {
                        if shell
                            .setup
                            .as_ref()
                            .is_some_and(|setup| !matches!(setup.page, Page::Remote(_)))
                        {
                            let profile =
                                shell.active.as_ref().map(|active| active.profile.clone());
                            shell.open_remote(profile.as_ref(), window, cx);
                        }
                    })),
            );
        let mut content = v_flex()
            .child(section_heading(
                "Server",
                "Connection settings for your workspace.",
                cx,
            ))
            .child(div().h_6())
            .child(settings_row(
                "Run bots on",
                "Choose a location",
                location,
                stacked,
                theme,
            ));
        let failure = match &setup.draft.check {
            Check::Failed(err) => Some(err.as_str()),
            _ => setup.error.as_deref(),
        };
        let action = match &setup.page {
            Page::Welcome => match &self.launch.local {
                Err(why) => {
                    let (title, text) = missing_local(why);
                    content = content.child(div().pt_4().child(notice(title, &text, theme)));
                    None
                }
                Ok(local) => {
                    let key = self.key.as_ref().unwrap_or(&KeyState::Missing);
                    let shown_key = match key {
                        KeyState::Loaded => "••••••••••••",
                        KeyState::Missing => "Not found",
                        KeyState::Unreadable(_) => "Cannot be read",
                    };
                    content = content
                        .child(settings_row(
                            "Server address",
                            "Local connection",
                            v_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_family(theme.mono_font_family.clone())
                                        .child(local.origin().as_str().to_owned()),
                                )
                                .child(status),
                            stacked,
                            theme,
                        ))
                        .child(settings_row(
                            "Owner key",
                            "On this device",
                            h_flex()
                                .gap_3()
                                .child(
                                    div()
                                        .flex_1()
                                        .text_sm()
                                        .font_family(theme.mono_font_family.clone())
                                        .child(shown_key),
                                )
                                .when(*key == KeyState::Loaded, |row| {
                                    row.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("Loaded"),
                                    )
                                }),
                            stacked,
                            theme,
                        ));
                    let incomplete = match key {
                        KeyState::Loaded => None,
                        KeyState::Missing => Some(
                            "No local key was found. Finish installing the local server, then try again. \
                             You can also connect to a remote server.",
                        ),
                        KeyState::Unreadable(err) => Some(err.as_str()),
                    };
                    content = content.when_some(
                        incomplete.filter(|_| failure.is_none()),
                        |content, err| {
                            content.child(notice("Local setup is incomplete", err, theme))
                        },
                    );
                    let label = match busy {
                        Some(Phase::Connecting) => "Connecting…",
                        Some(Phase::Starting) => "Starting local server…",
                        Some(Phase::Waiting) => "Waiting for the local server…",
                        None if failure.is_some() || *key != KeyState::Loaded => "Try local again",
                        None if local.manages_service() => "Start local server",
                        None => "Connect locally",
                    };
                    (!connected || busy.is_some()).then(|| {
                        Button::new("connection-start")
                            .focus_ring(false)
                            .cursor_pointer()
                            .label(label)
                            .primary()
                            .loading(busy.is_some())
                            .disabled(busy.is_some())
                            .on_click(
                                cx.listener(|shell, _, window, cx| shell.start_local(window, cx)),
                            )
                    })
                }
            },
            Page::Remote(form) => {
                content = content
                    .child(settings_row(
                        "Server address",
                        "HTTPS or a local SSH tunnel",
                        v_flex()
                            .gap_2()
                            .child(
                                Input::new(&form.origin)
                                    .focus_ring(false)
                                    .aria_label("Server address"),
                            )
                            .when(active.is_some(), |column| column.child(status)),
                        stacked,
                        theme,
                    ))
                    .child(settings_row(
                        "Owner key",
                        "From your remote server",
                        v_flex()
                            .gap_2()
                            .child(
                                Input::new(&form.token)
                                    .focus_ring(false)
                                    .aria_label("Owner key")
                                    .mask_toggle(),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("On your server, open ~/.else/data/owner-token."),
                            ),
                        stacked,
                        theme,
                    ));
                Some(
                    Button::new("connection-connect")
                        .focus_ring(false)
                        .cursor_pointer()
                        .label(if busy.is_some() {
                            "Connecting…"
                        } else {
                            "Save connection"
                        })
                        .primary()
                        .loading(busy.is_some())
                        .disabled(busy.is_some())
                        .on_click(
                            cx.listener(|shell, _, window, cx| shell.connect_remote(window, cx)),
                        ),
                )
            }
        };
        if let Some(active) = self.active.as_ref().filter(|_| active.is_none()) {
            let (status, _) = active.app.read(cx).status_text();
            content = content.child(
                div()
                    .pt_4()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "Current connection: {} · {status}",
                        active.profile.origin.as_str()
                    )),
            );
        }
        content
            .when_some(failure, |content, err| {
                content.child(div().pt_4().child(notice("Could not connect", err, theme)))
            })
            .child(h_flex().justify_end().gap_2().pt_5().children(action).when(
                matches!(setup.page, Page::Welcome) && connected,
                |row| {
                    row.child(
                        Button::new("app-settings-done")
                            .focus_ring(false)
                            .cursor_pointer()
                            .label("Done")
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.cancel(window, cx);
                                window.close_dialog(cx);
                            })),
                    )
                },
            ))
            .into_any_element()
    }
}

fn settings_row(
    label: &'static str,
    hint: &'static str,
    value: impl IntoElement,
    stacked: bool,
    theme: &Theme,
) -> Div {
    h_flex()
        .items_center()
        .when(stacked, |row| row.flex_col().items_start().gap_3())
        .py_5()
        .border_t_1()
        .border_color(theme.border)
        .child(
            v_flex()
                .when(stacked, |label| label.w_full())
                .when(!stacked, |label| label.flex_none().w(relative(0.43)).pr_6())
                .gap_1()
                .child(div().text_sm().child(label))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(hint),
                ),
        )
        .child(
            div()
                .when(stacked, |value| value.w_full())
                .when(!stacked, |value| value.flex_1())
                .min_w_0()
                .child(value),
        )
}

pub(super) fn section_heading(title: &'static str, description: &'static str, cx: &App) -> Div {
    v_flex()
        .gap_2()
        .child(
            div()
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(description),
        )
}

fn email_settings(cx: &App) -> AnyElement {
    let theme = cx.theme();
    v_flex()
        .gap_6()
        .child(section_heading(
            "Email",
            "Manage the inboxes your bots can work with.",
            cx,
        ))
        .child(
            v_flex()
                .items_center()
                .gap_3()
                .px_5()
                .py_10()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size_12()
                        .mb_1()
                        .rounded(theme.radius_lg)
                        .bg(theme.secondary)
                        .child(
                            Icon::new(IconName::Mail)
                                .size_6()
                                .text_color(theme.muted_foreground),
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child("No email accounts connected"),
                )
                .child(
                    div()
                        .text_sm()
                        .text_center()
                        .text_color(theme.muted_foreground)
                        .child("Your connected inboxes and their bot access will appear here."),
                ),
        )
        .into_any_element()
}
