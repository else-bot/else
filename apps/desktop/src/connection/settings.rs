use super::view::{missing_local, notice};
use super::*;
use crate::controls::{Control as _, Scale, Tier, segment, segment_track, text_field};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogEntrance, DialogTitle};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{FocusableExt as _, Icon, Selectable as _, Theme, h_flex};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsSection {
    Server,
    Email,
    Socials,
    Mcp,
    Skills,
    Backups,
    Updates,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Presentation {
    Onboarding,
    Settings(SettingsSection),
}

impl Shell {
    pub(super) fn open_email_from_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_section().is_none() {
            return;
        }
        let bot = self
            .active
            .as_ref()
            .and_then(|active| active.app.read(cx).selected_bot_id());
        self.open_email(bot, window, cx);
        self.select_settings(SettingsSection::Email, window, cx);
    }

    pub(super) fn open_email(
        &mut self,
        bot: Option<uuid::Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = &self.active else {
            return;
        };
        let owner = active.id;
        let client = active.client.clone();
        let view = cx.new(|cx| crate::email::EmailSettings::new(client, bot, window, cx));
        let subscription = cx.subscribe_in(
            &view,
            window,
            move |shell, _, event: &crate::email::OpenBot, window, cx| {
                if !shell
                    .active
                    .as_ref()
                    .is_some_and(|active| active.id == owner)
                {
                    return;
                }
                let app = shell.active.as_ref().map(|active| active.app.clone());
                window.close_dialog(cx);
                shell.cancel(window, cx);
                if let Some(app) = app {
                    app.update(cx, |app, cx| app.open_bot(event.0, window, cx));
                }
            },
        );
        self.email = Some((view, subscription));
        cx.notify();
    }

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
        self.server_editor = None;
        self.server_update_target = None;
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
        self.refresh_servers(window, cx);
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
            crate::appearance::modal_surface(dialog, cx)
                .w((viewport.width - px(64.)).min(SETTINGS_SHEET_WIDTH))
                .h((viewport.height - px(64.)).min(px(760.)))
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

    pub(super) fn select_settings(
        &mut self,
        section: SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if section != SettingsSection::Socials {
            self.clear_socials(window, cx);
        } else if self.socials.is_none()
            && let Some(active) = &self.active
        {
            let client = active.app.read(cx).social_client();
            self.socials =
                Some(cx.new(|cx| crate::socials::SocialsView::new(client, None, window, cx)));
            if !active.app.read(cx).social_network_available() {
                self.socials
                    .as_ref()
                    .unwrap()
                    .update(cx, |view, cx| view.set_available(false, window, cx));
            }
        }
        if let Some(setup) = &mut self.setup {
            setup.presentation = Presentation::Settings(section);
            setup.navigation_focus[match section {
                SettingsSection::Server => 0,
                SettingsSection::Email => 1,
                SettingsSection::Socials => 2,
                SettingsSection::Mcp => 3,
                SettingsSection::Skills => 4,
                SettingsSection::Backups => 5,
                SettingsSection::Updates => 6,
            }]
            .focus(window, cx);
            cx.notify();
        }
        if section == SettingsSection::Skills {
            if self.skills.is_none() {
                self.open_skills(window, cx);
            }
        } else {
            self.clear_skills(window, cx);
        }
        if section == SettingsSection::Mcp {
            if self.mcp.is_none() {
                self.open_mcp(window, cx);
            }
        } else {
            self.mcp = None;
        }
        if section == SettingsSection::Backups && self.backups.is_none() {
            self.open_backups(window, cx);
        }
        if section == SettingsSection::Email && self.email.is_none() {
            self.open_email(None, window, cx);
        }
        if section == SettingsSection::Server {
            self.refresh_servers(window, cx);
        }
        if section == SettingsSection::Updates {
            self.server_update_target = None;
            self.updates.refresh();
        }
    }

    pub(super) fn clear_socials(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.socials.take() {
            view.update(cx, |view, cx| view.clear(window, cx));
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
                    if target == SettingsSection::Server {
                        shell.close_server_update(window, cx);
                        shell.close_server_editor(window, cx);
                    }
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
                crate::appearance::section_label("This app", cx)
                    .px_2()
                    .pt_1()
                    .pb_2(),
            )
            .child(nav_item(
                "settings-server",
                "Servers",
                IconName::Server,
                SettingsSection::Server,
                &setup.navigation_focus[0],
            ))
            .child(nav_item(
                "settings-updates",
                "App updates",
                IconName::Download,
                SettingsSection::Updates,
                &setup.navigation_focus[6],
            ))
            .child(
                crate::appearance::section_label("This server", cx)
                    .px_2()
                    .pt_6()
                    .pb_2()
                    .truncate(),
            )
            .child(nav_item(
                "settings-email",
                "Email",
                IconName::Mail,
                SettingsSection::Email,
                &setup.navigation_focus[1],
            ))
            .child(nav_item(
                "settings-socials",
                "Socials",
                IconName::MessageSquare,
                SettingsSection::Socials,
                &setup.navigation_focus[2],
            ))
            .child(nav_item(
                "settings-mcp",
                "MCP",
                IconName::Plug,
                SettingsSection::Mcp,
                &setup.navigation_focus[3],
            ))
            .child(nav_item(
                "settings-skills",
                "Skills",
                IconName::FileText,
                SettingsSection::Skills,
                &setup.navigation_focus[4],
            ))
            .child(nav_item(
                "settings-backups",
                "Backups",
                IconName::CloudUpload,
                SettingsSection::Backups,
                &setup.navigation_focus[5],
            ));
        let body = match section {
            SettingsSection::Server => self.render_server_settings(setup, window, cx),
            SettingsSection::Email => div()
                .children(self.email.as_ref().map(|(view, _)| view.clone()))
                .into_any_element(),
            SettingsSection::Socials => div().children(self.socials.clone()).into_any_element(),
            SettingsSection::Mcp => self.render_mcp(window, cx),
            SettingsSection::Skills => self.render_skills(cx),
            SettingsSection::Backups => self.render_backups(window, cx),
            SettingsSection::Updates => self.render_updates(cx),
        };
        let body = if section == SettingsSection::Backups {
            body
        } else if section == SettingsSection::Updates {
            v_flex()
                .size_full()
                .py_8()
                .child(settings_column(body).min_h_0())
                .into_any_element()
        } else {
            div()
                .size_full()
                .child(v_flex().min_h_full().py_8().child(settings_column(body)))
                .overflow_y_scrollbar()
                .into_any_element()
        };
        let body = v_flex()
            .size_full()
            .min_h_0()
            .child(self.render_server_identity(cx))
            .child(div().flex_1().min_h_0().child(body));
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
                        Button::new("app-settings-close")
                            .control(Tier::Ghost, Scale::Compact, false, cx)
                            .icon(IconName::Close)
                            .label("Esc")
                            .accessibility_label("Close settings, Escape")
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.cancel(window, cx);
                                window.close_dialog(cx);
                            })),
                    ),
            )
            .when_some(
                self.connection_error
                    .clone()
                    .filter(|_| section != SettingsSection::Server),
                |panel, error| {
                    panel.child(
                        div()
                            .px_5()
                            .py_2()
                            .text_xs()
                            .text_color(theme.danger)
                            .child(error),
                    )
                },
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
                                SettingsSection::Socials => "settings-socials-content",
                                SettingsSection::Mcp => "settings-mcp-content",
                                SettingsSection::Skills => "settings-skills-content",
                                SettingsSection::Backups => "settings-backups-content",
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
        if self.server_update_target.is_some() {
            return v_flex()
                .gap_4()
                .child(
                    Button::new("server-update-back")
                        .control(Tier::Ghost, Scale::Compact, false, cx)
                        .label("Back to servers")
                        .on_click(cx.listener(|shell, _, window, cx| {
                            shell.close_server_update(window, cx)
                        })),
                )
                .child(self.render_updates(cx))
                .into_any_element();
        }
        if self.server_editor.is_none() {
            return self.render_server_list(cx);
        }
        self.render_connection_editor(setup, window, cx)
    }

    fn render_connection_editor(
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
                        .setup
                        .as_ref()
                        .is_some_and(|setup| !matches!(setup.page, Page::Remote(_)))
                    {
                        let profile = shell.active.as_ref().map(|active| active.profile.clone());
                        shell.open_remote(profile.as_ref(), window, cx);
                    }
                })),
            );
        let mut content = v_flex()
            .child(
                Button::new("connection-editor-back")
                    .control(Tier::Ghost, Scale::Compact, false, cx)
                    .label("Back to servers")
                    .on_click(
                        cx.listener(|shell, _, window, cx| shell.close_server_editor(window, cx)),
                    ),
            )
            .child(div().pt_4().child(section_heading(
                if matches!(self.server_editor, Some(servers::Editor::Edit(_))) {
                    "Edit server"
                } else {
                    "Add server"
                },
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

/// Width of the content column on every Settings page. The server strip,
/// page bodies and footers share it so their left and right edges line up.
pub(super) const SETTINGS_COLUMN: Pixels = px(640.);
/// Inset of that column from the left edge of the Settings pane.
pub(super) const SETTINGS_INSET: Pixels = px(32.);
/// The sheet fits the navigation, one inset column and a matching margin,
/// so every page keeps the same size and no page floats in a wide void.
const SETTINGS_SHEET_WIDTH: Pixels = px(920.);

/// Place `content` in the shared Settings column. The column grows to fill
/// the pane's height, so a page can center an empty state in it.
pub(super) fn settings_column(content: impl IntoElement) -> Div {
    v_flex().flex_1().w_full().px(SETTINGS_INSET).child(
        v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .max_w(SETTINGS_COLUMN)
            .child(content),
    )
}

/// The empty state of an inventory page (MCP, Skills): centered in the
/// space under the page heading, raised slightly so it sits optically
/// centered, with one supporting line and the button that fixes it.
pub(super) fn empty_state(
    icon: IconName,
    title: &'static str,
    detail: &'static str,
    action: Button,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    v_flex()
        .flex_1()
        .w_full()
        .min_h(px(240.))
        .pb(px(72.))
        .items_center()
        .justify_center()
        .text_center()
        .gap_2()
        .child(
            Icon::new(icon)
                .size_6()
                .mb_1()
                .text_color(theme.muted_foreground),
        )
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
                .child(detail),
        )
        .child(div().pt_2().child(action))
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
