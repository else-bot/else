use super::*;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogEntrance, DialogTitle};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Icon, Selectable as _, Sizable as _, h_flex};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsSection {
    Server,
    Email,
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
                .w(viewport.width - px(64.))
                .h(viewport.height - px(64.))
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
                .content(move |content, _, cx| {
                    content.size_full().min_h_0().overflow_hidden().children(
                        shell
                            .update(cx, |shell, cx| shell.render_app_settings(cx))
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
            }]
            .focus(window, cx);
            cx.notify();
        }
    }

    fn render_app_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(setup) = &self.setup else {
            return div().into_any_element();
        };
        let Some(section) = self.settings_section() else {
            return div().into_any_element();
        };
        let nav_item = |id, label, icon, target, focus: &FocusHandle| {
            Button::new(id)
                .track_focus(focus)
                .cursor_pointer()
                .label(label)
                .icon(icon)
                .ghost()
                .selected(section == target)
                .toggled(section == target)
                .w_full()
                .justify_start()
                .on_click(cx.listener(move |shell, _, window, cx| {
                    shell.select_settings(target, window, cx)
                }))
        };
        let navigation = v_flex()
            .flex_none()
            .w(px(156.))
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
            ));
        let body = match section {
            SettingsSection::Server => self.render_server_settings(setup, cx),
            SettingsSection::Email => email_settings(cx),
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
                            })
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(div().p_6().w_full().max_w(px(720.)).child(body))
                            .overflow_y_scrollbar(),
                    ),
            )
            .into_any_element()
    }

    fn render_server_settings(&self, setup: &Setup, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut content = v_flex().gap_5().child(section_heading(
            "Server",
            "Choose where your bots run and connect this app to their server.",
            cx,
        ));
        if let Some(active) = &self.active {
            let (status, _) = active.app.read(cx).status_text();
            content = content.child(
                v_flex()
                    .p_4()
                    .gap_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Current server"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .child(active.profile.origin.as_str().to_owned()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(if active.app.read(cx).connected() {
                                theme.success
                            } else {
                                theme.warning
                            })
                            .child(status),
                    ),
            );
        }
        content
            .when(self.launch.local.is_ok(), |content| {
                content.child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("settings-local-server")
                                .cursor_pointer()
                                .label("This machine")
                                .selected(matches!(setup.page, Page::Welcome))
                                .toggled(matches!(setup.page, Page::Welcome))
                                .on_click(cx.listener(|shell, _, window, cx| {
                                    shell.back_to_welcome(window, cx)
                                })),
                        )
                        .child(
                            Button::new("settings-remote-server")
                                .cursor_pointer()
                                .label("Remote server")
                                .selected(matches!(setup.page, Page::Remote(_)))
                                .toggled(matches!(setup.page, Page::Remote(_)))
                                .on_click(cx.listener(|shell, _, window, cx| {
                                    let profile =
                                        shell.active.as_ref().map(|active| active.profile.clone());
                                    shell.open_remote(profile.as_ref(), window, cx);
                                })),
                        ),
                )
            })
            .child(match &setup.page {
                Page::Welcome => self.render_welcome(setup, &theme, cx),
                Page::Remote(form) => self.render_remote(setup, form, &theme, cx),
            })
            .into_any_element()
    }
}

fn section_heading(title: &'static str, description: &'static str, cx: &App) -> Div {
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
