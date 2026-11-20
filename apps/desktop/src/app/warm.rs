//! Conversations kept after switching away, so switching back to a bot
//! draws it at once and catches up from where it left off.

use std::collections::VecDeque;
use std::time::Duration;

use gpui_kit::*;
use uuid::Uuid;

use super::ElseApp;
use super::conversation::{ConversationView, Load};
use crate::api::LiveCommand;

/// How many conversations a server keeps besides the shown one.
const WARM_VIEWS: usize = 8;
/// How long the pointer rests on a bot before its conversation preloads.
const PRELOAD_DWELL: Duration = Duration::from_millis(200);

/// Views by bot, the most recently kept first.
pub(super) struct Warm<V> {
    views: VecDeque<(Uuid, V)>,
    capacity: usize,
}

impl<V> Default for Warm<V> {
    fn default() -> Self {
        Self {
            views: VecDeque::new(),
            capacity: WARM_VIEWS,
        }
    }
}

impl<V> Warm<V> {
    /// Keeps `view` first, dropping the least recently kept beyond capacity.
    pub(super) fn keep(&mut self, bot: Uuid, view: V) {
        self.take(bot);
        self.views.push_front((bot, view));
        self.views.truncate(self.capacity);
    }

    pub(super) fn take(&mut self, bot: Uuid) -> Option<V> {
        let ix = self.views.iter().position(|(id, _)| *id == bot)?;
        self.views.remove(ix).map(|(_, view)| view)
    }

    pub(super) fn get(&self, bot: Uuid) -> Option<&V> {
        self.views
            .iter()
            .find(|(id, _)| *id == bot)
            .map(|(_, view)| view)
    }

    pub(super) fn get_mut(&mut self, bot: Uuid) -> Option<&mut V> {
        self.views
            .iter_mut()
            .find(|(id, _)| *id == bot)
            .map(|(_, view)| view)
    }

    pub(super) fn for_each(&mut self, f: impl FnMut(&mut V)) {
        self.views.iter_mut().map(|(_, view)| view).for_each(f);
    }

    pub(super) fn contains(&self, bot: Uuid) -> bool {
        self.views.iter().any(|(id, _)| *id == bot)
    }

    pub(super) fn retain(&mut self, keep: impl Fn(Uuid) -> bool) {
        self.views.retain(|(id, _)| keep(*id));
    }
}

/// What a view needs to catch up once shown.
#[derive(Debug, PartialEq)]
pub(super) enum Resume {
    /// Follow the live stream from this cursor.
    Follow(String),
    /// Its load is still running and subscribes when it lands.
    Wait,
    /// Load its newest page.
    Load,
}

impl ConversationView {
    pub(super) fn resume(&self) -> Resume {
        match (&self.load, &self.conversation) {
            (Load::Running { .. }, _) => Resume::Wait,
            (Load::Settled, Some(conversation)) if !conversation.is_saved() => {
                Resume::Follow(conversation.cursor())
            }
            _ => Resume::Load,
        }
    }
}

impl ElseApp {
    pub(super) fn shows(&self, bot: Uuid) -> bool {
        self.view.as_ref().is_some_and(|view| view.bot_id == bot)
    }

    pub(super) fn view_of(&mut self, bot: Uuid) -> Option<&mut ConversationView> {
        match self.view.as_mut() {
            Some(view) if view.bot_id == bot => Some(view),
            _ => self.warm.get_mut(bot),
        }
    }

    pub(super) fn swap_view(&mut self, bot: Option<Uuid>, cx: &mut Context<Self>) {
        // Taken before parking the shown view, which could evict it.
        let next = bot.map(|bot| {
            self.warm
                .take(bot)
                .unwrap_or_else(|| self.new_view(bot, cx))
        });
        if let Some(mut old) = std::mem::replace(&mut self.view, next) {
            self.live_command(LiveCommand::Unsubscribe { bot: old.bot_id });
            old.park();
            self.warm.keep(old.bot_id, old);
        }
    }

    fn new_view(&self, bot: Uuid, cx: &mut Context<Self>) -> ConversationView {
        let view = ConversationView::new(bot);
        let this = cx.entity().downgrade();
        view.list.set_scroll_handler(move |_, window, cx| {
            let _ = this.update(cx, |app, cx| {
                // The owner scrolled, so an exchange still laying out
                // no longer moves the view when it lands.
                if let Some(view) = app.view.as_mut() {
                    view.landing = None;
                }
                app.check_reading(window, cx)
            });
        });
        view
    }

    pub(super) fn resume_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_ref() else {
            return;
        };
        let bot = view.bot_id;
        match view.resume() {
            Resume::Follow(cursor) => self.live_command(LiveCommand::Subscribe {
                bot: view.bot_id,
                cursor,
            }),
            Resume::Wait => {}
            Resume::Load => self.load_conversation(window, cx),
        }
        self.ensure_rules(bot, window, cx);
        self.resume_find(window, cx);
    }

    /// Preloads `bot`'s conversation once the pointer or the quick
    /// switcher's highlight rests on it.
    pub(crate) fn dwell_on(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let dwell = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(PRELOAD_DWELL).await;
            let _ = this.update_in(cx, |app, window, cx| app.preload(bot, window, cx));
        });
        self._preload = Some((bot, dwell));
    }

    /// Stops a preload of `bot` still waiting out its dwell.
    pub(crate) fn leave(&mut self, bot: Uuid) {
        // Entering the next row can arrive before leaving this one.
        if self._preload.as_ref().is_some_and(|(id, _)| *id == bot) {
            self._preload = None;
        }
    }

    fn preload(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if self.shows(bot) || self.warm.contains(bot) || !self.active_bots().any(|b| b.id == bot) {
            return;
        }
        let view = self.new_view(bot, cx);
        self.warm.keep(bot, view);
        self.load_into(bot, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{ConversationView, Load, Resume, Warm};
    use crate::model::Conversation;
    use gpui_kit::Task;
    use uuid::Uuid;

    fn ids(warm: &Warm<u8>) -> Vec<u128> {
        warm.views.iter().map(|(id, _)| id.as_u128()).collect()
    }

    #[test]
    fn keeping_past_capacity_drops_the_least_recently_shown() {
        let mut warm = Warm::default();
        for n in 1..=8 {
            warm.keep(Uuid::from_u128(n), n as u8);
        }
        warm.keep(Uuid::from_u128(1), 1);
        warm.keep(Uuid::from_u128(9), 9);
        assert_eq!(ids(&warm), [9, 1, 8, 7, 6, 5, 4, 3]);
        assert_eq!(warm.take(Uuid::from_u128(2)), None, "2 was evicted");
        assert_eq!(warm.take(Uuid::from_u128(8)), Some(8));
        assert!(!warm.contains(Uuid::from_u128(8)));
    }

    fn page(applied: i64) -> crate::model::Snapshot {
        serde_json::from_value(serde_json::json!({
            "event_cursor": format!("server:bot:{applied}"),
            "items": [], "turns": [],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
        }))
        .unwrap()
    }

    #[test]
    fn a_view_shown_again_follows_from_its_cursor_unless_it_needs_a_load() {
        let mut view = ConversationView::new(Uuid::from_u128(1));
        assert_eq!(view.resume(), Resume::Load, "nothing loaded yet");
        view.load = Load::Running {
            _task: Task::ready(()),
        };
        assert_eq!(
            view.resume(),
            Resume::Wait,
            "its load subscribes on landing"
        );
        view.load = Load::Settled;
        view.conversation = Some(Conversation::from_snapshot(page(42)).unwrap());
        assert_eq!(view.resume(), Resume::Follow("server:bot:42".into()));
        view.load = Load::Failed { _retry: None };
        assert_eq!(
            view.resume(),
            Resume::Load,
            "a failed load or a reset reloads"
        );
        view.load = Load::Settled;
        view.conversation = Some(Conversation::from_saved(page(42)).unwrap());
        assert_eq!(view.resume(), Resume::Load, "a saved page is replaced");
    }
}
