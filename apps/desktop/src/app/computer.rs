use std::{collections::HashMap, sync::Arc, time::Duration};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Icon, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use crate::api::{
    Client, ClientError, ComputerState, ComputerUpgrade, DesktopCommand, DesktopCommands,
    DesktopEvent, DesktopMode, MAX_CLIPBOARD_BYTES,
};

#[cfg(target_os = "macos")]
mod text_input;

actions!(computer_viewer, [GuestTab, GuestBackTab]);

const CARRIED: &str = "Files, projects, browser sign-ins, installed packages, and anything in \
                       /usr/local or /opt carry over. Changes to /etc don't.";

pub(super) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", GuestTab, Some("ComputerControl")),
        KeyBinding::new("shift-tab", GuestBackTab, Some("ComputerControl")),
    ]);
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum DetailsTab {
    #[default]
    Details,
    Computer,
}

enum Screen {
    Loading,
    Asleep,
    Waking,
    Live,
    Failed(String),
    Unavailable,
    Upgrading,
}

pub(super) struct ComputerView {
    client: Arc<Client>,
    bot: Uuid,
    name: String,
    avatar: crate::avatar::Avatar,
    screen: Screen,
    computer: Option<ComputerState>,
    upgrade_refused: Option<String>,
    image: Option<Arc<RenderImage>>,
    painted_image: Option<Arc<RenderImage>>,
    resolution: (u16, u16),
    commands: Option<DesktopCommands>,
    mode: DesktopMode,
    mode_pending: bool,
    /// The owner asked to take over before the screen was live.
    control_on_live: bool,
    guest_cursor: Option<CursorStyle>,
    notice: Option<String>,
    pub expanded: bool,
    focus: FocusHandle,
    bounds: Bounds<Pixels>,
    buttons: u8,
    keys: HashMap<String, u32>,
    modifiers: Modifiers,
    /// The host clipboard text the guest last received in this control session.
    shared_clipboard: Option<String>,
    #[cfg(target_os = "macos")]
    marked_text: Option<String>,
    scroll: Point<f32>,
    _session: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

pub(super) struct LayoutChanged;
impl EventEmitter<LayoutChanged> for ComputerView {}

impl ComputerView {
    pub fn new(
        client: Arc<Client>,
        bot: &crate::api::Bot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let handle = window.window_handle();
        cx.on_release(move |view, cx| {
            if let Some(image) = view.painted_image.take() {
                cx.defer(move |cx| {
                    let _ = handle.update(cx, |_, window, _| window.drop_image(image));
                });
            }
        })
        .detach();
        let focus = cx.focus_handle();
        let blur = cx.on_blur(&focus, window, |view, _, cx| view.release(cx));
        let activation = cx.observe_window_activation(window, |view, window, cx| {
            if window.is_window_active() {
                view.share_clipboard(cx);
            } else {
                view.release(cx);
            }
        });
        let mut view = Self {
            client,
            bot: bot.id,
            name: bot.name.clone(),
            avatar: bot.avatar,
            screen: Screen::Loading,
            computer: None,
            upgrade_refused: None,
            image: None,
            painted_image: None,
            resolution: (0, 0),
            commands: None,
            mode: DesktopMode::Watch,
            mode_pending: false,
            control_on_live: false,
            guest_cursor: None,
            notice: None,
            expanded: false,
            focus,
            bounds: Bounds::default(),
            buttons: 0,
            keys: HashMap::new(),
            modifiers: Modifiers::default(),
            shared_clipboard: None,
            #[cfg(target_os = "macos")]
            marked_text: None,
            scroll: point(0., 0.),
            _session: None,
            _subscriptions: vec![blur, activation],
        };
        view.monitor(false, cx);
        view
    }

    fn monitor(&mut self, wake: bool, cx: &mut Context<Self>) {
        self.clear_input();
        self.notice = None;
        self.commands = None;
        self.image = None;
        self.mode = DesktopMode::Watch;
        self.mode_pending = false;
        self.guest_cursor = None;
        self.screen = if wake {
            Screen::Waking
        } else {
            Screen::Loading
        };
        let client = self.client.clone();
        let bot = self.bot;
        self._session = Some(cx.spawn(async move |this, cx| {
            if wake && let Err(error) = client.clone().resume_computer(bot).await {
                let _ = this.update(cx, |view, cx| view.fail(error.to_string(), cx));
                return;
            }
            loop {
                let running = if wake {
                    true
                } else {
                    match client.clone().computer(bot).await {
                        Ok(state) => {
                            let upgrading = state.upgrade == Some(ComputerUpgrade::Running);
                            let available = state.desktop_available;
                            let running =
                                state.state == "running" && state.desired_state != "stopped";
                            let _ = this.update(cx, |view, cx| {
                                view.computer = Some(state);
                                if upgrading {
                                    view.watch_upgrade(cx);
                                }
                                cx.notify();
                            });
                            if upgrading {
                                return;
                            }
                            if !available {
                                let _ = this.update(cx, |view, cx| {
                                    view.screen = Screen::Unavailable;
                                    cx.notify();
                                });
                                return;
                            }
                            running
                        }
                        Err(error) => {
                            let _ = this.update(cx, |view, cx| {
                                view.fail(error.to_string(), cx);
                            });
                            return;
                        }
                    }
                };
                if running {
                    let mut stream = client.clone().desktop(bot, wake);
                    if this
                        .update(cx, |view, cx| {
                            view.commands = Some(stream.commands.clone());
                            view.screen = if wake {
                                Screen::Waking
                            } else {
                                Screen::Loading
                            };
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                    while let Some(event) = stream.events.recv().await {
                        if this
                            .update(cx, |view, cx| {
                                match event {
                                    DesktopEvent::Size { width, height } => {
                                        view.resolution = (width, height)
                                    }
                                    DesktopEvent::Frame(image) => {
                                        view.image = Some(image);
                                        view.screen = Screen::Live;
                                        if std::mem::take(&mut view.control_on_live) {
                                            view.release(cx);
                                            view.mode_pending = true;
                                            view.send(
                                                DesktopCommand::Mode {
                                                    mode: DesktopMode::Control,
                                                },
                                                cx,
                                            );
                                        }
                                    }
                                    DesktopEvent::Mode(mode) => {
                                        view.mode = mode;
                                        view.mode_pending = false;
                                        view.shared_clipboard = None;
                                        if mode == DesktopMode::Control {
                                            view.share_clipboard(cx);
                                        } else {
                                            view.guest_cursor = None;
                                        }
                                    }
                                    DesktopEvent::Cursor(style) => {
                                        view.guest_cursor = Some(style);
                                    }
                                    DesktopEvent::Asleep => {
                                        view.clear_input();
                                        view.image = None;
                                        view.screen = Screen::Asleep;
                                        view.guest_cursor = None;
                                        view.mode = DesktopMode::Watch;
                                        view.mode_pending = false;
                                    }
                                    DesktopEvent::Notice(message) => {
                                        view.release(cx);
                                        view.mode_pending = false;
                                        view.notice = Some(message);
                                    }
                                    DesktopEvent::Error(error) => {
                                        view.fail(error, cx);
                                    }
                                }
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    let _ = this.update(cx, |view, cx| {
                        view.commands = None;
                        view.mode = DesktopMode::Watch;
                        view.mode_pending = false;
                        if !matches!(view.screen, Screen::Failed(_) | Screen::Asleep) {
                            view.fail("Connection closed. Click to reconnect.".into(), cx);
                        }
                        if !wake && view.control_on_live && matches!(view.screen, Screen::Asleep) {
                            view.wake_after_session(cx);
                        }
                        cx.notify();
                    });
                    return;
                }
                let wake = this.update(cx, |view, cx| {
                    view.screen = Screen::Asleep;
                    cx.notify();
                    view.control_on_live
                });
                match wake {
                    Err(_) => return,
                    Ok(true) => {
                        let _ = this.update(cx, |view, cx| view.wake_after_session(cx));
                        return;
                    }
                    Ok(false) => {}
                }
                cx.background_executor().timer(Duration::from_secs(3)).await;
            }
        }));
        cx.notify();
    }

    fn upgrade(&mut self, cx: &mut Context<Self>) {
        self.release(cx);
        let client = self.client.clone();
        let bot = self.bot;
        self.upgrade_refused = None;
        self.watch_upgrade(cx);
        self._session = Some(cx.spawn(async move |this, cx| {
            let started = client.clone().upgrade_computer(bot).await;
            let _ = this.update(cx, |view, cx| match started {
                Ok(()) => view.watch_upgrade(cx),
                Err(error) => {
                    view.upgrade_refused = Some(error.to_string());
                    if let Some(computer) = &mut view.computer {
                        computer.upgrade = None;
                    }
                    view.monitor(false, cx);
                }
            });
        }));
    }

    fn watch_upgrade(&mut self, cx: &mut Context<Self>) {
        self.clear_input();
        self.notice = None;
        self.commands = None;
        self.image = None;
        self.mode = DesktopMode::Watch;
        self.mode_pending = false;
        self.guest_cursor = None;
        self.screen = Screen::Upgrading;
        if let Some(computer) = &mut self.computer {
            computer.upgrade = Some(ComputerUpgrade::Running);
        }
        let client = self.client.clone();
        let bot = self.bot;
        self._session = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let state = match client.clone().computer(bot).await {
                    Ok(state) => state,
                    Err(
                        error @ (ClientError::Unauthorized
                        | ClientError::Api {
                            status: 400..=499, ..
                        }),
                    ) => {
                        let _ = this.update(cx, |view, cx| view.fail(error.to_string(), cx));
                        return;
                    }
                    Err(_) => continue,
                };
                if state.upgrade == Some(ComputerUpgrade::Running) {
                    continue;
                }
                let _ = this.update(cx, |view, cx| {
                    view.computer = Some(state);
                    view.monitor(false, cx);
                });
                return;
            }
        }));
        cx.notify();
    }

    fn upgrade_section(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let computer = self.computer.as_ref()?;
        let theme = cx.theme();
        let (message, failed) = match (&computer.upgrade, &self.upgrade_refused) {
            (Some(ComputerUpgrade::Running), _) => {
                return Some(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "Upgrading to the latest computer. This takes a few minutes. {CARRIED}"
                        ))
                        .into_any_element(),
                );
            }
            (
                Some(ComputerUpgrade::Completed {
                    unrestored_packages,
                }),
                _,
            ) if !computer.outdated() => {
                return Some(
                    div()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(format!(
                            "Upgraded, but these packages couldn't be installed again: {}. \
                             The bot can install them again itself.",
                            unrestored_packages.join(", ")
                        ))
                        .into_any_element(),
                );
            }
            _ if !computer.outdated() => return None,
            (_, Some(refused)) => (refused.clone(), true),
            (Some(ComputerUpgrade::Failed { error }), None) => {
                (format!("Upgrade failed: {error}"), true)
            }
            (None | Some(ComputerUpgrade::Completed { .. }), None) => (
                format!(
                    "A newer computer is available. {CARRIED} \
                     The bot can't use its computer while it upgrades."
                ),
                false,
            ),
        };
        Some(
            v_flex()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .text_color(if failed {
                            theme.danger
                        } else {
                            theme.muted_foreground
                        })
                        .child(message),
                )
                .child(
                    Button::new("upgrade-computer")
                        .cursor_pointer()
                        .focus_ring(false)
                        .label(if failed {
                            "Try upgrading again"
                        } else {
                            "Upgrade computer"
                        })
                        .small()
                        .w_full()
                        .on_click(cx.listener(|view, _, _, cx| view.upgrade(cx))),
                )
                .into_any_element(),
        )
    }

    fn fail(&mut self, error: String, cx: &mut Context<Self>) {
        self.notice = None;
        self.guest_cursor = None;
        self.screen = Screen::Failed(error);
        self.image = None;
        self.mode = DesktopMode::Watch;
        self.mode_pending = false;
        self.clear_input();
        cx.notify();
    }

    fn send(&mut self, command: DesktopCommand, cx: &mut Context<Self>) {
        if self
            .commands
            .as_ref()
            .is_some_and(|commands| !commands.send_or_disconnect(command))
        {
            self.fail("Connection interrupted. Click to reconnect.".into(), cx);
            self.commands = None;
        }
    }

    fn clear_input(&mut self) {
        self.keys.clear();
        #[cfg(target_os = "macos")]
        {
            self.marked_text = None;
        }
        self.buttons = 0;
        self.modifiers = Modifiers::default();
        self.scroll = point(0., 0.);
    }

    fn release(&mut self, cx: &mut Context<Self>) {
        self.clear_input();
        self.send(DesktopCommand::Release, cx);
    }

    fn set_mode(&mut self, mode: DesktopMode, window: &mut Window, cx: &mut Context<Self>) {
        self.release(cx);
        self.notice = None;
        self.mode_pending = true;
        self.send(DesktopCommand::Mode { mode }, cx);
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Wake the computer for a pending take-over, in a new session that
    /// starts once the current one has returned.
    fn wake_after_session(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |view, cx| view.monitor(true, cx));
        })
        .detach();
    }

    /// Expand the screen and take control as soon as it is live.
    pub(super) fn take_over(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.screen, Screen::Asleep | Screen::Failed(_)) {
            self.monitor(true, cx);
        }
        self.expanded = true;
        if matches!(self.screen, Screen::Live) {
            if self.mode != DesktopMode::Control {
                self.set_mode(DesktopMode::Control, window, cx);
            }
        } else {
            self.control_on_live = true;
            self.focus.focus(window, cx);
        }
        cx.emit(LayoutChanged);
        cx.notify();
    }

    fn collapse(&mut self, cx: &mut Context<Self>) {
        self.control_on_live = false;
        self.release(cx);
        if self.mode == DesktopMode::Control {
            self.send(
                DesktopCommand::Mode {
                    mode: DesktopMode::Watch,
                },
                cx,
            );
            self.mode = DesktopMode::Watch;
        }
        self.expanded = false;
        cx.emit(LayoutChanged);
        cx.notify();
    }

    fn controlling(&self) -> bool {
        self.expanded
            && self.mode == DesktopMode::Control
            && !self.mode_pending
            && matches!(self.screen, Screen::Live)
    }

    fn pointer(&mut self, position: Point<Pixels>, buttons: u8, cx: &mut Context<Self>) {
        if !self.controlling() {
            return;
        }
        if let Some((x, y)) = screen_point(self.bounds, self.resolution, position) {
            self.buttons = buttons;
            self.send(DesktopCommand::Pointer { x, y, buttons }, cx);
        }
    }

    /// Gives the guest the host clipboard's text when it changed since the
    /// guest last received it, so text copied inside the guest stays until
    /// something new is copied on the host. False when the host text is too
    /// long, so a paste does not insert the guest's older text instead.
    fn share_clipboard(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.controlling() {
            return true;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return true;
        };
        if text.is_empty() || self.shared_clipboard.as_ref() == Some(&text) {
            return true;
        }
        if text.len() > MAX_CLIPBOARD_BYTES {
            self.notice =
                Some("The clipboard holds more text than the desktop takes (256 KB).".into());
            cx.notify();
            return false;
        }
        self.send(DesktopCommand::Clipboard { text: text.clone() }, cx);
        self.shared_clipboard = Some(text);
        true
    }

    fn sync_modifiers(&mut self, next: Modifiers, cx: &mut Context<Self>) {
        for (before, after, key) in [
            (self.modifiers.control, next.control, 0xffe3),
            (self.modifiers.shift, next.shift, 0xffe1),
            (self.modifiers.alt, next.alt, 0xffe9),
            (self.modifiers.platform, next.platform, 0xffeb),
        ] {
            if before != after {
                self.send(DesktopCommand::Key { key, down: after }, cx);
            }
        }
        self.modifiers = next;
    }

    fn key(&mut self, stroke: &Keystroke, down: bool, cx: &mut Context<Self>) {
        if !self.controlling() {
            return;
        }
        if down && pastes(stroke) && !self.share_clipboard(cx) {
            return;
        }
        self.sync_modifiers(stroke.modifiers, cx);
        #[cfg(target_os = "macos")]
        if down && stroke.modifiers.platform && !stroke.modifiers.control && stroke.key == "v" {
            // The guest pastes with Ctrl+V. Cmd stays held, so it is pressed again after.
            for (key, down) in [
                (0xffeb, false),
                (0xffe3, true),
                (0x76, true),
                (0x76, false),
                (0xffe3, false),
                (0xffeb, true),
            ] {
                self.send(DesktopCommand::Key { key, down }, cx);
            }
            return;
        }
        let key = if down {
            let Some(key) = keysym(stroke) else {
                return;
            };
            self.keys.insert(stroke.key.clone(), key);
            key
        } else {
            let Some(key) = self.keys.remove(&stroke.key) else {
                return;
            };
            key
        };
        self.send(DesktopCommand::Key { key, down }, cx);
    }

    fn image_area(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let radius = rems(if self.expanded { 0.5 } else { 0.75 });
        let mut area = div()
            .id("computer-screen")
            .group("computer-screen")
            .relative()
            .w_full()
            .h_full()
            .rounded(radius)
            .overflow_hidden()
            .bg(rgb(0x111111));
        if let Some(image) = &self.image {
            area = area.child(
                img(image.clone())
                    .absolute()
                    .inset_0()
                    .size_full()
                    .rounded(radius)
                    .object_fit(ObjectFit::Contain),
            );
        } else {
            let (text, hint) = match &self.screen {
                Screen::Loading => ("Checking computer…", ""),
                Screen::Asleep => ("Computer is asleep", "Click to wake up"),
                Screen::Waking => ("Waking up…", "Your desktop will appear here"),
                Screen::Failed(error) if self.expanded => (error.as_str(), "Click to try again"),
                Screen::Failed(_) => ("Connection interrupted", "Click to try again"),
                Screen::Unavailable => (
                    "Computer unavailable",
                    "Desktop access is not enabled on this server",
                ),
                Screen::Live => ("Connecting…", ""),
                Screen::Upgrading => ("Upgrading computer…", "This takes a few minutes"),
            };
            area = area.child(
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .text_color(rgb(0xaaaaaa))
                    .child(div().opacity(0.55).child(
                        crate::avatar::bot_avatar("computer-avatar", self.avatar, px(56.)).still(),
                    ))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xf5f5f5))
                            .child(text.to_owned()),
                    )
                    .child(div().text_xs().child(hint)),
            );
        }
        if !self.expanded {
            if matches!(
                self.screen,
                Screen::Live | Screen::Asleep | Screen::Failed(_)
            ) {
                area = area
                    .cursor_pointer()
                    .on_click(cx.listener(|view, _, window, cx| {
                        if !matches!(view.screen, Screen::Live) {
                            view.monitor(true, cx);
                        }
                        view.expanded = true;
                        view.focus.focus(window, cx);
                        cx.emit(LayoutChanged);
                        cx.notify();
                    }));
            }
            if matches!(self.screen, Screen::Live) {
                area = area.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .opacity(0.)
                        .group_hover("computer-screen", |style| style.opacity(1.))
                        .child(
                            h_flex()
                                .px_3()
                                .py_2()
                                .gap_2()
                                .rounded_full()
                                .bg(rgba(0x000000bb))
                                .text_color(white())
                                .child(Icon::new(IconName::Maximize).small())
                                .child("Open"),
                        ),
                );
            }
        } else if matches!(self.screen, Screen::Failed(_) | Screen::Asleep) {
            area = area
                .cursor_pointer()
                .on_click(cx.listener(|view, _, _, cx| view.monitor(true, cx)));
        } else if self.controlling() {
            area = area
                .cursor(self.guest_cursor.unwrap_or(CursorStyle::PointingHand))
                .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, _, _cx| {
                    if let Some((x, y)) = screen_point(view.bounds, view.resolution, event.position)
                        && let Some(commands) = &view.commands
                    {
                        commands.move_pointer(x, y, view.buttons);
                    }
                }))
                .on_scroll(cx.listener(|view, event: &ScrollEvent, _, cx| {
                    let (delta, threshold) = match event.delta {
                        ScrollDelta::Pixels(p) => (point(f32::from(p.x), f32::from(p.y)), 40.),
                        ScrollDelta::Lines(p) => (p, 1.),
                    };
                    view.scroll.x += delta.x / threshold;
                    view.scroll.y += delta.y / threshold;
                    let dx = view.scroll.x.trunc().clamp(-8., 8.) as i32;
                    let dy = view.scroll.y.trunc().clamp(-8., 8.) as i32;
                    view.scroll.x -= dx as f32;
                    view.scroll.y -= dy as f32;
                    for (count, positive, negative) in [(dy, 8, 16), (dx, 32, 64)] {
                        let mask = if count > 0 { positive } else { negative };
                        let buttons = view.buttons;
                        for _ in 0..count.abs() {
                            view.pointer(event.position, buttons | mask, cx);
                            view.pointer(event.position, buttons, cx);
                        }
                    }
                    cx.stop_propagation();
                }));
            for button in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
                area = area
                    .on_mouse_down(
                        button,
                        cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                            view.focus.focus(window, cx);
                            if button == MouseButton::Middle && !view.share_clipboard(cx) {
                                cx.stop_propagation();
                                return;
                            }
                            view.sync_modifiers(event.modifiers, cx);
                            view.pointer(event.position, view.buttons | button_mask(button), cx);
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_up(
                        button,
                        cx.listener(move |view, event: &MouseUpEvent, _, cx| {
                            if screen_point(view.bounds, view.resolution, event.position).is_some()
                            {
                                view.pointer(
                                    event.position,
                                    view.buttons & !button_mask(button),
                                    cx,
                                );
                            } else {
                                view.release(cx);
                            }
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_up_out(button, cx.listener(|view, _, _, cx| view.release(cx)));
            }
        }
        let view = cx.entity().downgrade();
        area.child(
            canvas(
                move |bounds, _, cx| {
                    let _ = view.update(cx, |view, _| view.bounds = bounds);
                },
                {
                    #[cfg(target_os = "macos")]
                    let view = cx.entity();
                    move |bounds, _, window, cx| {
                        #[cfg(target_os = "macos")]
                        if view.read(cx).controlling() {
                            window.handle_input(
                                &view.read(cx).focus.clone(),
                                ElementInputHandler::new(bounds, view.clone()),
                                cx,
                            );
                        }
                        #[cfg(not(target_os = "macos"))]
                        let _ = (bounds, window, cx);
                    }
                },
            )
            .absolute()
            .inset_0()
            .size_full(),
        )
    }
}

impl Render for ComputerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.painted_image != self.image {
            if let Some(image) = self.painted_image.take() {
                let _ = window.drop_image(image);
            }
            self.painted_image = self.image.clone();
        }
        let theme = cx.theme().clone();
        if !self.expanded {
            return v_flex()
                .gap_2()
                .w_full()
                .child(
                    self.image_area(cx)
                        .h(px(176.))
                        .border_1()
                        .border_color(theme.border),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .text_center()
                        .child(format!("{}'s screen", self.name)),
                )
                .children(self.upgrade_section(cx))
                .children(match &self.screen {
                    Screen::Failed(error) => Some(
                        div()
                            .text_xs()
                            .text_color(theme.danger)
                            .child(error.clone()),
                    ),
                    _ => None,
                })
                .into_any_element();
        }
        let mode = self.mode;
        v_flex()
            .id("computer-expanded")
            .track_focus(&self.focus)
            .key_context(if self.controlling() {
                "ComputerControl"
            } else {
                "ComputerViewer"
            })
            .on_action(cx.listener(|view, _: &GuestTab, _, cx| {
                let stroke = Keystroke::parse("tab").expect("valid key");
                view.key(&stroke, true, cx);
                view.key(&stroke, false, cx);
            }))
            .on_action(cx.listener(|view, _: &GuestBackTab, _, cx| {
                let stroke = Keystroke::parse("shift-tab").expect("valid key");
                view.key(&stroke, true, cx);
                view.key(&stroke, false, cx);
                view.sync_modifiers(Modifiers::default(), cx);
            }))
            .capture_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if view.controlling() {
                    #[cfg(target_os = "macos")]
                    if text_input::is_text(&event.keystroke) {
                        return;
                    }
                    view.key(&event.keystroke, true, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }))
            .on_key_up(cx.listener(|view, event: &KeyUpEvent, _, cx| {
                if view.controlling() {
                    view.key(&event.keystroke, false, cx);
                    cx.stop_propagation();
                }
            }))
            .on_modifiers_changed(cx.listener(|view, event: &ModifiersChangedEvent, _, cx| {
                if view.controlling() {
                    view.sync_modifiers(event.modifiers, cx);
                }
            }))
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                h_flex()
                    .h(px(64.))
                    .px_6()
                    .items_center()
                    .justify_between()
                    .child(
                        v_flex().child(self.name.clone()).child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Computer"),
                        ),
                    )
                    .child(
                        Button::new("close-computer")
                            .cursor_pointer()
                            .label("Close")
                            .icon(IconName::Minimize)
                            .ghost()
                            .on_click(cx.listener(|view, _, _, cx| view.collapse(cx))),
                    ),
            )
            .child(div().mx_6().flex_1().min_h_0().child(self.image_area(cx)))
            .child(
                h_flex()
                    .h(px(80.))
                    .gap_3()
                    .items_center()
                    .justify_center()
                    .child(
                        Button::new("watch-computer")
                            .cursor_pointer()
                            .label("Watching")
                            .disabled(self.mode_pending || !matches!(self.screen, Screen::Live))
                            .icon(IconName::Eye)
                            .when(mode == DesktopMode::Watch, |button| button.primary())
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.set_mode(DesktopMode::Watch, window, cx)
                            })),
                    )
                    .child(
                        Button::new("control-computer")
                            .cursor_pointer()
                            .disabled(self.mode_pending || !matches!(self.screen, Screen::Live))
                            .label(if self.mode_pending {
                                "Switching…"
                            } else {
                                "Take over"
                            })
                            .when(mode == DesktopMode::Control, |button| button.primary())
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.set_mode(DesktopMode::Control, window, cx)
                            })),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        if mode == DesktopMode::Control {
                            "You control this computer"
                        } else {
                            "Your input stays on this device"
                        },
                    )),
            )
            .children(self.notice.as_ref().map(|notice| {
                div()
                    .px_6()
                    .pb_2()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(notice.clone())
            }))
            .into_any_element()
    }
}

fn button_mask(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 4,
        _ => 0,
    }
}

fn screen_point(
    bounds: Bounds<Pixels>,
    resolution: (u16, u16),
    position: Point<Pixels>,
) -> Option<(u16, u16)> {
    let (width, height) = resolution;
    if width == 0 || height == 0 {
        return None;
    }
    let scale = (f32::from(bounds.size.width) / width as f32)
        .min(f32::from(bounds.size.height) / height as f32);
    let x = (f32::from(position.x - bounds.origin.x)
        - (f32::from(bounds.size.width) - width as f32 * scale) / 2.)
        / scale;
    let y = (f32::from(position.y - bounds.origin.y)
        - (f32::from(bounds.size.height) - height as f32 * scale) / 2.)
        / scale;
    (x >= 0. && y >= 0. && x < width as f32 && y < height as f32).then_some((x as u16, y as u16))
}

/// Whether `stroke` pastes in a Linux app: Ctrl+V (with Shift in terminals),
/// Shift+Insert, or the Mac's Cmd+V.
fn pastes(stroke: &Keystroke) -> bool {
    let modifiers = stroke.modifiers;
    match stroke.key.as_str() {
        "v" => modifiers.control || modifiers.platform,
        "insert" => modifiers.shift && !modifiers.control,
        _ => false,
    }
}

fn keysym(stroke: &Keystroke) -> Option<u32> {
    let named = match stroke.key.as_str() {
        "backspace" => 0xff08,
        "tab" if stroke.modifiers.shift => 0xfe20,
        "tab" => 0xff09,
        "enter" => 0xff0d,
        "escape" => 0xff1b,
        "delete" => 0xffff,
        "insert" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" => 0xff55,
        "pagedown" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "space" => 0x20,
        key if key.starts_with('f')
            && key[1..].parse::<u32>().is_ok_and(|n| (1..=12).contains(&n)) =>
        {
            0xffbd + key[1..].parse::<u32>().ok()?
        }
        _ => {
            let text = stroke.key_char.as_deref().unwrap_or(&stroke.key);
            let mut chars = text.chars();
            let ch = chars.next()? as u32;
            if chars.next().is_some() {
                return None;
            }
            return Some(if ch <= 0xff { ch } else { 0x01000000 | ch });
        }
    };
    Some(named)
}

#[cfg(test)]
mod tests {
    use super::{keysym, pastes, screen_point};
    use gpui_kit::{Bounds, Keystroke, point, px, size};
    #[test]
    fn pointer_maps_letterboxed_screen_and_refuses_margins() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(800.), px(800.)));
        assert_eq!(
            screen_point(bounds, (1280, 800), point(px(410.), px(420.))),
            Some((640, 400))
        );
        assert_eq!(
            screen_point(bounds, (1280, 800), point(px(410.), px(30.))),
            None
        );
        assert_eq!(
            screen_point(bounds, (1280, 800), point(px(810.), px(420.))),
            None
        );
        assert_eq!(screen_point(bounds, (0, 0), point(px(10.), px(20.))), None);
    }
    #[test]
    fn keyboard_maps_shortcuts_symbols_and_unicode() {
        assert_eq!(
            keysym(&Keystroke::parse("ctrl-l").unwrap()),
            Some('l' as u32)
        );
        assert_eq!(keysym(&Keystroke::parse("tab").unwrap()), Some(0xff09));
        assert_eq!(
            keysym(&Keystroke::parse("shift-tab").unwrap()),
            Some(0xfe20)
        );
        assert_eq!(keysym(&Keystroke::parse("enter").unwrap()), Some(0xff0d));
        assert_eq!(keysym(&Keystroke::parse("f12").unwrap()), Some(0xffc9));
        assert_eq!(keysym(&Keystroke::parse("é").unwrap()), Some(0xe9));
    }

    #[test]
    fn paste_shortcuts_share_the_clipboard_and_other_chords_do_not() {
        for paste in ["ctrl-v", "ctrl-shift-v", "cmd-v", "shift-insert"] {
            assert!(pastes(&Keystroke::parse(paste).unwrap()), "{paste}");
        }
        for other in ["v", "shift-v", "ctrl-c", "insert", "ctrl-insert"] {
            assert!(!pastes(&Keystroke::parse(other).unwrap()), "{other}");
        }
    }
}
