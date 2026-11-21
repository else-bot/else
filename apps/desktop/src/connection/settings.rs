mod editor;
mod state;

pub(super) use state::{ServerPage, Settings};

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
    Vault,
    Usage,
    Computers,
    Backups,
    Updates,
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
        if let Some(settings) = self.front.settings_mut() {
            settings.email = Some((view, subscription));
        }
        cx.notify();
    }

    pub(super) fn settings_section(&self) -> Option<SettingsSection> {
        self.front.settings().map(|settings| settings.section)
    }

    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        if self.settings_section().is_some() {
            self.cancel(window, cx);
        }
        let Some(active) = &self.active else { return };
        active
            .app
            .update(cx, |app, cx| app.set_hidden(true, window, cx));
        let settings = Settings::new(cx);
        self.set_front(Front::Settings(Box::new(settings)), window, cx);
        self.refresh_servers(window, cx);
        self.settings_focus.focus(window, cx);
        let shell = cx.weak_entity();
        let shell_close = shell.clone();
        let first = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false)
                && let Some(shell) = shell.upgrade()
                && let Some(settings) = shell.read(cx).front.settings()
            {
                let focus = settings.navigation_focus[0].clone();
                focus.focus(window, cx);
            }
            let shell = shell.clone();
            let shell_close = shell_close.clone();
            crate::appearance::modal_surface(dialog, cx)
                .w(sheet_size(window.viewport_size()).width)
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
                    content.w_full().min_h_0().children(
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
        let Some(current) = self
            .front
            .settings()
            .map(|settings| settings.pane().section())
        else {
            return;
        };
        let pane = (current != Some(section)).then(|| self.new_pane(section, window, cx));
        if let Some(settings) = self.front.settings_mut() {
            if let Some(pane) = pane {
                settings.show(pane, window, cx);
            }
            settings.section = section;
            if section == SettingsSection::Updates
                && matches!(settings.servers(), ServerPage::Update(_))
            {
                settings.set_servers(ServerPage::List, window, cx);
            }
            settings.navigation_focus[match section {
                SettingsSection::Server => 0,
                SettingsSection::Email => 1,
                SettingsSection::Socials => 2,
                SettingsSection::Mcp => 3,
                SettingsSection::Skills => 4,
                SettingsSection::Vault => 9,
                SettingsSection::Usage => 5,
                SettingsSection::Backups => 6,
                SettingsSection::Updates => 7,
                SettingsSection::Computers => 8,
            }]
            .focus(window, cx);
            cx.notify();
        }
        let (backups, email) = self
            .front
            .settings()
            .map(|settings| (settings.backups.is_some(), settings.email.is_some()))
            .unwrap_or_default();
        if section == SettingsSection::Backups && !backups {
            self.open_backups(window, cx);
        }
        if section == SettingsSection::Email && !email {
            self.open_email(None, window, cx);
        }
        if section == SettingsSection::Server {
            self.refresh_servers(window, cx);
        }
        if section == SettingsSection::Updates {
            self.updates.refresh();
        }
    }

    /// Follows the active server's connection: losing it wipes typed
    /// skill keys and closes the Vault, which reopens once it is back.
    pub(super) fn connection_changed(
        &mut self,
        connected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.front.settings_mut() else {
            return;
        };
        match settings.pane() {
            pane::Pane::Skills(skills) => skills
                .view
                .update(cx, |view, cx| view.set_connected(connected, window, cx)),
            pane::Pane::Vault(_) if !connected => settings.show(pane::Pane::None, window, cx),
            pane::Pane::None if connected && settings.section == SettingsSection::Vault => {
                let pane = self.new_pane(SettingsSection::Vault, window, cx);
                if let Some(settings) = self.front.settings_mut() {
                    settings.show(pane, window, cx);
                }
            }
            _ => {}
        }
    }

    fn render_app_settings(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(settings) = self.front.settings() else {
            return div().into_any_element();
        };
        let section = settings.section;
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
            .p_3()
            .gap_1()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.secondary.opacity(0.45))
            .rounded_bl(sheet_corner(cx))
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
                &settings.navigation_focus[0],
            ))
            .child(nav_item(
                "settings-updates",
                "App updates",
                IconName::Download,
                SettingsSection::Updates,
                &settings.navigation_focus[7],
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
                &settings.navigation_focus[1],
            ))
            .child(nav_item(
                "settings-socials",
                "Socials",
                IconName::MessageSquare,
                SettingsSection::Socials,
                &settings.navigation_focus[2],
            ))
            .child(nav_item(
                "settings-mcp",
                "MCP",
                IconName::Plug,
                SettingsSection::Mcp,
                &settings.navigation_focus[3],
            ))
            .child(nav_item(
                "settings-skills",
                "Skills",
                IconName::FileText,
                SettingsSection::Skills,
                &settings.navigation_focus[4],
            ))
            .child(nav_item(
                "settings-vault",
                "Vault",
                IconName::Lock,
                SettingsSection::Vault,
                &settings.navigation_focus[9],
            ))
            .child(nav_item(
                "settings-usage",
                "Usage",
                IconName::Gauge,
                SettingsSection::Usage,
                &settings.navigation_focus[5],
            ))
            .child(nav_item(
                "settings-computers",
                "Computers",
                IconName::Monitor,
                SettingsSection::Computers,
                &settings.navigation_focus[8],
            ))
            .child(nav_item(
                "settings-backups",
                "Backups",
                IconName::CloudUpload,
                SettingsSection::Backups,
                &settings.navigation_focus[6],
            ));
        let body = match section {
            SettingsSection::Server => self.render_server_settings(settings.servers(), window, cx),
            SettingsSection::Email => div()
                .children(settings.email.as_ref().map(|(view, _)| view.clone()))
                .into_any_element(),
            SettingsSection::Vault if settings.pane().section().is_none() => div()
                .child("Reconnect to the server to open the Vault.")
                .into_any_element(),
            SettingsSection::Socials
            | SettingsSection::Mcp
            | SettingsSection::Skills
            | SettingsSection::Vault
            | SettingsSection::Usage
            | SettingsSection::Computers => settings.pane().element(),
            SettingsSection::Backups => match &settings.backups {
                Some(pane) => pane.view.clone().into_any_element(),
                None => div()
                    .p_8()
                    .child("Loading backup settings…")
                    .into_any_element(),
            },
            SettingsSection::Updates => self.render_updates(cx),
        };
        // Every page scrolls in the same frame, whose 32 px padding keeps
        // its first and last lines clear of the edge fades at rest.
        let body = if section == SettingsSection::Backups {
            body
        } else if section == SettingsSection::Updates {
            v_flex()
                .flex_1()
                .min_h_0()
                .py_8()
                .child(settings_column(body).min_h_0())
                .into_any_element()
        } else {
            sheet_scroller(
                div()
                    .id("settings-page-scroll")
                    .size_full()
                    .child(v_flex().min_h_full().py_8().child(settings_column(body)))
                    .overflow_y_scrollbar(),
                cx,
            )
            .into_any_element()
        };
        v_flex()
            .id("app-settings")
            .track_focus(&settings.focus)
            .on_modifiers_changed(cx.listener(|shell, event: &ModifiersChangedEvent, _, cx| {
                shell.modifiers_changed(event.modifiers, cx)
            }))
            .w_full()
            .h(sheet_size(window.viewport_size()).height)
            .overflow_hidden()
            .child(
                // One header: the sheet's title over the server it edits.
                h_flex()
                    .flex_none()
                    .h(px(64.))
                    .px_5()
                    .gap_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                DialogTitle::new()
                                    .text_size(px(15.))
                                    .line_height(px(20.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Settings"),
                            )
                            .child(self.render_server_identity(cx)),
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
                                SettingsSection::Vault => "settings-vault-content",
                                SettingsSection::Usage => "settings-usage-content",
                                SettingsSection::Computers => "settings-computers-content",
                                SettingsSection::Backups => "settings-backups-content",
                                SettingsSection::Updates => "settings-updates-content",
                            })
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(body),
                    ),
            )
            .into_any_element()
    }

    fn render_server_settings(
        &self,
        page: &ServerPage,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (setup, editing) = match page {
            ServerPage::List => return self.render_server_list(cx),
            ServerPage::Add(form) => (form, false),
            ServerPage::Edit { form, .. } => (form, true),
            ServerPage::Update(_) => {
                return v_flex()
                    .gap_4()
                    .child(
                        h_flex().child(
                            Button::new("server-update-back")
                                .control(Tier::Ghost, Scale::Compact, false, cx)
                                .label("Back to servers")
                                .icon(IconName::ChevronLeft)
                                .on_click(cx.listener(|shell, _, window, cx| {
                                    shell.close_server_update(window, cx)
                                })),
                        ),
                    )
                    .child(self.render_updates(cx))
                    .into_any_element();
            }
        };
        self.render_connection_editor(setup, editing, window, cx)
    }
}

/// Inset of the content column from the edges of the Settings pane.
const SETTINGS_INSET: Pixels = px(32.);
/// Every Settings page shares one sheet, sized for the Usage gauges and
/// ledger, so switching pages never resizes the dialog.
const SHEET_SIZE: Size<Pixels> = Size {
    width: px(1120.),
    height: px(900.),
};
/// The least gap between the sheet and each window edge.
const SHEET_CLEARANCE: Pixels = px(32.);

/// The sheet size, shrunk only when the window cannot fit it with
/// [`SHEET_CLEARANCE`] on every side. The dialog centres it.
fn sheet_size(viewport: Size<Pixels>) -> Size<Pixels> {
    let room = |extent: Pixels| (extent - SHEET_CLEARANCE * 2.).max(px(0.));
    Size {
        width: SHEET_SIZE.width.min(room(viewport.width)),
        height: SHEET_SIZE.height.min(room(viewport.height)),
    }
}

/// The radius just inside the sheet's 1 px border. Clipping is rectangular,
/// so any fill that reaches a bottom corner rounds itself to this.
pub(super) fn sheet_corner(cx: &App) -> Pixels {
    (cx.theme().radius_lg - px(1.)).max(px(0.))
}

/// A page's scroll container, faded at both edges into the sheet and
/// rounded where it meets the sheet's bottom-right corner.
pub(super) fn sheet_scroller(scroller: impl IntoElement, cx: &App) -> Div {
    crate::panel_kit::faded(scroller, cx.theme().popover, sheet_corner(cx))
        .flex_1()
        .min_h_0()
}

/// Place `content` in the shared Settings column, which fills the pane.
pub(super) fn settings_column(content: impl IntoElement) -> Div {
    v_flex()
        .flex_1()
        .min_h_0()
        .w_full()
        .px(SETTINGS_INSET)
        .child(content)
}

/// The empty state of an inventory page (MCP, Skills): a 28 px icon, 12 px
/// to the title, one supporting line, and 16 px to the button that fixes
/// it. It fills the page under the description and centres itself there,
/// a little above the middle so it reads as centred.
pub(super) fn empty_state(
    icon: IconName,
    title: &'static str,
    detail: &'static str,
    action: Button,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    v_flex()
        .w_full()
        .flex_1()
        .justify_center()
        .pb_10()
        .items_center()
        .text_center()
        .child(
            Icon::new(icon)
                .size(px(28.))
                .text_color(theme.muted_foreground.opacity(0.8)),
        )
        .child(
            div()
                .pt_3()
                .text_size(px(15.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(
            div()
                .pt_1()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(detail),
        )
        .child(div().pt_4().child(action))
}

pub(super) fn section_heading(title: &'static str, description: &'static str, cx: &App) -> Div {
    crate::panel_kit::page_heading(title, description, Vec::new(), cx)
}
