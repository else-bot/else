//! The Ctrl+K (Cmd+K on macOS) quick switcher: a search over every saved
//! server and the bots last seen on each, drawn over the window. Ranking and
//! keys live in `crate::quick_switcher`; this file draws the switcher and
//! opens what it picks.

use std::collections::HashMap;
use std::ops::Range;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{
    Backspace, Enter, Escape, IndentInline, Input, MoveDown, MoveUp, OutdentInline,
};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{Icon, h_flex};

use super::servers::Health;
use super::*;
use crate::appearance::{Tone, tone};
use crate::avatar::bot_avatar;
use crate::quick_switcher::{self as qs, Index, Key, Memory, Outcome, Picker, Target};

const CONTEXT: &str = "QuickSwitcher";
const WIDTH: f32 = 500.;
const ROW: f32 = 40.;
const VISIBLE_ROWS: usize = 8;
const PLACEHOLDER: &str = "Open a bot or server…";

actions!(
    else_desktop,
    [ToggleQuickSwitcher, SwitcherNext, SwitcherPrevious]
);

pub(crate) fn bind_keys(cx: &mut App) {
    let toggle = if cfg!(target_os = "macos") {
        "cmd-k"
    } else {
        "ctrl-k"
    };
    cx.bind_keys([
        KeyBinding::new(toggle, ToggleQuickSwitcher, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-n", SwitcherNext, Some(CONTEXT)),
        KeyBinding::new("ctrl-p", SwitcherPrevious, Some(CONTEXT)),
    ]);
}

pub(super) struct Switcher {
    input: Entity<InputState>,
    picker: Picker,
    index: Index,
    scroll: UniformListScrollHandle,
    /// What had focus before the switcher opened, for Esc to return to.
    restore: FocusHandle,
    /// The active server `restore` belongs to.
    server: Uuid,
    _input: Subscription,
}

/// The switcher's memory and its pending write.
#[derive(Default)]
pub(super) struct Recall {
    memory: Memory,
    /// When each server's cached bot list was fetched, so an older list
    /// never replaces a newer one.
    fetched: HashMap<Uuid, Instant>,
    _save: Option<Task<()>>,
}

impl Recall {
    pub(super) fn load(store: &Result<crate::config::ProfileStore, String>) -> Self {
        Self {
            memory: store
                .as_ref()
                .map(|store| Memory::load(&store.quick_switcher_path()))
                .unwrap_or_default(),
            fetched: HashMap::new(),
            _save: None,
        }
    }
}

impl Shell {
    /// The switcher is modal: Ctrl+1..9 server keys wait until it closes.
    pub(super) fn switcher_open(&self) -> bool {
        self.switcher.is_some()
    }

    pub(super) fn toggle_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.is_some() {
            self.close_switcher(window, cx);
            return;
        }
        let Some(active) = self.active.as_ref().map(|active| active.id) else {
            return;
        };
        if window.has_active_dialog(cx) || matches!(self.front, Front::Onboarding(_)) {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(PLACEHOLDER));
        let subscription = cx.subscribe_in(&input, window, |shell, _, event, window, cx| {
            if matches!(event, InputEvent::Change) {
                shell.search_switcher(cx);
                shell.preload_highlighted(window, cx);
            }
        });
        let restore = window
            .focused(cx)
            .unwrap_or_else(|| self.settings_focus.clone());
        input.read(cx).focus_handle(cx).focus(window, cx);
        let index = self.switcher_index(cx);
        let mut picker = Picker::default();
        picker.refresh(&index, "", &self.recall.memory.recents);
        self.switcher = Some(Switcher {
            input,
            picker,
            index,
            scroll: UniformListScrollHandle::new(),
            restore,
            server: active,
            _input: subscription,
        });
        self.preload_highlighted(window, cx);
        cx.notify();
    }

    fn close_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(switcher) = self.switcher.take() {
            switcher.restore.focus(window, cx);
        }
        cx.notify();
    }

    /// Preloads the highlighted bot's conversation if it is on the shown
    /// server, so opening it lands warm.
    fn preload_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(switcher), Some(active)) = (&self.switcher, &self.active) else {
            return;
        };
        if let Some(Target::Bot { server, bot }) = switcher
            .picker
            .hits
            .get(switcher.picker.selected)
            .map(|hit| hit.target)
            && server == active.id
        {
            active
                .app
                .update(cx, |app, cx| app.dwell_on(bot, window, cx));
        }
    }

    fn search_switcher(&mut self, cx: &mut Context<Self>) {
        let Some(switcher) = &mut self.switcher else {
            return;
        };
        let query = switcher.input.read(cx).value();
        switcher
            .picker
            .refresh(&switcher.index, &query, &self.recall.memory.recents);
        switcher.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn switcher_key(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) {
        let Some(switcher) = &mut self.switcher else {
            return;
        };
        let query_empty = switcher.input.read(cx).value().is_empty();
        let outcome = switcher.picker.press(key, query_empty);
        if outcome != Outcome::Pass {
            // Keeps the key from the search field and from moving focus.
            cx.stop_propagation();
        }
        match outcome {
            Outcome::Pass => {}
            Outcome::Moved => {
                switcher
                    .scroll
                    .scroll_to_item(switcher.picker.selected, ScrollStrategy::Nearest);
                self.preload_highlighted(window, cx);
                cx.notify();
            }
            Outcome::Rescoped => {
                let placeholder = match switcher
                    .picker
                    .scope
                    .and_then(|id| switcher.index.server(id))
                {
                    Some(server) => format!("Search bots on {}…", server.name),
                    None => PLACEHOLDER.to_owned(),
                };
                switcher.input.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                    input.set_placeholder(placeholder, window, cx);
                });
                self.search_switcher(cx);
            }
            Outcome::Open(target) => self.open_switcher_target(target, window, cx),
            Outcome::Close => self.close_switcher(window, cx),
        }
    }

    fn open_switcher_target(
        &mut self,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_switcher(window, cx);
        self.remember(target, cx);
        // A newer pick supersedes a bot still waiting for its list.
        self.arrival = None;
        if let Some(active) = &self.active {
            active.app.update(cx, |app, _| app.drop_requested_bot());
        }
        let here = self
            .active
            .as_ref()
            .is_some_and(|a| a.id == target.server());
        match target {
            Target::Bot { bot, .. } if here => self.open_active_bot(bot, window, cx),
            Target::Server { .. } if here => {
                if !matches!(self.front, Front::Session) {
                    self.cancel(window, cx);
                }
            }
            Target::Bot { server, bot } => {
                self.arrival = Some(Arrival {
                    server,
                    open: Open::Bot(bot),
                });
                self.select_server(server, window, cx);
            }
            Target::Server { server } => {
                self.arrival = Some(Arrival {
                    server,
                    open: Open::Server,
                });
                self.select_server(server, window, cx);
            }
        }
    }

    /// Opens a bot of the active server and leaves settings for its chat.
    pub(super) fn open_active_bot(
        &mut self,
        bot: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = &self.active else { return };
        let app = active.app.clone();
        if !app.read(cx).may_have_bot(bot) {
            return;
        }
        if !matches!(self.front, Front::Session) {
            if self.settings_section().is_some() {
                window.close_dialog(cx);
            }
            self.cancel(window, cx);
        }
        app.update(cx, |app, cx| app.open_bot_chat(bot, window, cx));
    }

    /// Keeps a server's latest bot list for searches while it is offline,
    /// including after a restart.
    pub(super) fn cache_bots(
        &mut self,
        id: Uuid,
        bots: &[qs::Bot],
        fetched: Instant,
        cx: &mut Context<Self>,
    ) {
        if self
            .recall
            .fetched
            .get(&id)
            .is_some_and(|at| *at >= fetched)
        {
            return;
        }
        self.recall.fetched.insert(id, fetched);
        if self.recall.memory.bots.get(&id).map(Vec::as_slice) == Some(bots) {
            return;
        }
        self.recall.memory.bots.insert(id, bots.to_vec());
        self.save_recall(cx);
    }

    /// Caches a session's bot list when it is newer than the cached one.
    pub(super) fn cache_session_bots(
        &mut self,
        id: Uuid,
        app: &Entity<ElseApp>,
        cx: &mut Context<Self>,
    ) {
        let Some(at) = app.read(cx).listed_at() else {
            return;
        };
        if self
            .recall
            .fetched
            .get(&id)
            .is_some_and(|known| *known >= at)
        {
            return;
        }
        if let Some((at, bots)) = app.read(cx).switcher_bots() {
            self.cache_bots(id, &bots, at, cx);
        }
    }

    /// Keeps keyboard focus in an open switcher when a switch or a bot list
    /// lands behind it, and returns focus there once it closes.
    pub(super) fn reclaim_switcher_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(switcher) = &mut self.switcher else {
            return;
        };
        if let Some(active) = &self.active
            && active.id != switcher.server
        {
            switcher.server = active.id;
            switcher.restore = active.app.read(cx).sidebar_focus_handle();
        }
        let input = switcher.input.read(cx).focus_handle(cx);
        if input.is_focused(window) {
            return;
        }
        if let Some(focused) = window.focused(cx) {
            switcher.restore = focused;
        }
        input.focus(window, cx);
    }

    /// Records an opened item and saves the switcher's memory shortly after.
    pub(super) fn remember(&mut self, target: Target, cx: &mut Context<Self>) {
        if self.recall.memory.recents.first() == Some(&target) {
            return;
        }
        qs::remember(&mut self.recall.memory.recents, target);
        self.save_recall(cx);
    }

    fn save_recall(&mut self, cx: &mut Context<Self>) {
        let Ok(path) = self.launch.store.as_ref().map(|s| s.quick_switcher_path()) else {
            return;
        };
        self.recall._save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let Ok(memory) = this.update(cx, |shell, _| shell.recall.memory.clone()) else {
                return;
            };
            let saved = cx
                .background_executor()
                .spawn(async move { memory.save(&path) })
                .await;
            if let Err(err) = saved {
                eprintln!("{err}");
            }
        }));
    }

    /// Every saved server in rail order with its bots: a session's live list
    /// when it has one, otherwise the newest list cached from any source.
    fn switcher_index(&mut self, cx: &mut Context<Self>) -> Index {
        let mut servers = Vec::with_capacity(self.registry.servers.len());
        for saved in &self.registry.servers {
            let session = match &self.active {
                Some(active) if active.id == saved.id => Some(active),
                _ => self.inactive.get(&saved.id),
            };
            let bots = session
                .and_then(|session| session.app.read(cx).switcher_bots())
                .map(|(_, bots)| bots)
                .or_else(|| self.recall.memory.bots.get(&saved.id).cloned())
                .unwrap_or_default();
            servers.push(qs::Server {
                id: saved.id,
                name: saved.name.clone(),
                bots,
            });
        }
        let index = Index { servers };
        let memory = &mut self.recall.memory;
        let before = (memory.bots.len(), memory.recents.len());
        memory.bots.retain(|id, _| index.server(*id).is_some());
        memory.recents.retain(|target| index.contains(*target));
        if before != (memory.bots.len(), memory.recents.len()) {
            self.save_recall(cx);
        }
        index
    }

    /// Whether a server reports itself unreachable or unusable, with the
    /// words to show; a server still being checked counts as reachable.
    fn switcher_trouble(&self, id: Uuid) -> Option<&'static str> {
        match self.servers.entries.get(&id).map(|entry| entry.health) {
            Some(Health::Offline) => Some("Offline"),
            Some(Health::Unauthorized) => Some("Sign in needed"),
            Some(Health::Incompatible) => Some("Unsupported API"),
            Some(Health::Online | Health::Checking) | None => None,
        }
    }

    pub(super) fn render_switcher(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let switcher = self.switcher.as_ref()?;
        let theme = cx.theme().clone();
        let picker = &switcher.picker;
        let count = picker.hits.len();
        let query_empty = switcher.input.read(cx).value().is_empty();
        let scope = picker
            .scope
            .and_then(|id| switcher.index.server(id))
            .map(|server| {
                h_flex()
                    .flex_none()
                    .gap_1()
                    .pl_1()
                    .pr_2()
                    .py(px(2.))
                    .rounded(theme.radius)
                    .bg(theme.secondary)
                    .text_sm()
                    .text_color(tone(Tone::Strong, cx))
                    .child(self.switcher_server_icon(server.id, px(16.), cx))
                    .child(server.name.clone())
                    .child(Icon::new(IconName::ChevronRight).size_3())
            });
        let header = h_flex()
            .flex_none()
            .h(px(48.))
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Icon::new(IconName::Search)
                    .size_4()
                    .text_color(theme.muted_foreground),
            )
            .children(scope)
            .child(
                Input::new(&switcher.input)
                    .appearance(false)
                    .aria_label("Search bots and servers")
                    .flex_1()
                    .text_base(),
            );
        let body = if count == 0 {
            let query = switcher.input.read(cx).value();
            let text = if picker.scope.is_some() && query_empty {
                "No bots on this server yet.".to_owned()
            } else if query_empty {
                "Type to search every bot and server.".to_owned()
            } else {
                format!("No bots or servers match “{query}”.")
            };
            div()
                .px_4()
                .py_6()
                .text_sm()
                .text_center()
                .text_color(theme.muted_foreground)
                .child(text)
                .into_any_element()
        } else {
            let heading = (query_empty && picker.scope.is_none()).then(|| {
                div()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child("Recent")
            });
            v_flex()
                .p_1()
                .children(heading)
                .child(
                    div()
                        .id("quick-switcher-list")
                        .role(Role::ListBox)
                        .aria_label("Bots and servers")
                        .child(
                            uniform_list(
                                "quick-switcher-results",
                                count,
                                cx.processor(|shell, range: Range<usize>, _, cx| {
                                    range
                                        .filter_map(|ix| shell.render_switcher_row(ix, cx))
                                        .collect()
                                }),
                            )
                            .h(px(count.min(VISIBLE_ROWS) as f32 * ROW))
                            .w_full()
                            .track_scroll(&switcher.scroll),
                        ),
                )
                .into_any_element()
        };
        let hint = |keys: &'static str, label: &'static str| {
            let keys: Vec<AnyElement> = keys
                .split(' ')
                .filter_map(|key| Keystroke::parse(key).ok())
                .map(|key| Kbd::new(key).into_any_element())
                .collect();
            h_flex().gap_1().items_center().children(keys).child(label)
        };
        let footer = h_flex()
            .flex_none()
            .px_3()
            .py(px(7.))
            .gap_4()
            .items_center()
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(hint("up down", "Move"))
            .child(hint("enter", "Open"))
            .children(
                picker
                    .scope
                    .is_none()
                    .then(|| hint("tab", "Search in server")),
            )
            .child(div().flex_1())
            .child(if picker.scope.is_some() {
                hint("escape", "Leave server")
            } else {
                hint("escape", "Close")
            });
        let panel = v_flex()
            .id("quick-switcher")
            .key_context(CONTEXT)
            .role(Role::Dialog)
            .aria_label("Quick switcher")
            .occlude()
            .w(px(WIDTH))
            .max_w(relative(0.92))
            .mt(px(72.))
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .shadow_lg()
            .overflow_hidden()
            .on_mouse_down_out(cx.listener(|shell, _, window, cx| shell.close_switcher(window, cx)))
            .capture_action(cx.listener(|shell, _: &MoveDown, window, cx| {
                shell.switcher_key(Key::Down, window, cx)
            }))
            .capture_action(
                cx.listener(|shell, _: &MoveUp, window, cx| {
                    shell.switcher_key(Key::Up, window, cx)
                }),
            )
            .capture_action(cx.listener(|shell, action: &Enter, window, cx| {
                if action.shift || action.secondary {
                    return;
                }
                shell.switcher_key(Key::Enter, window, cx)
            }))
            .capture_action(cx.listener(|shell, _: &IndentInline, window, cx| {
                shell.switcher_key(Key::Tab, window, cx)
            }))
            .capture_action(cx.listener(|_, _: &OutdentInline, _, cx| cx.stop_propagation()))
            .capture_action(cx.listener(|shell, _: &Escape, window, cx| {
                shell.switcher_key(Key::Escape, window, cx)
            }))
            .capture_action(cx.listener(|shell, _: &Backspace, window, cx| {
                shell.switcher_key(Key::Backspace, window, cx)
            }))
            .on_action(cx.listener(|shell, _: &SwitcherNext, window, cx| {
                shell.switcher_key(Key::Down, window, cx)
            }))
            .on_action(cx.listener(|shell, _: &SwitcherPrevious, window, cx| {
                shell.switcher_key(Key::Up, window, cx)
            }))
            .child(header)
            .child(body)
            .child(footer);
        Some(
            deferred(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .justify_center()
                    .items_start()
                    .bg(theme.overlay)
                    .occlude()
                    .child(panel),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    fn render_switcher_row(&self, ix: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let switcher = self.switcher.as_ref()?;
        let hit = switcher.picker.hits.get(ix)?;
        let selected = ix == switcher.picker.selected;
        let scoped = switcher.picker.scope.is_some();
        let theme = cx.theme().clone();
        let server = switcher.index.server(hit.target.server())?;
        let trouble = self.switcher_trouble(server.id);
        let (icon, name, kind, label) = match hit.target {
            Target::Bot { bot, .. } => {
                let bot = server.bots.iter().find(|b| b.id == bot)?;
                let icon = bot_avatar(
                    SharedString::from(format!("switcher-avatar-{}-{}", server.id, bot.id)),
                    bot.avatar,
                    px(24.),
                )
                .still()
                .into_any_element();
                let mut kind = h_flex().gap_1();
                if let Some(trouble) = trouble {
                    kind = kind.child(format!("{trouble} ·"));
                }
                kind = kind.child("Bot");
                if !scoped {
                    kind = kind
                        .child("·")
                        .child(self.highlighted(&server.name, &hit.server, cx));
                }
                let label = format!(
                    "{}, bot on {}{}",
                    bot.name,
                    server.name,
                    trouble.map(|t| format!(", {t}")).unwrap_or_default()
                );
                (icon, bot.name.as_str(), kind, label)
            }
            Target::Server { .. } => {
                let mut kind = h_flex().gap_1().child("Server");
                if let Some(trouble) = trouble {
                    kind = kind.child(format!("· {trouble}"));
                }
                let label = format!(
                    "{}, server{}",
                    server.name,
                    trouble.map(|t| format!(", {t}")).unwrap_or_default()
                );
                (
                    self.switcher_server_icon(server.id, px(24.), cx),
                    server.name.as_str(),
                    kind,
                    label,
                )
            }
        };
        let dim = if trouble.is_some() { 0.55 } else { 1. };
        let enter = Keystroke::parse("enter")
            .ok()
            .filter(|_| selected)
            .map(|key| Kbd::new(key).into_any_element());
        let tab = matches!(hit.target, Target::Server { .. })
            .then_some(())
            .filter(|_| selected && !scoped)
            .and_then(|_| Keystroke::parse("tab").ok())
            .map(|key| {
                h_flex()
                    .gap_1()
                    .child(Kbd::new(key))
                    .child("Search in")
                    .into_any_element()
            });
        Some(
            h_flex()
                .id(SharedString::from(format!("switcher-row-{ix}")))
                .role(Role::ListBoxOption)
                .aria_label(label)
                .aria_selected(selected)
                .w_full()
                .h(px(ROW))
                .px_2()
                .gap_3()
                .items_center()
                .rounded(theme.radius)
                .cursor_pointer()
                .when(selected, |row| row.bg(theme.accent))
                .on_mouse_move(cx.listener(move |shell, _, window, cx| {
                    if let Some(switcher) = &mut shell.switcher
                        && switcher.picker.select(ix)
                    {
                        shell.preload_highlighted(window, cx);
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |shell, _, window, cx| {
                    let target = shell
                        .switcher
                        .as_ref()
                        .and_then(|s| s.picker.hits.get(ix))
                        .map(|hit| hit.target);
                    if let Some(target) = target {
                        shell.open_switcher_target(target, window, cx);
                    }
                }))
                .child(div().flex_none().opacity(dim).child(icon))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .opacity(dim)
                        .child(self.highlighted(name, &hit.name, cx)),
                )
                .child(
                    h_flex()
                        .flex_none()
                        .gap_2()
                        .items_center()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(kind)
                        .children(tab)
                        .children(enter),
                )
                .into_any_element(),
        )
    }

    /// `text` with the chars at `at` drawn bold and bright.
    fn highlighted(&self, text: &str, at: &[usize], cx: &App) -> StyledText {
        let strong = HighlightStyle {
            color: Some(tone(Tone::Strong, cx)),
            font_weight: Some(FontWeight::SEMIBOLD),
            ..Default::default()
        };
        let mut ranges: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        for (ix, (byte, c)) in text.char_indices().enumerate() {
            if !at.contains(&ix) {
                continue;
            }
            let end = byte + c.len_utf8();
            match ranges.last_mut() {
                Some((range, _)) if range.end == byte => range.end = end,
                _ => ranges.push((byte..end, strong)),
            }
        }
        StyledText::new(text.to_owned()).with_highlights(ranges)
    }

    fn switcher_server_icon(&self, id: Uuid, size: Pixels, cx: &App) -> AnyElement {
        let Some(saved) = self.registry.find(id) else {
            return div().size(size).into_any_element();
        };
        crate::server_icon::render(
            self.rail.icons.get(id),
            &saved.name,
            self.managed_server(&saved.profile),
            size,
            cx,
        )
    }
}
