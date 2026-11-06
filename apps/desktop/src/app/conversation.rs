use super::ElseApp;
use super::reply::Author;
use super::row_text::{RowFormat, RowSource, RowText};
use super::task_reply::state::{TaskReply, task_replies};
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
    Task(Uuid),
    Pending(Uuid),
    SecretRequest(Uuid),
    Question(Uuid),
    Draft(Uuid),
    SocialSetup(Uuid),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TranscriptRow {
    pub(super) key: RowKey,
    pub(super) footer_stamp: Option<DateTime<Utc>>,
    pub(super) routine: Option<RoutineRow>,
    pub(super) task: Option<TaskReply>,
    routine_status: Option<(String, bool)>,
    state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RoutineRow {
    pub(super) turn: Uuid,
    pub(super) first: bool,
    pub(super) last: bool,
    pub(super) tail: bool,
}

fn routine_rows(rows: &[RowKey], conversation: Option<&Conversation>) -> Vec<Option<RoutineRow>> {
    let mut turns = HashMap::new();
    for turn in conversation.into_iter().flat_map(|c| c.turns_in_order()) {
        if turn.routine.is_some() {
            for question in &turn.questions {
                turns.insert(RowKey::Question(question.id), turn.id);
            }
            for request in &turn.secret_requests {
                turns.insert(RowKey::SecretRequest(request.id), turn.id);
            }
            for draft in &turn.outgoing_drafts {
                turns.insert(RowKey::Draft(draft.id), turn.id);
            }
            for setup in &turn.social_setups {
                turns.insert(RowKey::SocialSetup(setup.id), turn.id);
            }
        }
    }
    let turns: Vec<_> = rows
        .iter()
        .map(|key| match *key {
            RowKey::Task(id) => conversation
                .and_then(|c| c.turn(id))
                .filter(|turn| turn.routine.is_some())
                .map(|turn| turn.id),
            RowKey::Item(id) => conversation
                .and_then(|c| c.item(id).and_then(|item| c.turn(item.turn_id)))
                .filter(|turn| turn.routine.is_some())
                .map(|turn| turn.id),
            RowKey::Question(_)
            | RowKey::SecretRequest(_)
            | RowKey::Draft(_)
            | RowKey::SocialSetup(_) => turns.get(key).copied(),
            RowKey::Pending(_) => None,
        })
        .collect();
    let tails: HashMap<_, _> = turns
        .iter()
        .enumerate()
        .filter_map(|(ix, turn)| turn.map(|turn| (turn, ix)))
        .collect();
    turns
        .iter()
        .enumerate()
        .map(|(ix, turn)| {
            turn.map(|turn| RoutineRow {
                turn,
                first: ix == 0 || turns[ix - 1] != Some(turn),
                last: turns.get(ix + 1) != Some(&Some(turn)),
                tail: tails.get(&turn) == Some(&ix),
            })
        })
        .collect()
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
    pub(super) question_submits: HashMap<Uuid, super::question_card::QuestionForm>,
    pub(super) draft_forms: HashMap<Uuid, super::draft_card::DraftForm>,
    pub(super) social_forms: HashMap<Uuid, super::social_card::SocialCard>,
    pub(super) expanded_drafts: HashSet<Uuid>,
    pub(super) expanded_tasks: HashSet<Uuid>,
    pub(super) reply_transitions: HashMap<Uuid, std::time::Instant>,
    pub(super) _reply_clock: Option<Task<()>>,
    pub(super) latest_seen: bool,
    pub(super) reply_layout: Option<ReplyLayout>,
    pub(super) _load: Option<Task<()>>,
    pub(super) _older: Option<Task<()>>,
}

impl ConversationView {
    pub(super) fn row_index(&self, key: RowKey) -> Option<usize> {
        let task = match key {
            RowKey::Item(id) => self
                .conversation
                .as_ref()
                .and_then(|c| c.item(id))
                .map(|i| RowKey::Task(i.turn_id)),
            _ => None,
        };
        self.rows
            .iter()
            .position(|row| row.key == key)
            .or_else(|| task.and_then(|task| self.rows.iter().position(|row| row.key == task)))
    }

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
            draft_forms: HashMap::new(),
            social_forms: HashMap::new(),
            expanded_drafts: HashSet::new(),
            expanded_tasks: HashSet::new(),
            reply_transitions: HashMap::new(),
            _reply_clock: None,
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
            && let Some(ix) = view.row_index(key)
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

    pub(super) fn sync_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let bot = view.bot_id;
        let conversation = view.conversation.as_ref();
        if let (Some(pending), Some(conversation)) = (self.pending.get_mut(&bot), conversation) {
            pending.retain(|p| !conversation.has_request(p.request_id));
        }
        let tasks = conversation.map(task_replies).unwrap_or_default();
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
                        .chain(turn.social_setups.iter().map(move |setup| {
                            (
                                RowKey::SocialSetup(setup.id),
                                c.question_anchor(turn.id, None),
                            )
                        }))
                        .chain(
                            turn.outgoing_drafts
                                .iter()
                                .filter(|d| d.status != crate::model::DraftStatus::Superseded)
                                .map(move |draft| {
                                    (
                                        RowKey::Draft(draft.id),
                                        c.question_anchor(turn.id, draft.settled_at),
                                    )
                                }),
                        )
                })
            })
            .collect();
        let items: Vec<_> = conversation.into_iter().flat_map(|c| c.items()).collect();
        for item in &items {
            if item.kind != "assistant" || !tasks.contains_key(&item.turn_id) {
                rows.push(RowKey::Item(item.id));
            }
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
                item.parts.clone(),
            ));
            if tasks
                .get(&item.turn_id)
                .is_some_and(|reply| reply.anchor == Some(item.id))
            {
                rows.push(RowKey::Task(item.turn_id));
            }
            requests.retain(|(key, anchor)| {
                let here = *anchor == Some(item.id);
                if here {
                    rows.push(*key);
                }
                !here
            });
        }
        for turn in conversation.into_iter().flat_map(|c| c.turns_in_order()) {
            if tasks
                .get(&turn.id)
                .is_some_and(|reply| reply.anchor.is_none())
            {
                rows.push(RowKey::Task(turn.id));
            }
        }
        rows.extend(requests.into_iter().map(|(key, _)| key));
        for pending in self.pending.get(&bot).into_iter().flatten() {
            rows.push(RowKey::Pending(pending.request_id));
            texts.push((
                RowKey::Pending(pending.request_id),
                pending.text.clone(),
                RowFormat::Plain,
                pending.parts.clone(),
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
                            let author = Author::of(&item.kind).filter(|_| {
                                item.reaction.is_none()
                                    && conversation
                                        .and_then(|c| c.turn(item.turn_id))
                                        .is_none_or(|turn| turn.routine.is_none())
                            });
                            (author, item.created_at)
                        })
                }
                RowKey::Task(id) => (
                    Some(Author::Bot),
                    conversation
                        .and_then(|c| c.turn(id))
                        .and_then(|t| t.finished_at),
                ),
                RowKey::Pending(request) => pending
                    .and_then(|list| list.iter().find(|p| p.request_id == request))
                    .map_or((None, None), |p| {
                        let author =
                            (p.rejected.is_none() && p.reaction.is_none()).then_some(Author::Owner);
                        (author, Some(p.sent_at))
                    }),
                RowKey::SecretRequest(_)
                | RowKey::Question(_)
                | RowKey::Draft(_)
                | RowKey::SocialSetup(_) => (None, None),
            })
            .collect();
        let routines = routine_rows(&rows, conversation);
        let rows: Vec<TranscriptRow> = rows
            .into_iter()
            .zip(burst_tail_stamps(&senders, &Local))
            .zip(routines)
            .map(|((key, footer_stamp), routine)| {
                let routine_turn = routine.and_then(|r| conversation?.turn(r.turn));
                let routine_status = routine_turn.map(super::message_row::turn_label);
                let footer_stamp = if let Some(routine) = routine {
                    conversation
                        .filter(|_| routine.tail)
                        .and_then(|c| c.items().rev().find(|item| item.turn_id == routine.turn))
                        .and_then(|item| item.created_at)
                } else {
                    footer_stamp
                };
                let state = conversation.and_then(|c| match key {
                    RowKey::Question(id) => c.question(id).map(|q| q.status.clone()),
                    RowKey::Draft(id) => c.outgoing_draft(id).map(|d| {
                        format!(
                            "{}:{}:{:?}:{}",
                            d.version, d.updated_at, d.status, d.can_approve
                        )
                    }),
                    RowKey::SecretRequest(id) => c.secret_request(id).map(|r| r.status.clone()),
                    RowKey::SocialSetup(id) => c.social_setup(id).map(|s| s.status.clone()),
                    RowKey::Item(id) => c
                        .item(id)
                        .and_then(|i| c.turn(i.turn_id))
                        .map(|t| t.status.clone()),
                    RowKey::Task(id) => c.turn(id).map(|t| t.status.clone()),
                    RowKey::Pending(_) => None,
                });
                TranscriptRow {
                    key,
                    footer_stamp,
                    routine,
                    routine_status,
                    task: match key {
                        RowKey::Task(id) => tasks.get(&id).cloned(),
                        _ => None,
                    },
                    state,
                }
            })
            .collect();
        let old = std::mem::take(&mut view.rows);
        view.reply_transitions
            .retain(|_, at| at.elapsed() < Duration::from_millis(320));
        for task in tasks.values() {
            if task.state.terminal()
                && old.iter().any(|row| {
                    row.task
                        .as_ref()
                        .is_some_and(|prior| prior.turn == task.turn && !prior.state.terminal())
                })
            {
                view.reply_transitions
                    .insert(task.turn, std::time::Instant::now());
            }
        }
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
            if previous.get(&row.key).is_some_and(|old| *old != row) {
                view.list.remeasure_items(ix..ix + 1);
            }
        }
        let mut changed = Vec::new();
        let mut created = false;
        let text_keys: HashSet<_> = texts.iter().map(|(key, ..)| *key).collect();
        for (key, text, format, parts) in texts {
            match view.texts.get_mut(&key) {
                Some(row) if row.format() == format => {
                    if row.sync(
                        RowSource {
                            text,
                            parts,
                            format,
                        },
                        &self.mention_bots,
                        window,
                        cx,
                    ) {
                        changed.push(key);
                    }
                }
                _ => {
                    view.texts.insert(
                        key,
                        RowText::new(
                            key,
                            RowSource {
                                text,
                                parts,
                                format,
                            },
                            self.client.clone(),
                            &self.mention_bots,
                            bot,
                            window,
                            cx,
                        ),
                    );
                    created = true;
                }
            }
        }
        view.texts.retain(|key, _| text_keys.contains(key));
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
            let key = match key {
                RowKey::Item(id) => conversation
                    .and_then(|c| c.item(id))
                    .filter(|item| item.kind == "assistant" && tasks.contains_key(&item.turn_id))
                    .map_or(key, |item| RowKey::Task(item.turn_id)),
                _ => key,
            };
            if let Some(ix) = rows.iter().position(|r| r.key == key) {
                view.list.remeasure_items(ix..ix + 1);
            }
        }
        view.rows = rows;
        let ticking = tasks.values().any(|reply| reply.state.ticking());
        if !ticking {
            view._reply_clock = None;
        } else if view._reply_clock.is_none() {
            view._reply_clock = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let keep = this.update(cx, |app, cx| {
                        let ticking = app
                            .view
                            .as_ref()
                            .filter(|view| view.bot_id == bot)
                            .is_some_and(|view| {
                                view.rows.iter().any(|row| {
                                    row.task.as_ref().is_some_and(|task| task.state.ticking())
                                })
                            });
                        if ticking {
                            cx.notify();
                        }
                        ticking
                    });
                    if !matches!(keep, Ok(true)) {
                        break;
                    }
                }
            }));
        }
        self.sync_secret_forms(window, cx);
        self.sync_question_submits();
        self.sync_draft_forms(window, cx);
        self.sync_social_forms(window, cx);
        self.check_reading(window, cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConversationView, RoutineRow, RowKey, TranscriptRow, burst_tail_stamps, routine_rows,
    };
    use crate::app::reply::Author::{self, Bot, Owner};
    use chrono::{DateTime, FixedOffset, Utc};

    use crate::model::{Conversation, Snapshot};
    use serde_json::json;
    use uuid::Uuid;

    fn routine_conversation() -> Conversation {
        let turn = |id: u128| {
            json!({
                "id": Uuid::from_u128(id), "request_id": Uuid::from_u128(id + 100),
                "turn_no": id, "provider_id": "codex", "status": "waiting",
                "error": null, "effect_uncertain": false,
                "routine": { "routine_id": Uuid::from_u128(99), "name": "Cleanup",
                             "due_at": "2026-11-05T04:00:00Z" },
                "questions": [{ "id": Uuid::from_u128(id + 200), "kind": "external",
                                "title": "Approve cleanup?", "choices": [], "status": "pending" }],
                "secret_requests": [{ "id": Uuid::from_u128(id + 300), "name": "Access",
                                      "reason": "Read branches", "env": ["TOKEN"], "status": "pending" }]
            })
        };
        let items: Vec<_> = [(1, 10), (2, 10), (3, 11), (4, 12), (5, 10)]
            .into_iter()
            .map(|(id, turn)| {
                json!({
                    "id": Uuid::from_u128(id), "seq": id, "turn_id": Uuid::from_u128(turn),
                    "kind": "assistant", "text": "Reply", "provider_id": "codex"
                })
            })
            .collect();
        let snapshot: Snapshot = serde_json::from_value(json!({
            "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
            "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
            "items": items, "turns": [turn(10), turn(11)],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
        })).unwrap();
        Conversation::from_snapshot(snapshot).unwrap()
    }

    fn item(id: u128) -> RowKey {
        RowKey::Item(Uuid::from_u128(id))
    }

    #[test]
    fn folded_text_remeasures_its_capsule_and_plain_rows_keep_their_own_slot() {
        let mut view = ConversationView::new(Uuid::from_u128(80));
        view.conversation = Some(routine_conversation());
        view.rows = [
            RowKey::Item(Uuid::from_u128(3)),
            RowKey::Task(Uuid::from_u128(10)),
        ]
        .into_iter()
        .map(|key| TranscriptRow {
            key,
            footer_stamp: None,
            routine: None,
            task: None,
            routine_status: None,
            state: None,
        })
        .collect();
        assert_eq!(view.row_index(item(3)), Some(0));
        assert_eq!(view.row_index(item(1)), Some(1));
        assert_eq!(view.row_index(item(2)), Some(1));
        assert_eq!(view.row_index(item(999)), None);
    }

    #[test]
    fn routine_card_contains_messages_and_requests_once() {
        let c = routine_conversation();
        let rows = [
            item(1),
            item(2),
            RowKey::Question(Uuid::from_u128(210)),
            RowKey::SecretRequest(Uuid::from_u128(310)),
        ];
        let groups = routine_rows(&rows, Some(&c));
        assert_eq!(
            groups,
            [
                Some(RoutineRow {
                    turn: Uuid::from_u128(10),
                    first: true,
                    last: false,
                    tail: false
                }),
                Some(RoutineRow {
                    turn: Uuid::from_u128(10),
                    first: false,
                    last: false,
                    tail: false
                }),
                Some(RoutineRow {
                    turn: Uuid::from_u128(10),
                    first: false,
                    last: false,
                    tail: false
                }),
                Some(RoutineRow {
                    turn: Uuid::from_u128(10),
                    first: false,
                    last: true,
                    tail: true
                }),
            ]
        );
    }

    #[test]
    fn separate_occurrences_and_chat_rows_keep_separate_cards() {
        let c = routine_conversation();
        let rows = [
            item(1),
            item(3),
            item(4),
            item(5),
            RowKey::Pending(Uuid::from_u128(90)),
        ];
        let groups = routine_rows(&rows, Some(&c));
        assert_eq!(
            groups,
            [
                Some(RoutineRow {
                    turn: Uuid::from_u128(10),
                    first: true,
                    last: true,
                    tail: false
                }),
                Some(RoutineRow {
                    turn: Uuid::from_u128(11),
                    first: true,
                    last: true,
                    tail: true
                }),
                None,
                Some(RoutineRow {
                    turn: Uuid::from_u128(10),
                    first: true,
                    last: true,
                    tail: true
                }),
                None,
            ]
        );
    }

    #[test]
    fn streaming_a_new_message_moves_the_routine_footer() {
        let c = routine_conversation();
        let before = routine_rows(&[item(1)], Some(&c));
        let after = routine_rows(&[item(1), item(2)], Some(&c));
        assert!(before[0].unwrap().last && before[0].unwrap().tail);
        assert!(after[0].unwrap().first);
        assert!(!after[0].unwrap().last && !after[0].unwrap().tail);
        assert!(!after[1].unwrap().first);
        assert!(after[1].unwrap().last && after[1].unwrap().tail);
    }

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
