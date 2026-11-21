//! Conversations kept after switching away, so switching back to a bot
//! draws it at once and catches up from where it left off.

use std::collections::VecDeque;
use std::time::Duration;

use gpui_kit::*;
use uuid::Uuid;

use super::ElseApp;
use super::conversation::{ConversationView, Load};
use crate::api::LiveCommand;
use crate::model::Conversation;

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

    pub(super) fn iter(&self) -> impl Iterator<Item = &V> {
        self.views.iter().map(|(_, view)| view)
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
        resume(&self.load, self.conversation.as_ref())
    }
}

fn resume(load: &Load, conversation: Option<&Conversation>) -> Resume {
    match (load, conversation) {
        (Load::Running { .. }, _) => Resume::Wait,
        (Load::Settled, Some(conversation)) if !conversation.is_saved() => {
            Resume::Follow(conversation.cursor())
        }
        _ => Resume::Load,
    }
}

impl ElseApp {
    pub(super) fn shows(&self, bot: Uuid, cx: &App) -> bool {
        self.shown_bot(cx) == Some(bot)
    }

    pub(super) fn shown_bot(&self, cx: &App) -> Option<Uuid> {
        self.view.as_ref().map(|view| view.read(cx).bot_id)
    }

    pub(super) fn shown<'a>(&self, cx: &'a App) -> Option<&'a ConversationView> {
        self.view.as_ref().map(|view| view.read(cx))
    }

    pub(super) fn update_view<R>(
        &self,
        cx: &mut App,
        f: impl FnOnce(&mut ConversationView, &mut Context<ConversationView>) -> R,
    ) -> Option<R> {
        self.view.as_ref().map(|view| {
            view.update(cx, |view, cx| {
                let result = f(view, cx);
                cx.notify();
                result
            })
        })
    }

    pub(super) fn view_of(&self, bot: Uuid, cx: &App) -> Option<Entity<ConversationView>> {
        match &self.view {
            Some(view) if view.read(cx).bot_id == bot => Some(view.clone()),
            _ => self.warm.get(bot).cloned(),
        }
    }

    pub(super) fn swap_view(
        &mut self,
        bot: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Taken before parking the shown view, which could evict it.
        let next = bot.map(|bot| {
            self.warm
                .take(bot)
                .unwrap_or_else(|| self.new_view(bot, cx))
        });
        if let Some(next) = &next {
            next.update(cx, |view, _| view.show());
        }
        if let Some(old) = std::mem::replace(&mut self.view, next) {
            let bot = old.update(cx, |view, cx| {
                view.clear_secret_forms(window, cx);
                view.clear_social_forms(window, cx);
                view.park();
                view.bot_id
            });
            self.live_command(LiveCommand::Unsubscribe { bot });
            self.warm.keep(bot, old);
        }
    }

    fn new_view(&self, bot: Uuid, cx: &mut Context<Self>) -> Entity<ConversationView> {
        let (app, client) = (cx.entity().downgrade(), self.client.clone());
        let inputs = self.transcript_inputs(bot);
        cx.new(|cx| ConversationView::new(bot, app, client, inputs, cx))
    }

    pub(super) fn resume_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.shown(cx) else {
            return;
        };
        let bot = view.bot_id;
        match view.resume() {
            Resume::Follow(cursor) => self.live_command(LiveCommand::Subscribe { bot, cursor }),
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
        if self.shows(bot, cx)
            || self.warm.contains(bot)
            || !self.active_bots().any(|b| b.id == bot)
        {
            return;
        }
        let view = self.new_view(bot, cx);
        self.warm.keep(bot, view);
        self.load_into(bot, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{Load, Resume, Warm, resume};
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

    fn page(applied: i64) -> else_wire::conversation::Snapshot {
        serde_json::from_value(serde_json::json!({
            "event_cursor": format!("server:bot:{applied}"),
            "items": [], "turns": [],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
        }))
        .unwrap()
    }

    #[test]
    fn a_view_shown_again_follows_from_its_cursor_unless_it_needs_a_load() {
        assert_eq!(
            resume(&Load::Settled, None),
            Resume::Load,
            "nothing loaded yet"
        );
        let running = Load::Running {
            _task: Task::ready(()),
        };
        assert_eq!(
            resume(&running, None),
            Resume::Wait,
            "its load subscribes on landing"
        );
        let live = Conversation::from_snapshot(page(42)).unwrap();
        assert_eq!(
            resume(&Load::Settled, Some(&live)),
            Resume::Follow("server:bot:42".into())
        );
        assert_eq!(
            resume(&Load::Failed { _retry: None }, Some(&live)),
            Resume::Load,
            "a failed load or a reset reloads"
        );
        let saved = Conversation::from_saved(page(42)).unwrap();
        assert_eq!(
            resume(&Load::Settled, Some(&saved)),
            Resume::Load,
            "a saved page is replaced"
        );
    }
}
