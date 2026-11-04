//! Transcript loading, committed events, paging, and row reconciliation.

use super::ElseApp;
use super::reply::Author;
use super::row_text::{RowFormat, RowText};
use crate::api::{ClientError, LiveCommand, StreamMessage};
use crate::model::{Applied, Conversation, Snapshot};
use chrono::{DateTime, Local, TimeDelta, TimeZone, Utc};
use gpui_kit::*;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum RowKey {
    Item(Uuid),
    Pending(Uuid),
    /// A bot's request for credentials, by request ID.
    SecretRequest(Uuid),
    /// A bot's question or approval request, by question ID.
    Question(Uuid),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TranscriptRow {
    pub(super) key: RowKey,
    pub(super) footer_stamp: Option<DateTime<Utc>>,
    state: Option<String>,
}

const BURST_GAP: TimeDelta = TimeDelta::minutes(5);

fn burst_tail_stamps<Tz: TimeZone>(
    rows: &[(Option<Author>, Option<DateTime<Utc>>)],
    zone: &Tz,
) -> Vec<Option<DateTime<Utc>>> {
    let joins = |(author, at): (Option<Author>, Option<DateTime<Utc>>),
                 (next_author, next_at): (Option<Author>, Option<DateTime<Utc>>)| {
        let (Some(author), Some(at), Some(next_at)) = (author, at, next_at) else {
            return false;
        };
        let gap = next_at - at;
        next_author == Some(author)
            && gap >= TimeDelta::zero()
            && gap < BURST_GAP
            && at.with_timezone(zone).date_naive() == next_at.with_timezone(zone).date_naive()
    };
    rows.iter()
        .enumerate()
        .map(|(ix, &row)| {
            let joined = rows.get(ix + 1).is_some_and(|&next| joins(row, next));
            if joined { None } else { row.1 }
        })
        .collect()
}

#[derive(Clone, Copy)]
pub(super) struct ReplyLayout {
    pub position: crate::api::ReplyPosition,
    pub bounds: Bounds<Pixels>,
}

/// The selected bot's transcript and its live subscription.
pub(super) struct ConversationView {
    pub(super) bot_id: Uuid,
    pub(super) conversation: Option<Conversation>,
    pub(super) stream_problem: Option<String>,
    pub(super) loading_older: bool,
    pub(super) rows: Vec<TranscriptRow>,
    pub(super) list: ListState,
    pub(super) texts: HashMap<RowKey, RowText>,
    pub(super) secret_forms: HashMap<Uuid, super::secret_card::SecretForm>,
    pub(super) answered: HashSet<Uuid>,
    /// An answer in flight, or its failure, per pending question.
    pub(super) question_submits: HashMap<Uuid, super::question_card::QuestionSubmit>,
    /// The end of the latest loaded reply was on screen at the last check.
    pub(super) latest_seen: bool,
    pub(super) reply_layout: Option<ReplyLayout>,
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
            secret_forms: HashMap::new(),
            answered: HashSet::new(),
            question_submits: HashMap::new(),
            latest_seen: false,
            reply_layout: None,
            _load: None,
            _older: None,
        }
    }
}

impl ElseApp {
    pub(super) fn remeasure_row(&self, key: RowKey, cx: &mut Context<Self>) {
        if let Some(view) = self.view.as_ref()
            && let Some(ix) = view.rows.iter().position(|row| row.key == key)
        {
            view.list.remeasure_items(ix..ix + 1);
        }
        cx.notify();
    }

    pub(super) fn load_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (client, Some(view)) = (self.client.clone(), self.view.as_mut()) else {
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
        let client = self.client.clone();
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
        let mut texts = Vec::new();
        let mut requests: Vec<(RowKey, Option<Uuid>)> = conversation
            .into_iter()
            .flat_map(|c| {
                c.turns_in_order().into_iter().flat_map(move |turn| {
                    turn.secret_requests
                        .iter()
                        .map(move |request| {
                            (
                                RowKey::SecretRequest(request.id),
                                c.question_anchor(turn.id, None),
                            )
                        })
                        .chain(turn.questions.iter().map(move |question| {
                            (
                                RowKey::Question(question.id),
                                c.question_anchor(turn.id, question.settled_at),
                            )
                        }))
                })
            })
            .collect();
        let items: Vec<_> = conversation.into_iter().flat_map(|c| c.items()).collect();
        for item in &items {
            rows.push(RowKey::Item(item.id));
            let format = if item.kind == "assistant" || item.bot_reply.is_some() {
                RowFormat::Markdown
            } else {
                RowFormat::Plain
            };
            texts.push((
                RowKey::Item(item.id),
                item.bot_task
                    .as_ref()
                    .map_or_else(|| item.text.clone(), |task| task.prompt.clone()),
                format,
            ));
            requests.retain(|(key, anchor)| {
                let here = *anchor == Some(item.id);
                if here {
                    rows.push(*key);
                }
                !here
            });
        }
        rows.extend(requests.into_iter().map(|(key, _)| key));
        for pending in self.pending.get(&bot).into_iter().flatten() {
            rows.push(RowKey::Pending(pending.request_id));
            texts.push((
                RowKey::Pending(pending.request_id),
                pending.text.clone(),
                RowFormat::Plain,
            ));
        }
        let pending = self.pending.get(&bot);
        let senders: Vec<_> = rows
            .iter()
            .map(|key| match *key {
                RowKey::Item(id) => {
                    conversation
                        .and_then(|c| c.item(id))
                        .map_or((None, None), |item| {
                            let author = Author::of(&item.kind).filter(|_| item.reaction.is_none());
                            (author, item.created_at)
                        })
                }
                RowKey::Pending(request) => pending
                    .and_then(|list| list.iter().find(|p| p.request_id == request))
                    .map_or((None, None), |p| {
                        let author =
                            (p.rejected.is_none() && p.reaction.is_none()).then_some(Author::Owner);
                        (author, Some(p.sent_at))
                    }),
                RowKey::SecretRequest(_) | RowKey::Question(_) => (None, None),
            })
            .collect();
        let rows: Vec<TranscriptRow> = rows
            .into_iter()
            .zip(burst_tail_stamps(&senders, &Local))
            .map(|(key, footer_stamp)| {
                let state = conversation.and_then(|c| match key {
                    RowKey::Question(id) => c.question(id).map(|q| q.status.clone()),
                    RowKey::SecretRequest(id) => c.secret_request(id).map(|r| r.status.clone()),
                    RowKey::Item(id) => c
                        .item(id)
                        .and_then(|i| c.turn(i.turn_id))
                        .map(|t| t.status.clone()),
                    RowKey::Pending(_) => None,
                });
                TranscriptRow {
                    key,
                    footer_stamp,
                    state,
                }
            })
            .collect();
        let old = std::mem::take(&mut view.rows);
        let same = |a: &TranscriptRow, b: &TranscriptRow| a.key == b.key;
        let prefix = old
            .iter()
            .zip(&rows)
            .take_while(|(a, b)| same(a, b))
            .count();
        let max_suffix = old.len().min(rows.len()) - prefix;
        let suffix = old
            .iter()
            .rev()
            .zip(rows.iter().rev())
            .take(max_suffix)
            .take_while(|(a, b)| same(a, b))
            .count();
        if prefix + suffix != old.len() || old.len() != rows.len() {
            view.list
                .splice(prefix..old.len() - suffix, rows.len() - prefix - suffix);
        }
        let previous: HashMap<_, _> = old.iter().map(|row| (row.key, row)).collect();
        for (ix, row) in rows.iter().enumerate() {
            if previous
                .get(&row.key)
                .is_some_and(|old| old.footer_stamp != row.footer_stamp || old.state != row.state)
            {
                view.list.remeasure_items(ix..ix + 1);
            }
        }
        // Create or update the selectable text of each row.
        let mut changed = Vec::new();
        let mut created = false;
        for (key, text, format) in texts {
            match view.texts.get_mut(&key) {
                Some(row) if row.format() == format => {
                    if row.sync(text, &self.bots, window, cx) {
                        changed.push(key);
                    }
                }
                _ => {
                    view.texts
                        .insert(key, RowText::new(key, text, format, &self.bots, window, cx));
                    created = true;
                }
            }
        }
        view.texts
            .retain(|key, _| rows.iter().any(|row| row.key == *key));
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
            if let Some(ix) = rows.iter().position(|r| r.key == key) {
                view.list.remeasure_items(ix..ix + 1);
            }
        }
        view.rows = rows;
        self.sync_secret_forms(window, cx);
        self.sync_question_submits();
        self.check_reading(window, cx);
        cx.notify();
    }

    // ----- Sending ----------------------------------------------------------
}

#[cfg(test)]
mod tests {
    use super::burst_tail_stamps;
    use crate::app::reply::Author::{self, Bot, Owner};
    use chrono::{DateTime, FixedOffset, Utc};

    fn at(text: &str) -> DateTime<Utc> {
        format!("2026-11-04T{text}Z").parse().unwrap()
    }

    fn stamps(rows: &[(Option<Author>, Option<&str>)]) -> Vec<Option<DateTime<Utc>>> {
        let rows: Vec<_> = rows.iter().map(|&(a, t)| (a, t.map(at))).collect();
        burst_tail_stamps(&rows, &Utc)
    }

    #[test]
    fn a_burst_shares_the_stamp_of_its_last_message() {
        let burst = [
            (Some(Bot), Some("10:00:00")),
            (Some(Bot), Some("10:04:00")),
            (Some(Bot), Some("10:08:59")),
            (Some(Bot), Some("10:08:59")),
        ];
        assert_eq!(stamps(&burst), [None, None, None, Some(at("10:08:59"))]);
    }

    #[test]
    fn a_five_minute_gap_or_another_sender_starts_a_new_stamp() {
        let rows = [
            (Some(Bot), Some("10:00:00")),
            (Some(Bot), Some("10:05:00")),
            (Some(Owner), Some("10:05:01")),
            (Some(Bot), Some("10:05:02")),
        ];
        assert_eq!(
            stamps(&rows),
            [
                Some(at("10:00:00")),
                Some(at("10:05:00")),
                Some(at("10:05:01")),
                Some(at("10:05:02")),
            ]
        );
    }

    #[test]
    fn missing_or_reversed_times_break_a_burst() {
        let rows = [
            (Some(Bot), None),
            (Some(Bot), Some("10:01:00")),
            (Some(Bot), Some("10:00:00")),
        ];
        assert_eq!(
            stamps(&rows),
            [None, Some(at("10:01:00")), Some(at("10:00:00"))]
        );
    }

    #[test]
    fn rows_without_a_sender_stand_apart_and_keep_their_own_time() {
        let rows = [
            (Some(Bot), Some("10:00:00")),
            (None, None),
            (Some(Bot), Some("10:00:10")),
            (None, Some("10:00:20")),
            (Some(Bot), Some("10:00:30")),
        ];
        assert_eq!(
            stamps(&rows),
            [
                Some(at("10:00:00")),
                None,
                Some(at("10:00:10")),
                Some(at("10:00:20")),
                Some(at("10:00:30")),
            ]
        );
    }

    #[test]
    fn local_midnight_splits_a_burst_in_the_viewer_zone() {
        let rows = [
            (Some(Bot), Some(at("21:58:00"))),
            (Some(Bot), Some(at("22:01:00"))),
        ];
        let plus_two = FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(
            burst_tail_stamps(&rows, &plus_two),
            [Some(at("21:58:00")), Some(at("22:01:00"))]
        );
        assert_eq!(burst_tail_stamps(&rows, &Utc), [None, Some(at("22:01:00"))]);
    }
}
