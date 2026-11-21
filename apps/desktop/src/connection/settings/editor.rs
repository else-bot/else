//! The Servers page's editor that adds a server or edits a saved one.

use super::super::view::{missing_local, notice};
use super::*;

impl Shell {
    pub(super) fn render_connection_editor(
        &self,
        setup: &ConnectForm,
        editing: bool,
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
        let location = segment_track(cx)
            .w_full()
            .when(self.launch.local.is_ok(), |row| {
                row.child(
                    segment(
                        "settings-local-server",
                        "This machine",
                        matches!(setup.page, Page::Welcome),
                        false,
                        cx,
                    )
                    .toggled(matches!(setup.page, Page::Welcome))
                    .on_click(cx.listener(|shell, _, window, cx| {
                        if shell
                            .front
                            .form()
                            .is_some_and(|setup| !matches!(setup.page, Page::Welcome))
                        {
                            shell.back_to_welcome(window, cx);
                        }
                    })),
                )
            })
            .child(
                segment(
                    "settings-remote-server",
                    "Remote server",
                    matches!(setup.page, Page::Remote(_)),
                    false,
                    cx,
                )
                .toggled(matches!(setup.page, Page::Remote(_)))
                .on_click(cx.listener(|shell, _, window, cx| {
                    if shell
                        .front
                        .form()
                        .is_some_and(|setup| !matches!(setup.page, Page::Remote(_)))
                    {
                        let profile = shell.active.as_ref().map(|active| active.profile.clone());
                        shell.open_remote(profile.as_ref(), window, cx);
                    }
                })),
            );
        let mut content = v_flex()
            .child(
                h_flex().child(
                    Button::new("connection-editor-back")
                        .control(Tier::Ghost, Scale::Compact, false, cx)
                        .label("Back to servers")
                        .icon(IconName::ChevronLeft)
                        .on_click(cx.listener(|shell, _, window, cx| {
                            shell.close_server_editor(window, cx)
                        })),
                ),
            )
            .child(div().pt_4().child(section_heading(
                if editing { "Edit server" } else { "Add server" },
                "Test the connection before saving it.",
                cx,
            )))
            .child(div().h_6())
            .child(settings_row(
                "Name",
                "A name for this connection",
                text_field(Input::new(&setup.name), cx).aria_label("Server name"),
                stacked,
                theme,
            ))
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
                            .control(Tier::Primary, Scale::Standard, busy.is_some(), cx)
                            .label(label)
                            .loading(busy.is_some())
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
                        "HTTPS, Tailscale IP, or a local SSH tunnel",
                        v_flex()
                            .gap_2()
                            .child(
                                text_field(Input::new(&form.origin), cx)
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
                                text_field(Input::new(&form.token), cx)
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
                        .control(Tier::Primary, Scale::Standard, busy.is_some(), cx)
                        .label(if busy.is_some() {
                            "Connecting…"
                        } else {
                            "Test and save ↵"
                        })
                        .loading(busy.is_some())
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
                            .control(Tier::Secondary, Scale::Standard, false, cx)
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
