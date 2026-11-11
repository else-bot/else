use super::ElseApp;
use super::bot_activity::{BotActivity, group_activity};
use super::reply::Author;
use super::row_text::{RowFormat, RowSource, RowText};
use super::task_reply::state::{ReplyState, TaskReply, task_replies};
use crate::api::{ClientError, LiveCommand, ReplyPosition, StreamMessage};
use crate::model::{Applied, Conversation, Snapshot};
use chrono::{DateTime, Local, TimeDelta, TimeZone, Utc};
use gpui_kit::component::WindowExt as _;
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
    SkillInstall(Uuid),
    Question(Uuid),
    Draft(Uuid),
    SocialSetup(Uuid),
    /// The first task of a provider attempt's delegations.
    Handoff(Uuid),
    ComputerHandoff(Uuid),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TranscriptRow {
    pub(super) key: RowKey,
    pub(super) unread_start: bool,
    pub(super) footer_stamp: Option<DateTime<Utc>>,
    pub(super) routine: Option<RoutineRow>,
    pub(super) task: Option<TaskReply>,
    pub(super) activity: Option<BotActivity>,
    routine_status: Option<(String, bool)>,
    reaction: Option<crate::model::ReactionKind>,
    state: Option<String>,
}

impl TranscriptRow {
    pub(super) fn contains(&self, key: RowKey) -> bool {
        self.key == key
            || matches!(key, RowKey::Item(id) if self.activity.as_ref().is_some_and(|activity| activity.contains(id)))
    }
}

fn first_unread_row(
    rows: &[RowKey],
    conversation: &Conversation,
    read_through: ReplyPosition,
) -> Option<RowKey> {
    let unread: HashSet<_> = conversation
        .items()
        .filter(|item| {
            item.kind == "assistant"
                && !conversation.is_feedback_acknowledgement(item)
                && ReplyPosition {
                    item_seq: item.seq,
                    text_bytes: item.text.len() as i64,
                } > read_through
        })
        .flat_map(|item| {
            [
                Some(RowKey::Item(item.id)),
                Some(RowKey::Task(item.turn_id)),
                conversation
                    .task_receipt(item.turn_id)
                    .map(|receipt| RowKey::Item(receipt.id)),
            ]
        })
        .flatten()
        .collect();
    rows.iter().copied().find(|key| unread.contains(key))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RoutineRow {
    pub(super) turn: Uuid,
    pub(super) first: bool,
    pub(super) last: bool,
}

fn routine_rows(rows: &[RowKey], conversation: Option<&Conversation>) -> Vec<Option<RoutineRow>> {
    let mut turns = HashMap::new();
    for turn in conversation.into_iter().flat_map(|c| c.turns_in_order()) {
        if turn.routine.is_some() {
            for question in &turn.questions {
                turns.insert(RowKey::Question(question.id), turn.id);
            }
            for install in &turn.skill_installs {
                turns.insert(RowKey::SkillInstall(install.id), turn.id);
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
            for delegation in &turn.delegations {
                turns.insert(RowKey::Handoff(delegation.id), turn.id);
            }
            for handoff in &turn.computer_handoffs {
                turns.insert(RowKey::ComputerHandoff(handoff.id), turn.id);
            }
        }
    }
    let turns: Vec<_> = rows
        .iter()
        .map(|key| match *key {
            RowKey::Item(id) => conversation
                .and_then(|c| c.item(id).and_then(|item| c.turn(item.turn_id)))
                .filter(|turn| turn.routine.is_some())
                .map(|turn| turn.id),
            RowKey::Question(_)
            | RowKey::SkillInstall(_)
            | RowKey::SecretRequest(_)
            | RowKey::Draft(_)
            | RowKey::SocialSetup(_)
            | RowKey::Handoff(_)
            | RowKey::ComputerHandoff(_) => turns.get(key).copied(),
            RowKey::Task(_) | RowKey::Pending(_) => None,
        })
        .collect();
    turns
        .iter()
        .enumerate()
        .map(|(ix, turn)| {
            turn.map(|turn| RoutineRow {
                turn,
                first: ix == 0 || turns[ix - 1] != Some(turn),
                last: turns.get(ix + 1) != Some(&Some(turn)),
            })
        })
        .collect()
}

pub(super) fn request_rows(c: &Conversation) -> Vec<(RowKey, Option<Uuid>)> {
    c.turns_in_order()
        .into_iter()
        .flat_map(|turn| {
            turn.secret_requests
                .iter()
                .map(move |request| {
                    (
                        RowKey::SecretRequest(request.id),
                        c.question_anchor(turn.id, None),
                    )
                })
                .chain(turn.skill_installs.iter().map(move |install| {
                    (
                        RowKey::SkillInstall(install.id),
                        c.question_anchor(turn.id, install.settled_at),
                    )
                }))
                .chain(turn.questions.iter().filter_map(move |question| {
                    let anchor = c.question_anchor(turn.id, question.settled_at);
                    (question.is_pending() || anchor.is_some())
                        .then_some((RowKey::Question(question.id), anchor))
                }))
                .chain(
                    turn.delegations
                        .iter()
                        .enumerate()
                        .filter(|(ix, d)| {
                            turn.delegations[..*ix]
                                .iter()
                                .all(|earlier| !earlier.shares_handoff(d))
                        })
                        .filter_map(move |(_, d)| {
                            let anchor = c.question_anchor(turn.id, Some(d.created_at));
                            let group: Vec<_> = turn
                                .delegations
                                .iter()
                                .filter(|other| other.shares_handoff(d))
                                .collect();
                            let live = group.iter().any(|other| !other.status.settled());
                            // A single live task shows in the waiting tray above the composer.
                            let shown = if live {
                                group.len() > 1
                            } else {
                                anchor.is_some()
                            };
                            shown.then_some((RowKey::Handoff(d.id), anchor))
                        }),
                )
                // A pending handoff of a turn outside the loaded page shows
                // only in the composer bar.
                .chain(turn.computer_handoffs.iter().filter_map(move |handoff| {
                    let anchor = c.question_anchor(turn.id, None)?;
                    Some((RowKey::ComputerHandoff(handoff.id), Some(anchor)))
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
                        .filter_map(move |draft| {
                            let anchor = c.question_anchor(turn.id, draft.settled_at);
                            (draft.settled_at.is_none() || anchor.is_some())
                                .then_some((RowKey::Draft(draft.id), anchor))
                        }),
                )
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
    pub(super) unread_since: Option<ReplyPosition>,
    pub(super) _unread_dismissal: Option<Task<()>>,
    pub(super) conversation: Option<Conversation>,
    pub(super) stream_problem: Option<String>,
    pub(super) loading_older: bool,
    pub(super) rows: Vec<TranscriptRow>,
    pub(super) list: ListState,
    pub(super) texts: HashMap<RowKey, RowText>,
    pub(super) skill_cards: HashMap<Uuid, super::skill_card::SkillCard>,
    pub(super) secret_forms: HashMap<Uuid, super::secret_card::SecretForm>,
    pub(super) answered: HashSet<Uuid>,
    pub(super) connections: HashMap<Uuid, super::question_card::connection::Form>,
    pub(super) question_submits: HashMap<Uuid, super::question_card::QuestionForm>,
    /// The typed answer field on each pending single-choice card.
    pub(super) write_ins: HashMap<Uuid, super::question_card::WriteIn>,
    /// The approval scope picked on each pending approval card.
    pub(super) approval_scopes: HashMap<Uuid, String>,
    pub(super) draft_forms: HashMap<Uuid, super::draft_card::DraftForm>,
    pub(super) social_forms: HashMap<Uuid, super::social_card::SocialCard>,
    pub(super) expanded_drafts: HashSet<Uuid>,
    pub(super) expanded_questions: HashSet<Uuid>,
    pub(super) expanded_tasks: HashSet<Uuid>,
    /// Live councils whose queued lane shows every member.
    pub(super) expanded_councils: HashSet<Uuid>,
    /// The waiting tray lists its tasks instead of only its header.
    pub(super) tray_expanded: bool,
    pub(super) reply_transitions: HashMap<Uuid, std::time::Instant>,
    pub(super) _reply_clock: Option<Task<()>>,
    pub(super) latest_seen: bool,
    pub(super) reply_layout: Option<ReplyLayout>,
    /// The row the last "go to original message" landed on, and when.
    pub(super) revealed: Option<(RowKey, std::time::Instant)>,
    /// A quoted message to find once older history arrives.
    pending_reveal: Option<(RowKey, Author, String)>,
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
            .position(|row| row.contains(key))
            .or_else(|| task.and_then(|task| self.rows.iter().position(|row| row.key == task)))
    }

    pub(super) fn new(bot_id: Uuid) -> Self {
        let list = ListState::new(0, ListAlignment::Bottom, px(400.));
        list.set_follow_mode(FollowMode::Tail);
        Self {
            bot_id,
            unread_since: None,
            _unread_dismissal: None,
            conversation: None,
            stream_problem: None,
            loading_older: false,
            rows: Vec::new(),
            list,
            texts: HashMap::new(),
            secret_forms: HashMap::new(),
            skill_cards: HashMap::new(),
            answered: HashSet::new(),
            question_submits: HashMap::new(),
            write_ins: HashMap::new(),
            connections: HashMap::new(),
            approval_scopes: HashMap::new(),
            draft_forms: HashMap::new(),
            social_forms: HashMap::new(),
            expanded_drafts: HashSet::new(),
            expanded_questions: HashSet::new(),
            expanded_tasks: HashSet::new(),
            expanded_councils: HashSet::new(),
            tray_expanded: false,
            reply_transitions: HashMap::new(),
            _reply_clock: None,
            latest_seen: false,
            reply_layout: None,
            revealed: None,
            pending_reveal: None,
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
                self.ensure_rules(bot, window, cx);
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
                        if let Some(entry) = self.bots.iter_mut().find(|b| b.id == bot)
                            && activity.is_some()
                        {
                            entry.activity = activity;
                        }
                        self.sync_rows(window, cx);
                        self.ensure_rules(bot, window, cx);
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

    /// Scrolls to the message a reply quotes, loading older pages until it
    /// is found or the history runs out.
    pub(super) fn reveal_quoted(
        &mut self,
        bot: Uuid,
        from: RowKey,
        author: Author,
        original: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.view.as_mut().filter(|v| v.bot_id == bot) else {
            return;
        };
        view.pending_reveal = None;
        let Some(conversation) = view.conversation.as_ref() else {
            return;
        };
        let before = match from {
            RowKey::Item(id) => conversation.item(id).map(|item| item.seq),
            _ => None,
        };
        // A whole-message match anywhere beats a partial one in a later message.
        let target = [super::reply::quotes_whole, super::reply::quotes_part]
            .into_iter()
            .find_map(|quotes| {
                conversation
                    .items()
                    .rev()
                    .filter(|item| before.is_none_or(|seq| item.seq < seq))
                    .find(|item| {
                        Author::of(&item.kind) == Some(author) && quotes(&item.text, original)
                    })
            })
            .map(|item| item.id);
        if let Some(target) = target
            && let Some(ix) = view.row_index(RowKey::Item(target))
        {
            let row = &view.rows[ix];
            if let Some(task) = &row.task
                && (task.history.contains(&target)
                    || (task.latest == Some(target)
                        && !task.latest_kept
                        && matches!(task.state, ReplyState::Queued | ReplyState::Working)))
            {
                view.expanded_tasks.insert(task.turn);
                view.list.remeasure_items(ix..ix + 1);
            }
            view.list.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: px(0.),
            });
            view.revealed = Some((row.key, std::time::Instant::now()));
            cx.notify();
        } else if conversation.older_before_seq.is_some() {
            view.pending_reveal = Some((from, author, original.to_owned()));
            self.load_older(window, cx);
        } else {
            window.push_notification(
                "The original message is no longer in this conversation.",
                cx,
            );
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
                        let pending = view.pending_reveal.take();
                        app.sync_rows(window, cx);
                        if let Some((from, author, original)) = pending {
                            app.reveal_quoted(bot, from, author, &original, window, cx);
                        }
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
        self.sync_trigger_changes();
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let bot = view.bot_id;
        if self.bots.iter().any(|b| b.id == bot && b.silenced()) {
            view.unread_since = None;
        }
        let conversation = view.conversation.as_ref();
        if let (Some(pending), Some(conversation)) = (self.pending.get_mut(&bot), conversation) {
            pending.retain(|p| !conversation.has_request(p.request_id));
        }
        let tasks = conversation.map(task_replies).unwrap_or_default();
        let mut rows = Vec::new();
        let mut texts = Vec::new();
        let mut requests = conversation.map(request_rows).unwrap_or_default();
        requests.extend(conversation.into_iter().flat_map(|c| {
            c.turns_in_order()
                .into_iter()
                .filter(|turn| {
                    c.reaction_in(turn.id).is_some()
                        && matches!(turn.status.as_str(), "failed" | "interrupted" | "uncertain")
                })
                .map(|turn| {
                    (
                        RowKey::Task(turn.id),
                        c.items()
                            .rev()
                            .find(|item| item.turn_id == turn.id)
                            .map(|item| item.id),
                    )
                })
        }));
        let items: Vec<_> = conversation.into_iter().flat_map(|c| c.items()).collect();
        let delegated = conversation
            .map(Conversation::delegated_turns)
            .unwrap_or_default();
        // A received task's row sits where its latest answer landed, like a task card.
        let answered: HashMap<Uuid, Uuid> = items
            .iter()
            .filter(|item| item.kind == "assistant" && delegated.contains(&item.turn_id))
            .map(|item| (item.turn_id, item.id))
            .collect();
        for item in &items {
            if item.kind == "assistant" && answered.get(&item.turn_id) == Some(&item.id) {
                rows.extend(
                    conversation
                        .and_then(|c| c.task_receipt(item.turn_id))
                        .map(|receipt| RowKey::Item(receipt.id)),
                );
            } else if item.reaction.is_none()
                && !(item.bot_task.is_some() && answered.contains_key(&item.turn_id))
                && (item.kind != "assistant"
                    || !(tasks.contains_key(&item.turn_id) || delegated.contains(&item.turn_id)))
            {
                rows.push(RowKey::Item(item.id));
            }
            let format = RowFormat::for_item(item);
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
                RowFormat::for_owner(&pending.text, &pending.parts),
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
                                    && !conversation
                                        .is_some_and(|c| c.is_feedback_acknowledgement(item))
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
                            (!p.is_rejected() && p.reaction.is_none()).then_some(Author::Owner);
                        (author, Some(p.sent_at))
                    }),
                RowKey::SkillInstall(_)
                | RowKey::SecretRequest(_)
                | RowKey::Question(_)
                | RowKey::Draft(_)
                | RowKey::SocialSetup(_)
                | RowKey::Handoff(_)
                | RowKey::ComputerHandoff(_) => (None, None),
            })
            .collect();
        let unread_start =
            conversation.and_then(|c| first_unread_row(&rows, c, view.unread_since?));
        let mut routines = routine_rows(&rows, conversation);
        if let Some(ix) = rows.iter().position(|key| Some(*key) == unread_start) {
            if let Some(group) = routines[ix].as_mut() {
                group.first = true;
            }
            if ix > 0
                && let Some(group) = routines[ix - 1].as_mut()
            {
                group.last = true;
            }
        }
        let rows: Vec<TranscriptRow> = rows
            .into_iter()
            .zip(burst_tail_stamps(&senders, &Local))
            .zip(routines)
            .map(|((key, footer_stamp), routine)| {
                let routine_turn = routine.and_then(|r| conversation?.turn(r.turn));
                let routine_status = routine_turn.map(super::message_row::turn_label);
                let state = conversation.and_then(|c| match key {
                    RowKey::Question(id) => c.question(id).map(|q| q.status.clone()),
                    RowKey::Draft(id) => c.outgoing_draft(id).map(|d| {
                        format!(
                            "{}:{}:{:?}:{}:{}",
                            d.version, d.updated_at, d.status, d.can_approve, d.in_reply
                        )
                    }),
                    RowKey::SkillInstall(id) => {
                        c.skill_install(id).map(|r| format!("{:?}", r.status))
                    }
                    RowKey::SecretRequest(id) => c.secret_request(id).map(|r| r.status.clone()),
                    RowKey::SocialSetup(id) => c.social_setup(id).map(|s| s.status.clone()),
                    RowKey::ComputerHandoff(id) => {
                        c.computer_handoff(id).map(|h| format!("{:?}", h.status))
                    }
                    RowKey::Handoff(id) => c.handoff(id).map(|(_, group)| {
                        format!("{:?}", group.iter().map(|d| d.status).collect::<Vec<_>>())
                    }),
                    RowKey::Item(id) => c
                        .item(id)
                        .and_then(|i| c.turn(i.turn_id))
                        .map(|t| t.status.clone()),
                    RowKey::Task(id) => c.turn(id).map(|t| t.status.clone()),
                    RowKey::Pending(_) => None,
                });
                TranscriptRow {
                    key,
                    unread_start: Some(key) == unread_start,
                    footer_stamp: footer_stamp.filter(|_| routine.is_none()),
                    routine,
                    routine_status,
                    reaction: match key {
                        RowKey::Item(id) => conversation.and_then(|c| c.reaction_for(id)),
                        _ => None,
                    },
                    task: match key {
                        RowKey::Task(id) => tasks.get(&id).cloned(),
                        _ => None,
                    },
                    activity: None,
                    state,
                }
            })
            .collect();
        let rows = group_activity(rows, conversation);
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
                if task.state == ReplyState::Complete {
                    view.expanded_tasks.remove(&task.turn);
                }
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
            if let Some(ix) = rows.iter().position(|r| r.contains(key)) {
                view.list.remeasure_items(ix..ix + 1);
            }
        }
        view.rows = rows;
        if created {
            super::quotes::paint_view(view, &self.drafts, cx);
        }
        let ticking = tasks.values().any(|reply| reply.state.ticking())
            || view
                .conversation
                .as_ref()
                .is_some_and(|c| !c.waiting_on().is_empty());
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
                                }) || view
                                    .conversation
                                    .as_ref()
                                    .is_some_and(|c| !c.waiting_on().is_empty())
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
        self.sync_write_ins(window, cx);
        self.sync_connections(cx);
        self.sync_draft_forms(window, cx);
        self.sync_social_forms(window, cx);
        self.check_reading(window, cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConversationView, RoutineRow, RowKey, TranscriptRow, burst_tail_stamps, first_unread_row,
        request_rows, routine_rows,
    };
    use crate::api::ReplyPosition;
    use crate::app::reply::Author::{self, Bot, Owner};
    use chrono::{DateTime, FixedOffset, Utc};

    use crate::model::{Conversation, Snapshot};
    use serde_json::{Value, json};
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

    fn question_page(status: &str, seq: i64, created_at: &str) -> Snapshot {
        serde_json::from_value(json!({
            "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
            "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
            "older_before_seq": if seq > 1 { Some(seq) } else { None },
            "items": [{"id": Uuid::from_u128(seq as u128), "seq":seq, "turn_id":Uuid::from_u128(10),
                "kind":"assistant", "text":"Kaneo connection conversation", "created_at":created_at}],
            "turns": [{
                "id":Uuid::from_u128(10), "request_id":Uuid::from_u128(110), "turn_no":1,
                "provider_id":"codex", "status":if status == "pending" { "waiting" } else { "completed" },
                "effect_uncertain":false,
                "questions":[{"id":Uuid::from_u128(210), "kind":"external", "title":"Connect Kaneo",
                    "choices":[], "status":status,
                    "answer":if status == "answered" { Some("approve") } else { None },
                    "settled_at":if status == "pending" { None } else { Some("2026-11-01T00:00:01Z") },
                    "action":{"mcp_connection":{"id":"kaneo", "provider":"https://issuer.test", "scopes":[]}}
                }]
            }],
            "activity":{"phase":"idle", "active_turn_id":null, "queued_count":0}
        })).unwrap()
    }

    fn draft_page(status: &str, seq: i64, created_at: &str) -> Snapshot {
        let items = json!([{"id": Uuid::from_u128(seq as u128), "seq": seq,
            "turn_id": Uuid::from_u128(10), "kind": "assistant", "text": "Edward's reply",
            "created_at": created_at}]);
        serde_json::from_value(card_page(status, items, (seq > 1).then_some(seq))).unwrap()
    }

    fn card_page(status: &str, items: serde_json::Value, older: Option<i64>) -> serde_json::Value {
        let settled = status == "sent";
        json!({
            "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
            "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
            "older_before_seq": older,
            "items": items,
            "turns": [{
                "id":Uuid::from_u128(10), "request_id":Uuid::from_u128(110), "turn_no":1,
                "provider_id":"codex", "status":if settled { "interrupted" } else { "waiting" },
                "effect_uncertain":false,
                "outgoing_drafts": [{"id": Uuid::from_u128(310), "lineage_id": Uuid::from_u128(311),
                    "revision": 1, "content": {"channel":"email", "from": "main@example.invalid",
                    "to": ["edward@example.invalid"], "cc": [], "bcc": [], "subject": "Re: help",
                    "body": "Hello"}, "version": 1, "status": status, "can_approve": !settled,
                    "result_detail": null, "updated_at": "2026-11-01T00:00:01Z",
                    "settled_at": if settled { Some("2026-11-01T00:00:01Z") } else { None }}]
            }],
            "activity":{"phase":"idle", "active_turn_id":null, "queued_count":0}
        })
    }

    fn card_rows(page: serde_json::Value) -> Vec<(RowKey, Option<Uuid>)> {
        request_rows(&Conversation::from_snapshot(serde_json::from_value(page).unwrap()).unwrap())
    }

    #[test]
    fn a_draft_sent_by_a_turn_without_visible_items_follows_earlier_history() {
        let earlier = json!([{"id": Uuid::from_u128(1), "seq": 1, "turn_id": Uuid::from_u128(20),
            "kind": "user", "text": "Earlier request", "created_at": "2026-11-01T00:00:00Z"}]);
        assert_eq!(
            card_rows(card_page("sent", earlier, None)),
            [(
                RowKey::Draft(Uuid::from_u128(310)),
                Some(Uuid::from_u128(1))
            )]
        );
    }

    #[test]
    fn an_undated_item_does_not_pull_a_sent_draft_to_the_tail() {
        let items = json!([
            {"id": Uuid::from_u128(1), "seq": 1, "turn_id": Uuid::from_u128(10),
                "kind": "assistant", "text": "Sent it.", "created_at": "2026-11-02T00:00:00Z"},
            {"id": Uuid::from_u128(2), "seq": 2, "turn_id": Uuid::from_u128(20),
                "kind": "user", "text": "Later request", "created_at": null},
        ]);
        assert_eq!(
            card_rows(card_page("sent", items, None)),
            [(
                RowKey::Draft(Uuid::from_u128(310)),
                Some(Uuid::from_u128(1))
            )]
        );
    }

    #[test]
    fn settled_skill_installs_wait_for_their_history_page() {
        let items = json!([{"id": Uuid::from_u128(2), "seq": 2, "turn_id": Uuid::from_u128(10),
            "kind": "assistant", "text": "Installed.", "created_at": "2026-11-02T00:00:00Z"}]);
        let mut page = card_page("pending", items, Some(2));
        page["turns"][0]["outgoing_drafts"] = json!([]);
        page["turns"][0]["skill_installs"] = json!([{"id": Uuid::from_u128(320),
            "skill_id": "growth", "name": "growth", "description": "Analyze growth",
            "reason": "Review the site", "source": {"url": "https://github.com/example/repo",
                "repo": "example/repo", "path": "skills/growth", "commit": "a41c9e2ffff"},
            "files": [], "env": [], "secret": "skill-growth", "status": "installed",
            "scope": null, "key_stored": false, "created_at": "2026-11-01T00:00:00Z",
            "settled_at": "2026-11-01T00:00:01Z"}]);
        assert_eq!(card_rows(page), []);
    }

    #[test]
    fn sent_drafts_wait_for_their_history_page() {
        let mut c =
            Conversation::from_snapshot(draft_page("sent", 2, "2026-11-02T00:00:00Z")).unwrap();
        assert!(
            request_rows(&c).is_empty(),
            "a sent draft must not reappear at the conversation tail"
        );
        c.merge_older(draft_page("sent", 1, "2026-11-01T00:00:00Z"));
        assert_eq!(
            request_rows(&c),
            [(
                RowKey::Draft(Uuid::from_u128(310)),
                Some(Uuid::from_u128(1))
            )]
        );
        let pending =
            Conversation::from_snapshot(draft_page("pending", 2, "2026-11-02T00:00:00Z")).unwrap();
        assert_eq!(
            request_rows(&pending),
            [(
                RowKey::Draft(Uuid::from_u128(310)),
                Some(Uuid::from_u128(2))
            )]
        );
    }

    #[test]
    fn a_draft_sent_before_any_visible_item_of_its_turn_stays_shown() {
        let c = Conversation::from_snapshot(draft_page("sent", 1, "2026-11-02T00:00:00Z")).unwrap();
        assert_eq!(
            request_rows(&c),
            [(
                RowKey::Draft(Uuid::from_u128(310)),
                Some(Uuid::from_u128(1))
            )]
        );
    }

    #[test]
    fn closed_questions_wait_for_their_history_page() {
        for status in ["cancelled", "answered", "skipped"] {
            let mut c =
                Conversation::from_snapshot(question_page(status, 2, "2026-11-02T00:00:00Z"))
                    .unwrap();
            assert!(
                request_rows(&c).is_empty(),
                "a closed connection card must not reappear at the conversation tail"
            );
            c.merge_older(question_page("pending", 1, "2026-11-01T00:00:00Z"));
            assert_eq!(
                request_rows(&c),
                [(
                    RowKey::Question(Uuid::from_u128(210)),
                    Some(Uuid::from_u128(1))
                )]
            );
        }
    }

    #[test]
    fn each_attempts_delegations_share_one_handoff_after_the_message_before_them() {
        let task = |id: u128, attempt: i64, at: &str| {
            json!({ "id": Uuid::from_u128(id), "attempt": attempt,
                    "target_bot_id": Uuid::from_u128(id + 50), "target_name": "Scout",
                    "task": "Research", "status": "completed", "created_at": at })
        };
        let snapshot: Snapshot = serde_json::from_value(json!({
            "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
            "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
            "items": [
                { "id": Uuid::from_u128(1), "seq": 1, "turn_id": Uuid::from_u128(10), "kind": "assistant",
                  "text": "Asking the council.", "created_at": "2026-11-07T10:00:00Z" },
                { "id": Uuid::from_u128(2), "seq": 2, "turn_id": Uuid::from_u128(10), "kind": "assistant",
                  "text": "Now Scout alone.", "created_at": "2026-11-07T10:05:00Z" }
            ],
            "turns": [{
                "id": Uuid::from_u128(10), "request_id": Uuid::from_u128(11), "turn_no": 1,
                "provider_id": "codex", "status": "waiting", "error": null, "effect_uncertain": false,
                "delegations": [task(20, 1, "2026-11-07T10:01:00Z"), task(21, 1, "2026-11-07T10:01:00Z"),
                                task(22, 2, "2026-11-07T10:06:00Z")]
            }],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
        }))
        .unwrap();
        let c = Conversation::from_snapshot(snapshot).unwrap();
        assert_eq!(
            request_rows(&c),
            [
                (
                    RowKey::Handoff(Uuid::from_u128(20)),
                    Some(Uuid::from_u128(1))
                ),
                (
                    RowKey::Handoff(Uuid::from_u128(22)),
                    Some(Uuid::from_u128(2))
                ),
            ]
        );
        let council: Vec<_> = c
            .handoff(Uuid::from_u128(21))
            .unwrap()
            .1
            .iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(council, [Uuid::from_u128(20), Uuid::from_u128(21)]);
    }

    #[test]
    fn a_settled_handoff_waits_for_its_history_page_but_a_live_one_shows() {
        let page = |status: &str| -> Snapshot {
            serde_json::from_value(json!({
                "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
                "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
                "older_before_seq": 5, "items": [],
                "turns": [{
                    "id": Uuid::from_u128(10), "request_id": Uuid::from_u128(11), "turn_no": 1,
                    "provider_id": "codex", "status": "completed", "error": null,
                    "effect_uncertain": false,
                    "delegations": [{ "id": Uuid::from_u128(20), "attempt": 1,
                        "target_bot_id": Uuid::from_u128(30), "target_name": "Scout",
                        "task": "Research", "status": status,
                        "created_at": "2026-11-07T10:01:00Z" }]
                }],
                "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
            }))
            .unwrap()
        };
        let settled = Conversation::from_snapshot(page("completed")).unwrap();
        assert!(
            request_rows(&settled).is_empty(),
            "no stale summary at the tail"
        );
        let live = Conversation::from_snapshot(page("running")).unwrap();
        assert!(
            request_rows(&live).is_empty(),
            "a single live task shows in the waiting tray, not the transcript"
        );
    }

    #[test]
    fn the_tray_waits_on_unfinished_and_unread_single_tasks_of_unfinished_turns() {
        let task = |id: u128, attempt: i64, status: &str, seen: bool| {
            json!({ "id": Uuid::from_u128(id), "attempt": attempt,
                    "target_bot_id": Uuid::from_u128(id + 50), "target_name": "Scout",
                    "task": "Research", "status": status, "seen": seen,
                    "created_at": format!("2026-11-07T10:{:02}:00Z", id - 20) })
        };
        let turn = |id: u128, no: i64, status: &str, delegations: Value| {
            json!({ "id": Uuid::from_u128(id), "request_id": Uuid::from_u128(id + 1),
                    "turn_no": no, "provider_id": "codex", "status": status, "error": null,
                    "effect_uncertain": false, "delegations": delegations })
        };
        let snapshot: Snapshot = serde_json::from_value(json!({
            "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
            "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
            "items": [],
            "turns": [
                turn(10, 1, "failed", json!([task(20, 1, "completed", false)])),
                turn(12, 2, "running", json!([
                    task(21, 0, "running", false), task(22, 1, "completed", false),
                    task(23, 0, "completed", true), task(24, 2, "running", false),
                    task(25, 2, "queued", false)
                ])),
            ],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
        }))
        .unwrap();
        let c = Conversation::from_snapshot(snapshot).unwrap();
        let waiting: Vec<_> = c.waiting_on().iter().map(|d| d.id).collect();
        assert_eq!(waiting, [Uuid::from_u128(21), Uuid::from_u128(22)]);
    }

    #[test]
    fn tasks_carried_from_earlier_messages_stand_alone_and_a_live_council_stays_in_the_transcript()
    {
        let task = |id: u128, attempt: i64, status: &str| {
            json!({ "id": Uuid::from_u128(id), "attempt": attempt,
                    "target_bot_id": Uuid::from_u128(id + 50), "target_name": "Scout",
                    "task": "Research", "status": status, "created_at": "2026-11-07T10:01:00Z" })
        };
        let snapshot: Snapshot = serde_json::from_value(json!({
            "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
            "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
            "items": [],
            "turns": [{
                "id": Uuid::from_u128(10), "request_id": Uuid::from_u128(11), "turn_no": 2,
                "provider_id": "codex", "status": "waiting", "error": null, "effect_uncertain": false,
                "delegations": [task(20, 0, "running"), task(21, 0, "running"),
                                task(22, 1, "running"), task(23, 1, "queued")]
            }],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
        }))
        .unwrap();
        let c = Conversation::from_snapshot(snapshot).unwrap();
        assert_eq!(c.handoff(Uuid::from_u128(21)).unwrap().1.len(), 1);
        assert_eq!(
            request_rows(&c),
            [(RowKey::Handoff(Uuid::from_u128(22)), None)]
        );
    }

    #[test]
    fn pending_questions_remain_visible_without_loaded_messages() {
        let mut page = question_page("pending", 2, "2026-11-02T00:00:00Z");
        page.items.clear();
        let c = Conversation::from_snapshot(page).unwrap();
        assert_eq!(
            request_rows(&c),
            [(RowKey::Question(Uuid::from_u128(210)), None)]
        );
    }

    fn item(id: u128) -> RowKey {
        RowKey::Item(Uuid::from_u128(id))
    }

    fn activity_conversation() -> Conversation {
        let items: Vec<_> = (1..=8)
            .map(|id| {
                let mut item = json!({
                    "id": Uuid::from_u128(id), "seq": id,
                    "turn_id": Uuid::from_u128(match id { 2 => 1, 5 => 4, 8 => 7, _ => id } + 100),
                    "kind": match id { 3 => "user", 6 => "assistant", _ => "status" },
                    "text": format!("Full message {id}")
                });
                if matches!(id, 1 | 7) {
                    item["bot_task"] = json!({
                        "sender_bot_id": Uuid::from_u128(90), "sender_name": "Dev",
                        "prompt": "Complete original task"
                    });
                } else if matches!(id, 2 | 4 | 5 | 8) {
                    item["bot_reply"] = json!({
                        "sender_bot_id": Uuid::from_u128(90), "sender_name": "Dev",
                        "prompt": "Complete original task",
                        "status": match id { 4 => "failed", 5 => "cancelled", _ => "completed" }
                    });
                }
                item
            })
            .collect();
        Conversation::from_snapshot(serde_json::from_value(json!({
            "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
            "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
            "items": items, "turns": [],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
        })).unwrap()).unwrap()
    }

    fn transcript_row(key: RowKey) -> TranscriptRow {
        TranscriptRow {
            key,
            unread_start: false,
            footer_stamp: None,
            routine: None,
            task: None,
            activity: None,
            routine_status: None,
            reaction: None,
            state: None,
        }
    }

    #[test]
    fn bot_activity_groups_adjacent_exchanges_without_hiding_owner_or_action_rows() {
        let c = activity_conversation();
        let question = RowKey::Question(Uuid::from_u128(40));
        let task = RowKey::Task(Uuid::from_u128(41));
        let keys = [
            item(1),
            item(2),
            item(3),
            item(4),
            item(5),
            item(6),
            question,
            item(7),
            item(8),
            task,
        ];
        let rows = super::group_activity(keys.into_iter().map(transcript_row).collect(), Some(&c));
        assert_eq!(
            rows.iter().map(|row| row.key).collect::<Vec<_>>(),
            [item(1), item(3), item(4), item(6), question, item(7), task]
        );
        assert_eq!(
            rows[0].activity.as_ref().unwrap().items,
            [Uuid::from_u128(1), Uuid::from_u128(2)]
        );
        assert_eq!(
            rows[2].activity.as_ref().unwrap().items,
            [Uuid::from_u128(4), Uuid::from_u128(5)]
        );
        assert!(rows[1].activity.is_none());
        assert!(rows[3].activity.is_none());
        assert_eq!(c.item(Uuid::from_u128(2)).unwrap().text, "Full message 2");
        assert_eq!(
            c.item(Uuid::from_u128(2))
                .unwrap()
                .bot_reply
                .as_ref()
                .unwrap()
                .prompt,
            "Complete original task"
        );
        let mut view = ConversationView::new(Uuid::from_u128(80));
        view.conversation = Some(c);
        view.rows = rows;
        assert_eq!(view.row_index(item(2)), Some(0));
        assert_eq!(view.row_index(item(5)), Some(2));
        assert_eq!(view.row_index(item(6)), Some(3));
    }

    #[test]
    fn bot_activity_keeps_each_turns_exchange_in_its_own_row() {
        let c = activity_conversation();
        let rows = super::group_activity(
            [item(1), item(2), item(7), item(8)]
                .into_iter()
                .map(transcript_row)
                .collect(),
            Some(&c),
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.activity.as_ref().unwrap().items.clone())
                .collect::<Vec<_>>(),
            [
                vec![Uuid::from_u128(1), Uuid::from_u128(2)],
                vec![Uuid::from_u128(7), Uuid::from_u128(8)],
            ],
            "tasks from different turns stay separate even when adjacent"
        );
    }

    #[test]
    fn a_received_task_row_holds_this_bots_answer() {
        let c = Conversation::from_snapshot(serde_json::from_value(json!({
            "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
            "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
            "items": [
                {"id": Uuid::from_u128(1), "seq": 1, "turn_id": Uuid::from_u128(10), "kind": "status",
                 "text": "Received a task", "bot_task": {"sender_bot_id": Uuid::from_u128(90),
                 "sender_name": "Proliquid", "prompt": "List your routines"}},
                {"id": Uuid::from_u128(2), "seq": 2, "turn_id": Uuid::from_u128(10),
                 "kind": "assistant", "text": "Report attached.",
                 "parts": [{"type": "file", "file": {"id": Uuid::from_u128(50),
                     "name": "report.txt", "media_type": "text/plain", "size": 12}}]},
                {"id": Uuid::from_u128(3), "seq": 3, "turn_id": Uuid::from_u128(11),
                 "kind": "user", "text": "Hi"},
                {"id": Uuid::from_u128(4), "seq": 4, "turn_id": Uuid::from_u128(10),
                 "kind": "assistant", "text": "Progress note."},
                {"id": Uuid::from_u128(5), "seq": 5, "turn_id": Uuid::from_u128(10),
                 "kind": "assistant", "text": "No routines."}
            ],
            "turns": [{"id": Uuid::from_u128(10), "request_id": Uuid::from_u128(110), "turn_no": 1,
                       "provider_id": "codex", "status": "completed", "effect_uncertain": false}],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
        })).unwrap()).unwrap();
        assert_eq!(
            first_unread_row(
                &[item(3), item(1)],
                &c,
                ReplyPosition {
                    item_seq: 3,
                    text_bytes: 0
                }
            ),
            Some(item(1)),
            "an unread answer marks its exchange row"
        );
        let rows = super::group_activity(vec![transcript_row(item(1))], Some(&c));
        assert_eq!(
            rows[0].activity.as_ref().unwrap().answers,
            [Uuid::from_u128(2), Uuid::from_u128(5)],
            "the file reply and the final answer stay reachable"
        );
        let mut view = ConversationView::new(Uuid::from_u128(80));
        view.conversation = Some(c);
        view.rows = rows;
        assert_eq!(
            view.row_index(item(5)),
            Some(0),
            "the answer scrolls to its exchange row"
        );
    }

    #[test]
    fn bot_activity_keeps_unread_and_routine_boundaries() {
        let c = activity_conversation();
        let first = transcript_row(item(1));
        let mut second = transcript_row(item(2));
        second.unread_start = true;
        let rows = super::group_activity(vec![first.clone(), second], Some(&c));
        assert_eq!(rows.len(), 2);
        assert!(rows[1].unread_start);
        let mut routine = transcript_row(item(4));
        routine.routine = Some(RoutineRow {
            turn: Uuid::from_u128(104),
            first: true,
            last: true,
        });
        let rows = super::group_activity(vec![first, routine], Some(&c));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].routine.unwrap().turn, Uuid::from_u128(104));
        assert_eq!(
            rows[1].activity.as_ref().unwrap().items,
            [Uuid::from_u128(4)]
        );
        let mut first = transcript_row(item(1));
        first.routine = Some(RoutineRow {
            turn: Uuid::from_u128(101),
            first: true,
            last: false,
        });
        let mut second = transcript_row(item(2));
        second.routine = Some(RoutineRow {
            turn: Uuid::from_u128(101),
            first: false,
            last: true,
        });
        let rows = super::group_activity(vec![first, second], Some(&c));
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].routine,
            Some(RoutineRow {
                turn: Uuid::from_u128(101),
                first: true,
                last: true
            })
        );
    }

    #[test]
    fn unread_divider_precedes_the_first_unread_reply_and_includes_appended_text() {
        let c = routine_conversation();
        let rows = [item(1), item(2), item(3), item(4), item(5)];
        let read = |item_seq, text_bytes| ReplyPosition {
            item_seq,
            text_bytes,
        };
        assert_eq!(first_unread_row(&rows, &c, read(0, 0)), Some(item(1)));
        assert_eq!(first_unread_row(&rows, &c, read(2, 5)), Some(item(3)));
        assert_eq!(first_unread_row(&rows, &c, read(2, 3)), Some(item(2)));
        assert_eq!(first_unread_row(&rows, &c, read(5, 5)), None);
    }

    #[test]
    fn unread_divider_marks_the_visible_task_when_unread_replies_are_folded() {
        let c = routine_conversation();
        let task = RowKey::Task(Uuid::from_u128(10));
        let rows = [task, item(3), item(4)];
        assert_eq!(
            first_unread_row(
                &rows,
                &c,
                ReplyPosition {
                    item_seq: 2,
                    text_bytes: 5,
                },
            ),
            Some(task)
        );
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
            unread_start: false,
            footer_stamp: None,
            routine: None,
            task: None,
            activity: None,
            routine_status: None,
            reaction: None,
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
                    last: false
                }),
                Some(RoutineRow {
                    turn: Uuid::from_u128(10),
                    first: false,
                    last: false
                }),
                Some(RoutineRow {
                    turn: Uuid::from_u128(10),
                    first: false,
                    last: false
                }),
                Some(RoutineRow {
                    turn: Uuid::from_u128(10),
                    first: false,
                    last: true
                }),
            ]
        );
    }

    #[test]
    fn task_bubbles_separate_the_surrounding_routine_request_cards() {
        let c = routine_conversation();
        let rows = [
            RowKey::Question(Uuid::from_u128(210)),
            RowKey::Task(Uuid::from_u128(10)),
            RowKey::SecretRequest(Uuid::from_u128(310)),
        ];
        assert_eq!(
            routine_rows(&rows, Some(&c)),
            [
                Some(RoutineRow {
                    turn: Uuid::from_u128(10),
                    first: true,
                    last: true,
                }),
                None,
                Some(RoutineRow {
                    turn: Uuid::from_u128(10),
                    first: true,
                    last: true,
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
                    last: true
                }),
                Some(RoutineRow {
                    turn: Uuid::from_u128(11),
                    first: true,
                    last: true
                }),
                None,
                Some(RoutineRow {
                    turn: Uuid::from_u128(10),
                    first: true,
                    last: true
                }),
                None,
            ]
        );
    }

    #[test]
    fn streaming_a_new_message_extends_the_routine_card() {
        let c = routine_conversation();
        let before = routine_rows(&[item(1)], Some(&c));
        let after = routine_rows(&[item(1), item(2)], Some(&c));
        assert!(before[0].unwrap().last);
        assert!(after[0].unwrap().first);
        assert!(!after[0].unwrap().last);
        assert!(!after[1].unwrap().first);
        assert!(after[1].unwrap().last);
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
