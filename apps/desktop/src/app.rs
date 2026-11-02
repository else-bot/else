//! The messaging window: bot sidebar and selected conversation.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::*;
use uuid::Uuid;

use crate::api::{Bot, BotList, Capabilities, Client, ClientError};

actions!(else_desktop, [SelectNext, SelectPrev]);

const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Retrying a rejected token cannot succeed; poll rarely until it changes.
const UNAUTHORIZED_POLL_INTERVAL: Duration = Duration::from_secs(30);
const SIDEBAR_CONTEXT: &str = "Sidebar";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, Some(SIDEBAR_CONTEXT)),
        KeyBinding::new("up", SelectPrev, Some(SIDEBAR_CONTEXT)),
        KeyBinding::new("ctrl-n", SelectNext, Some(SIDEBAR_CONTEXT)),
        KeyBinding::new("ctrl-p", SelectPrev, Some(SIDEBAR_CONTEXT)),
    ]);
}

/// What the window knows about its API connection.
#[derive(Debug, Clone, PartialEq)]
pub enum Connection {
    Connecting,
    Connected,
    /// Shown with any history already loaded; polling keeps retrying.
    Disconnected(String),
    /// Retrying will not help until the token is fixed.
    InvalidCredentials(String),
    /// Configuration could not be loaded at all.
    Misconfigured(String),
}

pub struct ElseApp {
    client: Option<Arc<Client>>,
    connection: Connection,
    caps: Option<Capabilities>,
    bots: Vec<Bot>,
    default_bot_id: Option<Uuid>,
    selected: Option<Uuid>,
    sidebar_focus: FocusHandle,
    _poll: Option<Task<()>>,
}

impl ElseApp {
    pub fn new(
        client: Result<Arc<Client>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sidebar_focus = cx.focus_handle();
        sidebar_focus.focus(window, cx);
        let (client, connection) = match client {
            Ok(client) => (Some(client), Connection::Connecting),
            Err(message) => (None, Connection::Misconfigured(message)),
        };
        let mut app = Self {
            client,
            connection,
            caps: None,
            bots: Vec::new(),
            default_bot_id: None,
            selected: None,
            sidebar_focus,
            _poll: None,
        };
        app.start_polling(cx);
        app
    }

    /// Refresh capabilities and bot summaries now and every few seconds.
    /// Network work runs on the client's runtime, never in `render`.
    fn start_polling(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                let caps = client.clone().capabilities().await;
                let bots = match &caps {
                    Ok(_) => Some(client.clone().bots().await),
                    Err(_) => None,
                };
                let Ok(interval) = this.update(cx, |app, cx| {
                    app.apply_refresh(caps, bots, cx);
                    match app.connection {
                        Connection::InvalidCredentials(_) => UNAUTHORIZED_POLL_INTERVAL,
                        _ => POLL_INTERVAL,
                    }
                }) else {
                    return;
                };
                cx.background_executor().timer(interval).await;
            }
        }));
    }

    fn apply_refresh(
        &mut self,
        caps: Result<Capabilities, ClientError>,
        bots: Option<Result<BotList, ClientError>>,
        cx: &mut Context<Self>,
    ) {
        let result = caps.and_then(|caps| {
            bots.expect("bots fetched with capabilities")
                .map(|b| (caps, b))
        });
        match result {
            Ok((caps, list)) => {
                self.caps = Some(caps);
                self.default_bot_id = list.default_bot_id;
                self.bots = list.bots;
                self.connection = Connection::Connected;
                self.reconcile_selection();
            }
            Err(ClientError::Unauthorized) => {
                self.connection =
                    Connection::InvalidCredentials(ClientError::Unauthorized.to_string());
            }
            // Keep whatever was loaded; show the problem and keep retrying.
            Err(err) => self.connection = Connection::Disconnected(err.to_string()),
        }
        cx.notify();
    }

    fn active_bots(&self) -> impl Iterator<Item = &Bot> {
        self.bots.iter().filter(|bot| bot.is_active())
    }

    /// Prefer the current selection, then the saved default, then the oldest
    /// active bot. Navigation only: this never changes the saved default.
    fn reconcile_selection(&mut self) {
        let active = |id: Option<Uuid>| id.filter(|id| self.active_bots().any(|b| b.id == *id));
        self.selected = active(self.selected)
            .or(active(self.default_bot_id))
            .or_else(|| self.active_bots().next().map(|b| b.id));
    }

    fn select(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.selected = Some(id);
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let ids: Vec<Uuid> = self.active_bots().map(|b| b.id).collect();
        if ids.is_empty() {
            return;
        }
        let current = self
            .selected
            .and_then(|id| ids.iter().position(|b| *b == id))
            .unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, ids.len() as isize - 1) as usize;
        self.select(ids[next], cx);
    }

    fn selected_bot(&self) -> Option<&Bot> {
        self.selected
            .and_then(|id| self.bots.iter().find(|b| b.id == id))
    }

    fn status_text(&self) -> (SharedString, bool) {
        match &self.connection {
            Connection::Connecting => ("Connecting…".into(), false),
            Connection::Connected => ("Connected".into(), false),
            Connection::Disconnected(message) => {
                (format!("Disconnected — retrying. {message}").into(), true)
            }
            Connection::InvalidCredentials(message) => (message.clone().into(), true),
            Connection::Misconfigured(message) => (message.clone().into(), true),
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (status, is_error) = self.status_text();
        let mut list = v_flex().gap_1().p_2().flex_1();
        for bot in &self.bots {
            let id = bot.id;
            let selected = self.selected == Some(id);
            let pending = !bot.is_active();
            let activity = if pending {
                "Deleting…".to_owned()
            } else {
                activity_label(bot)
            };
            let mut name_row = h_flex()
                .gap_2()
                .child(div().text_sm().child(bot.name.clone()));
            if self.default_bot_id == Some(id) {
                name_row = name_row.child(
                    div()
                        .text_xs()
                        .px_1()
                        .rounded_sm()
                        .bg(theme.secondary)
                        .text_color(theme.secondary_foreground)
                        .child("Main"),
                );
            }
            let mut row = v_flex()
                .id(SharedString::from(format!("bot-{id}")))
                .px_2()
                .py_1p5()
                .rounded_md()
                .child(name_row)
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(activity),
                );
            if pending {
                row = row.opacity(0.5);
            } else {
                row = row
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.list_hover))
                    .on_click(cx.listener(move |app, _, _, cx| app.select(id, cx)));
            }
            if selected {
                row = row.bg(theme.list_active);
            }
            list = list.child(row);
        }
        if self.bots.is_empty() && self.connection == Connection::Connected {
            list = list.child(
                div()
                    .p_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No bots yet."),
            );
        }
        v_flex()
            .id("sidebar")
            .key_context(SIDEBAR_CONTEXT)
            .track_focus(&self.sidebar_focus)
            .on_action(cx.listener(|app, _: &SelectNext, _, cx| app.move_selection(1, cx)))
            .on_action(cx.listener(|app, _: &SelectPrev, _, cx| app.move_selection(-1, cx)))
            .w(px(260.))
            .h_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                v_flex()
                    .p_3()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("else.bot"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(if is_error {
                                theme.danger
                            } else {
                                theme.muted_foreground
                            })
                            .child(status),
                    ),
            )
            .child(list)
    }

    fn render_main(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let body = match self.selected_bot() {
            Some(bot) => v_flex()
                .gap_2()
                .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(bot.name.clone()))
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(bot.role.clone().unwrap_or_default()),
                ),
            None => v_flex().gap_2().child(div().text_color(theme.muted_foreground).child(
                match self.connection {
                    Connection::Connected => "Create a bot to start a conversation.",
                    Connection::InvalidCredentials(_) | Connection::Misconfigured(_) => {
                        "Fix the desktop configuration shown in the sidebar, then restart the desktop."
                    }
                    _ => "Waiting for the API…",
                },
            )),
        };
        v_flex()
            .flex_1()
            .h_full()
            .p_4()
            .bg(theme.background)
            .child(body)
    }
}

fn activity_label(bot: &Bot) -> String {
    let Some(activity) = &bot.activity else {
        return String::new();
    };
    match activity.phase.as_str() {
        "working" if activity.queued_count > 0 => {
            format!("Working · {} queued", activity.queued_count)
        }
        "working" => "Working".into(),
        "queued" => format!("{} queued", activity.queued_count),
        "cancelling" => "Stopping…".into(),
        "unavailable" => "Unavailable".into(),
        _ => "Idle".into(),
    }
}

impl Render for ElseApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .size_full()
            .text_color(theme.foreground)
            .child(self.render_sidebar(cx))
            .child(self.render_main(cx))
    }
}
