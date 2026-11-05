use super::*;
use gpui_kit::component::FocusableExt as _;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, Theme, h_flex};

impl Shell {
    pub(super) fn render_bar(
        &self,
        active: &Active,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
                Button::new("app-settings-open")
                    .focus_ring(false)
                    .track_focus(&self.settings_focus)
                    .cursor_pointer()
                    .icon(IconName::Settings)
                    .label("Settings")
                    .children(
                        Kbd::binding_for_action(&OpenAppSettings, Some(SHELL_CONTEXT), window)
                            .map(|kbd| div().flex_none().text_xs().child(kbd.appearance(false))),
                    )
                    .xsmall()
                    .ghost()
                    .on_click(cx.listener(|shell, _, window, cx| shell.open_settings(window, cx))),
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

    pub(super) fn render_setup(
        &self,
        setup: &Setup,
        wide: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let panel = match &setup.page {
            Page::Welcome => self.render_welcome(setup, &theme, cx),
            Page::Remote(form) => self.render_remote(setup, form, &theme, cx),
        };
        h_flex()
            .size_full()
            .items_stretch()
            .when(wide, |row| row.child(story(&theme)))
            .child(
                div()
                    .id("setup-panel")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .min_h_full()
                            .justify_center()
                            .items_center()
                            .px(px(28.))
                            .py(px(30.))
                            .child(panel.w_full().max_w(px(380.))),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn render_welcome(
        &self,
        setup: &Setup,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let busy = setup.draft.busy();
        let failure = match &setup.draft.check {
            Check::Failed(err) => Some(err.clone()),
            _ => setup.error.clone(),
        };
        let here = this_machine();
        let mut panel = v_flex()
            .gap_0()
            .child(heading(format!("Start on {here}"), theme));
        let local = match &self.launch.local {
            Ok(local) => local,
            Err(why) => {
                let (title, text) = missing_local(why);
                return panel
                    .child(intro("Connect to the else.bot server you use.", theme))
                    .when_some(failure, |panel, err| {
                        panel.child(notice("Cannot use the saved server", &err, theme))
                    })
                    .child(notice(title, &text, theme))
                    .child(
                        Button::new("connection-edit")
                            .focus_ring(false)
                            .cursor_pointer()
                            .label("Connect to a remote server")
                            .primary()
                            .w_full()
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.open_remote(None, window, cx)
                            })),
                    );
            }
        };
        let key = self.key.clone().unwrap_or(KeyState::Missing);
        let loaded = key == KeyState::Loaded;
        panel = panel.child(intro(
            if loaded {
                "Your local address and key are filled in."
            } else {
                "Set up your local connection to get started."
            },
            theme,
        ));
        let incomplete = match (&key, &failure) {
            (_, Some(err)) => Some(err.clone()),
            (KeyState::Missing, None) => Some(
                "No local key was found. Finish installing the local server, then try again. \
                 You can also connect to a remote server."
                    .to_owned(),
            ),
            (KeyState::Unreadable(err), None) => Some(err.clone()),
            (KeyState::Loaded, None) => None,
        };
        if let Some(err) = &incomplete {
            let title = if loaded {
                "Could not connect"
            } else {
                "Local setup is incomplete"
            };
            panel = panel.child(notice(title, err, theme));
        }
        let (pill, pill_color) = match (&key, busy) {
            (_, Some(Phase::Starting | Phase::Waiting)) => ("Starting", theme.warning),
            (KeyState::Loaded, _) => ("Configured", theme.success),
            _ => ("Setup needed", theme.warning),
        };
        let label: SharedString = match busy {
            Some(Phase::Connecting) => "Connecting…".into(),
            Some(Phase::Starting) => "Starting local server…".into(),
            Some(Phase::Waiting) => "Waiting for the local server…".into(),
            None if incomplete.is_some() => "Try local again".into(),
            None if local.manages_service() => "Start local".into(),
            None => "Connect locally".into(),
        };
        let device = h_flex()
            .gap(px(10.))
            .items_center()
            .mb(px(13.))
            .child(
                div()
                    .flex_none()
                    .size(px(34.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .child(Icon::new(IconName::Laptop).text_color(theme.foreground)),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(capitalized(here)),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        if local.manages_service() {
                            "Local server"
                        } else {
                            "Development server"
                        },
                    )),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(pill_color)
                    .child(format!("● {pill}")),
            );
        let fact = |name: &'static str, value: SharedString| {
            h_flex()
                .gap_2()
                .justify_between()
                .text_xs()
                .child(div().text_color(theme.muted_foreground).child(name))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_family(theme.mono_font_family.clone())
                        .child(value),
                )
        };
        let shown_key: SharedString = match &key {
            KeyState::Loaded => "•••••••••••• · loaded".into(),
            KeyState::Missing => "Not found".into(),
            KeyState::Unreadable(_) => "Cannot be read".into(),
        };
        let card = v_flex()
            .p(px(15.))
            .mb(px(20.))
            .rounded(px(9.))
            .border_1()
            .border_color(theme.primary.opacity(0.3))
            .bg(theme.secondary)
            .child(device)
            .child(
                v_flex()
                    .gap(px(6.))
                    .pt(px(11.))
                    .border_t_1()
                    .border_color(theme.border)
                    .child(fact(
                        "Address",
                        local
                            .origin()
                            .as_str()
                            .trim_start_matches("http://")
                            .to_owned()
                            .into(),
                    ))
                    .child(fact("Owner key", shown_key)),
            );
        panel
            .child(card)
            .child(
                Button::new("connection-start")
                    .focus_ring(false)
                    .cursor_pointer()
                    .label(label)
                    .primary()
                    .w_full()
                    .h(px(40.))
                    .loading(busy.is_some())
                    .disabled(busy.is_some())
                    .on_click(cx.listener(|shell, _, window, cx| shell.start_local(window, cx))),
            )
            .when(setup.presentation == Presentation::Onboarding, |panel| {
                panel
                    .child(
                        Button::new("connection-edit")
                            .focus_ring(false)
                            .cursor_pointer()
                            .label("Edit configuration")
                            .w_full()
                            .h(px(40.))
                            .mt(px(9.))
                            .disabled(busy.is_some())
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.open_remote(None, window, cx)
                            })),
                    )
                    .child(note(
                        "Use Edit configuration to connect to a server on another machine.",
                        theme,
                    ))
            })
    }

    pub(super) fn render_remote(
        &self,
        setup: &Setup,
        form: &RemoteForm,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let busy = setup.draft.busy().is_some();
        let settings = matches!(setup.presentation, Presentation::Settings(_));
        let field = |label: &'static str, input: AnyElement, hint: &'static str| {
            v_flex()
                .gap(px(6.))
                .mb(px(16.))
                .child(div().text_xs().font_weight(FontWeight::MEDIUM).child(label))
                .child(input)
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(hint),
                )
        };
        let failure = match &setup.draft.check {
            Check::Failed(err) => Some(err.clone()),
            _ => setup.error.clone(),
        };
        v_flex()
            .when(!settings, |panel| {
                panel.child(
                    div().pb(px(16.)).child(
                        Button::new("connection-back")
                            .focus_ring(false)
                            .cursor_pointer()
                            .label("Back to local setup")
                            .icon(IconName::ChevronLeft)
                            .ghost()
                            .small()
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.back_to_welcome(window, cx)
                            })),
                    ),
                )
            })
            .child(heading(
                if settings {
                    "Connection details"
                } else {
                    "Connect to a remote server"
                }
                .into(),
                theme,
            ))
            .child(intro(
                "Use the address and owner key from the machine running your else.bot server.",
                theme,
            ))
            .child(field(
                "Server address",
                Input::new(&form.origin)
                    .focus_ring(false)
                    .aria_label("Server address")
                    .into_any_element(),
                "Use an HTTPS address, or a local SSH tunnel.",
            ))
            .child(field(
                "Owner key",
                Input::new(&form.token)
                    .focus_ring(false)
                    .aria_label("Owner key")
                    .mask_toggle()
                    .into_any_element(),
                "On your server, open ~/.else/data/owner-token.",
            ))
            .when_some(failure, |panel, err| {
                panel.child(notice("Could not connect", &err, theme))
            })
            .child(
                Button::new("connection-connect")
                    .focus_ring(false)
                    .cursor_pointer()
                    .label(if busy {
                        "Connecting…"
                    } else if settings {
                        "Save connection"
                    } else {
                        "Connect to server"
                    })
                    .primary()
                    .w_full()
                    .h(px(40.))
                    .loading(busy)
                    .disabled(busy)
                    .on_click(cx.listener(|shell, _, window, cx| shell.connect_remote(window, cx))),
            )
            .child(note("Connection checked before saving.", theme))
    }
}

fn story(theme: &Theme) -> AnyElement {
    let place = if cfg!(target_os = "macos") {
        "On your Mac."
    } else {
        "On your machine."
    };
    v_flex()
        .flex_none()
        .w(relative(0.39))
        .h_full()
        .justify_center()
        .items_start()
        .p(px(30.))
        .bg(theme.secondary)
        .border_r_1()
        .border_color(theme.border)
        .child(
            div()
                .size(px(52.))
                .mb(px(23.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(14.))
                .bg(theme.foreground)
                .text_color(theme.background)
                .text_size(px(28.))
                .font_weight(FontWeight::SEMIBOLD)
                .child("e."),
        )
        .child(
            v_flex()
                .mb(px(14.))
                .text_size(px(23.))
                .line_height(px(29.))
                .font_weight(FontWeight::MEDIUM)
                .child("Your bots.")
                .child(place),
        )
        .child(
            v_flex()
                .text_xs()
                .line_height(px(21.))
                .text_color(theme.muted_foreground)
                .child("Create a bot for each project.")
                .child("Give it a task and follow its work."),
        )
        .into_any_element()
}

fn heading(text: String, theme: &Theme) -> Div {
    div()
        .mb(px(10.))
        .text_size(px(22.))
        .line_height(px(27.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.foreground)
        .child(text)
}

fn intro(text: &'static str, theme: &Theme) -> Div {
    div()
        .mb(px(20.))
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(text)
}

fn note(text: &'static str, theme: &Theme) -> Div {
    div()
        .mt(px(17.))
        .text_xs()
        .text_center()
        .text_color(theme.muted_foreground)
        .child(text)
}

pub(super) fn notice(title: &str, text: &str, theme: &Theme) -> Div {
    v_flex()
        .gap(px(3.))
        .mb(px(17.))
        .p(px(12.))
        .rounded(px(7.))
        .border_1()
        .border_color(theme.warning.opacity(0.5))
        .bg(theme.warning.opacity(0.07))
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .child(title.to_owned()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text.to_owned()),
        )
}

pub(super) fn missing_local(why: &NoLocal) -> (&'static str, String) {
    let here = this_machine();
    match why {
        NoLocal::AppOnly => (
            "No server on this machine",
            format!(
                "Only the app is installed on {here}. Connect to your server on another machine, \
                 or add a local server with: elsebot update --both"
            ),
        ),
        NoLocal::NotInstalled => (
            "No local server found",
            format!("else.bot's server is not installed on {here}. Connect to a server elsewhere."),
        ),
        NoLocal::Explicit => (
            "No local server configured",
            "This session names no server on this machine. Connect to a server.".to_owned(),
        ),
        NoLocal::Refused(why) => ("The local installation cannot be used", why.clone()),
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn missing_local_states_name_the_next_step() {
        assert!(
            missing_local(&NoLocal::AppOnly)
                .1
                .contains("elsebot update --both")
        );
        assert_eq!(missing_local(&NoLocal::Refused("why".into())).1, "why");
        assert_eq!(capitalized(this_machine()).chars().next(), Some('T'));
    }
}
