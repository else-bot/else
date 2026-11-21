//! Loading each bot's conversation, shown or warm, and the conversations
//! kept after switching away, so switching back to a bot draws it at once
//! and catches up from where it left off.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use else_wire::bots::Phase;
use else_wire::conversation::Snapshot;
use gpui_kit::*;
use uuid::Uuid;

use super::ElseApp;
use super::conversation::{ConversationView, Resume};
use crate::api::{ClientError, LiveCommand, StreamMessage};
use crate::model::{Applied, Conversation};

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
        self.update_view(cx, |view, cx| view.resume_find(window, cx));
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

impl ElseApp {
    pub(super) fn load_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bot) = self.shown_bot(cx) {
            self.load_into(bot, window, cx);
        }
    }

    /// Loads `bot`'s newest page into its view, shown or warm. A view with
    /// nothing to draw shows the saved page while the server's loads.
    pub(super) fn load_into(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let (client, saved) = (self.client.clone(), self.saved.clone());
        let Some(view) = self.view_of(bot, cx) else {
            return;
        };
        let cold = view.read(cx).conversation.is_none();
        let task = cx.spawn_in(window, async move |this, cx| {
            let fetch = cx.background_executor().spawn(client.latest(bot));
            if cold
                && let Some(saved) = saved.clone()
                && let Some(page) = cx
                    .background_executor()
                    .spawn(async move { saved.load(bot) })
                    .await
            {
                let _ =
                    this.update_in(cx, |app, window, cx| app.apply_saved(bot, page, window, cx));
            }
            let snapshot = fetch.await.map(|crate::api::Latest { snapshot, raw }| {
                if let Some(saved) = saved {
                    let ticket = saved.ticket();
                    cx.background_executor()
                        .spawn(async move {
                            if let Err(err) = saved.save(bot, &raw, ticket) {
                                eprintln!("{err}");
                            }
                        })
                        .detach();
                }
                snapshot
            });
            let _ = this.update_in(cx, |app, window, cx| {
                if let Some(cursor) = app.apply_snapshot(bot, snapshot, window, cx) {
                    app.live_command(LiveCommand::Subscribe { bot, cursor });
                }
            });
        });
        view.update(cx, |view, _| view.begin_load(task));
    }

    /// Shows the saved page of a view still waiting for its first one.
    fn apply_saved(
        &mut self,
        bot: Uuid,
        page: Snapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = self.shows(bot, cx);
        let (Some(view), Ok(conversation)) =
            (self.view_of(bot, cx), Conversation::from_saved(page))
        else {
            return;
        };
        if view.update(cx, |view, cx| view.show_saved(conversation, cx)) && shown {
            self.sync_rows(window, cx);
        }
    }

    /// Applies `bot`'s newest page to its view, and returns the cursor to
    /// follow if the view is shown.
    fn apply_snapshot(
        &mut self,
        bot: Uuid,
        snapshot: Result<Snapshot, ClientError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let shown = self.shows(bot, cx);
        let view = self.view_of(bot, cx)?;
        let failed = |problem: String, retry: Option<Task<()>>, cx: &mut Context<Self>| {
            view.update(cx, |view, cx| view.fail(problem, retry, cx));
            cx.notify();
        };
        match snapshot.map(Conversation::from_snapshot) {
            Ok(Ok(conversation)) => {
                let cursor = conversation.cursor();
                view.update(cx, |view, cx| view.settle(conversation, cx));
                if !shown {
                    return None;
                }
                self.sync_rows(window, cx);
                self.ensure_rules(bot, window, cx);
                Some(cursor)
            }
            Ok(Err(_)) => {
                failed("The server sent an invalid snapshot.".into(), None, cx);
                None
            }
            Err(ClientError::Api { status: 410, .. }) => {
                view.update(cx, |view, _| view.expire());
                self.refresh_now(window, cx);
                None
            }
            Err(err) => {
                let retry = shown.then(|| {
                    cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor().timer(Duration::from_secs(2)).await;
                        let _ = this.update_in(cx, |app, window, cx| {
                            if app.shows(bot, cx) {
                                app.load_conversation(window, cx);
                            }
                        });
                    })
                });
                failed(format!("Could not load history: {err}"), retry, cx);
                None
            }
        }
    }

    pub(super) fn apply_stream(
        &mut self,
        bot: Uuid,
        message: StreamMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A warm view keeps applying what arrives until its unsubscribe
        // lands, so the cursor it resumes from is the one it applied.
        let shown = self.shows(bot, cx);
        let Some(view) = self.view_of(bot, cx) else {
            return;
        };
        let failed = match message {
            StreamMessage::Frame(frame) => {
                let Some(applied) = view.update(cx, |view, cx| view.apply_frame(&frame, cx)) else {
                    return;
                };
                match applied {
                    Ok((Applied::Duplicate, _)) => false,
                    // The bot list stays authoritative for a warm bot's
                    // activity: its last frames can trail the list.
                    Ok((Applied::Changed, _)) if !shown => {
                        self.prune_pending(bot, cx);
                        false
                    }
                    Ok((Applied::Changed, activity)) => {
                        if activity.as_ref().is_some_and(|a| a.phase == Phase::Idle) {
                            self.stopping.remove(&bot);
                        }
                        if let Some(entry) = Arc::make_mut(&mut self.bots)
                            .iter_mut()
                            .find(|b| b.id == bot)
                            && activity.is_some()
                        {
                            entry.activity = activity;
                        }
                        self.sync_rows(window, cx);
                        self.ensure_rules(bot, window, cx);
                        false
                    }
                    Err(_) => true,
                }
            }
            StreamMessage::Reset => true,
            StreamMessage::Deleted => {
                self.refresh_now(window, cx);
                false
            }
        };
        if !failed {
            return;
        }
        // Dropped so the reloaded page's cursor subscribes anew, even when
        // it equals the one asked for before.
        self.live_command(LiveCommand::Unsubscribe { bot });
        if shown {
            self.load_conversation(window, cx);
        } else {
            view.update(cx, |view, _| view.break_stream());
        }
    }

    /// Drops `bot`'s pending sends its conversation now holds.
    pub(super) fn prune_pending(&mut self, bot: Uuid, cx: &App) {
        let Some(view) = self.view_of(bot, cx) else {
            return;
        };
        if let (Some(conversation), Some(pending)) = (
            view.read(cx).conversation.as_ref(),
            self.pending.get_mut(&bot),
        ) {
            pending.retain(|p| !conversation.has_request(p.request_id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Warm;
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
}
