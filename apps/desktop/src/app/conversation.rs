//! Transcript loading, committed events, paging, and row reconciliation.

use super::ElseApp;
use crate::api::{ClientError, LiveCommand, StreamMessage};
use crate::model::{Applied, Conversation, Snapshot};
use gpui_kit::component::input::TextareaState;
use gpui_kit::*;
use std::collections::HashMap;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum RowKey {
    Item(Uuid),
    Pending(Uuid),
}

/// The selected bot's transcript and its live subscription.
pub(super) struct ConversationView {
    pub(super) bot_id: Uuid,
    pub(super) conversation: Option<Conversation>,
    pub(super) stream_problem: Option<String>,
    pub(super) loading_older: bool,
    pub(super) rows: Vec<RowKey>,
    pub(super) list: ListState,
    pub(super) texts: HashMap<RowKey, (Entity<TextareaState>, String)>,
    pub(super) _load: Option<Task<()>>,
    pub(super) _older: Option<Task<()>>,
}

impl ConversationView {
    pub(super) fn new(bot_id: Uuid) -> Self {
        let list = ListState::new(0, ListAlignment::Bottom, px(400.));
        list.set_follow_mode(FollowMode::Tail);
        Self {
            bot_id,
            conversation: None,
            stream_problem: None,
            loading_older: false,
            rows: Vec::new(),
            list,
            texts: HashMap::new(),
            _load: None,
            _older: None,
        }
    }
}

impl ElseApp {
    pub(super) fn load_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(view)) = (self.client.clone(), self.view.as_mut()) else {
            return;
        };
        let bot = view.bot_id;
        view._load = Some(cx.spawn_in(window, async move |this, cx| {
            let snapshot = client.snapshot(bot, None).await;
            let _ = this.update_in(cx, |app, window, cx| {
                if let Some(cursor) = app.apply_snapshot(bot, snapshot, window, cx) {
                    app.live_command(LiveCommand::Subscribe { bot, cursor });
                }
            });
        }));
    }

    /// Returns the cursor to subscribe from.
    fn apply_snapshot(
        &mut self,
        bot: Uuid,
        snapshot: Result<Snapshot, ClientError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let view = self.view.as_mut().filter(|v| v.bot_id == bot)?;
        match snapshot.map(Conversation::from_snapshot) {
            Ok(Ok(conversation)) => {
                let cursor = conversation.cursor();
                view.conversation = Some(conversation);
                view.stream_problem = None;
                self.sync_rows(window, cx);
                Some(cursor)
            }
            Ok(Err(_)) => {
                view.stream_problem = Some("The server sent an invalid snapshot.".into());
                cx.notify();
                None
            }
            Err(ClientError::Api { status: 410, .. }) => {
                self.refresh_now(window, cx);
                None
            }
            Err(err) => {
                view.stream_problem = Some(format!("Could not load history: {err}"));
                cx.notify();
                // Retry shortly; the poll loop also keeps the list current.
                let retry = cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    let _ = this.update_in(cx, |app, window, cx| {
                        if app.view.as_ref().is_some_and(|v| v.bot_id == bot) {
                            app.load_conversation(window, cx);
                        }
                    });
                });
                retry.detach();
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
        // Events for a conversation no longer open are ignored.
        let Some(view) = self.view.as_mut().filter(|v| v.bot_id == bot) else {
            return;
        };
        match message {
            StreamMessage::Frame(frame) => {
                let Some(conversation) = view.conversation.as_mut() else {
                    return;
                };
                match conversation.apply(&frame) {
                    Ok(Applied::Duplicate) => {}
                    Ok(Applied::Changed) => {
                        // The channel's activity is fresher than the list.
                        let activity = conversation.activity.clone();
                        if activity.as_ref().is_some_and(|a| a.phase == "idle") {
                            self.stopping.remove(&bot);
                        }
                        if let Some(entry) = self.bots.iter_mut().find(|b| b.id == bot) {
                            if activity.is_some() {
                                entry.activity = activity;
                            }
                        }
                        self.sync_rows(window, cx);
                    }
                    Err(_) => {
                        // Drop this subscription and reload a snapshot.
                        self.live_command(LiveCommand::Unsubscribe { bot });
                        self.load_conversation(window, cx);
                    }
                }
            }
            StreamMessage::Reset => self.load_conversation(window, cx),
            StreamMessage::Deleted => self.refresh_now(window, cx),
        }
    }

    pub(super) fn load_older(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let Some(before) = view.conversation.as_ref().and_then(|c| c.older_before_seq) else {
            return;
        };
        if view.loading_older {
            return;
        }
        view.loading_older = true;
        let bot = view.bot_id;
        view._older = Some(cx.spawn_in(window, async move |this, cx| {
            let page = client.snapshot(bot, Some(before)).await;
            let _ = this.update_in(cx, |app, window, cx| {
                let Some(view) = app.view.as_mut().filter(|v| v.bot_id == bot) else {
                    return;
                };
                view.loading_older = false;
                match page {
                    Ok(page) => {
                        if let Some(conversation) = view.conversation.as_mut() {
                            conversation.merge_older(page);
                        }
                        app.sync_rows(window, cx);
                    }
                    Err(err) => {
                        app.notice = Some(format!("Could not load older messages: {err}"));
                        cx.notify();
                    }
                }
            });
        }));
        cx.notify();
    }

    /// Rebuild the row list from the model and pending sends, splicing only
    /// what changed so scroll position and measurements are kept.
    pub(super) fn sync_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let bot = view.bot_id;
        // Pending sends are shown even before history has loaded.
        let conversation = view.conversation.as_ref();
        // A pending send disappears once its canonical item is present.
        if let (Some(pending), Some(conversation)) = (self.pending.get_mut(&bot), conversation) {
            pending.retain(|p| !conversation.has_request(p.request_id));
        }
        let mut rows = Vec::new();
        let mut texts: Vec<(RowKey, String)> = Vec::new();
        for item in conversation.into_iter().flat_map(|c| c.items()) {
            rows.push(RowKey::Item(item.id));
            texts.push((RowKey::Item(item.id), item.text.clone()));
        }
        for pending in self.pending.get(&bot).into_iter().flatten() {
            rows.push(RowKey::Pending(pending.request_id));
            texts.push((RowKey::Pending(pending.request_id), pending.text.clone()));
        }
        let old = std::mem::take(&mut view.rows);
        let prefix = old.iter().zip(&rows).take_while(|(a, b)| a == b).count();
        let max_suffix = old.len().min(rows.len()) - prefix;
        let suffix = old
            .iter()
            .rev()
            .zip(rows.iter().rev())
            .take(max_suffix)
            .take_while(|(a, b)| a == b)
            .count();
        if prefix + suffix != old.len() || old.len() != rows.len() {
            view.list
                .splice(prefix..old.len() - suffix, rows.len() - prefix - suffix);
        }
        // Create or update the selectable text of each row.
        let mut changed = Vec::new();
        let mut created = false;
        for (key, text) in texts {
            match view.texts.get_mut(&key) {
                Some((state, current)) if *current != text => {
                    state.update(cx, |state, cx| state.set_value(text.clone(), window, cx));
                    *current = text;
                    changed.push(key);
                }
                Some(_) => {}
                None => {
                    let state = cx.new(|cx| {
                        TextareaState::new(window, cx)
                            .auto_grow(1, 100_000)
                            .default_value(text.clone())
                    });
                    view.texts.insert(key, (state, text));
                    created = true;
                }
            }
        }
        view.texts.retain(|key, _| rows.contains(key));
        if created {
            // New text views learn their wrapped height during their first
            // layout; measure the rows again once that has happened.
            let list = view.list.clone();
            window.on_next_frame(move |window, _| {
                let list = list.clone();
                window.on_next_frame(move |window, _| {
                    list.remeasure();
                    window.refresh();
                });
            });
        }
        for key in changed {
            if let Some(ix) = rows.iter().position(|r| *r == key) {
                view.list.remeasure_items(ix..ix + 1);
            }
        }
        view.rows = rows;
        cx.notify();
    }

    // ----- Sending ----------------------------------------------------------
}
