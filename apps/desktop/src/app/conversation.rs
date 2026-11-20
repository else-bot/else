use super::ElseApp;
use super::bot_activity::{BotActivity, group_activity};
use super::reply::Author;
use super::row_text::{RowFormat, RowSource, RowText};
use super::task_reply::state::{ReplyState, TaskReply, place_cards, task_replies};
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
    OwnerNotice(Uuid),
    /// Unloaded messages just below the loaded item with this seq.
    Gap(i64),
    /// A routine run that completed with no news, folded to its header.
    QuietRun(Uuid),
    /// Consecutive no-news runs of one routine, by the first run's turn.
    QuietRuns(Uuid),
}

impl RowKey {
    pub(super) fn task_sender(self, c: &Conversation) -> Option<&str> {
        if !matches!(
            self,
            Self::Question(_)
                | Self::SecretRequest(_)
                | Self::Draft(_)
                | Self::Handoff(_)
                | Self::ComputerHandoff(_)
        ) {
            return None;
        }
        let turn = c.turns_in_order().into_iter().find(|turn| match self {
            Self::Question(id) => turn.questions.iter().any(|q| q.id == id),
            Self::SecretRequest(id) => turn.secret_requests.iter().any(|r| r.id == id),
            Self::Draft(id) => turn.outgoing_drafts.iter().any(|d| d.id == id),
            Self::Handoff(id) => turn.delegations.iter().any(|d| d.id == id),
            Self::ComputerHandoff(id) => turn.computer_handoffs.iter().any(|h| h.id == id),
            _ => false,
        })?;
        Some(&c.task_receipt(turn.id)?.bot_task.as_ref()?.sender_name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TranscriptRow {
    pub(super) key: RowKey,
    pub(super) unread_start: bool,
    pub(super) footer_stamp: Option<DateTime<Utc>>,
    /// Set on the first row of each local day; a centered separator names that day.
    pub(super) day: Option<DateTime<Utc>>,
    pub(super) routine: Option<RoutineRow>,
    pub(super) task: Option<TaskReply>,
    /// A reply's turn updates from before the owner message below it, which
    /// fold under the reply.
    pub(super) folded: Vec<Uuid>,
    pub(super) activity: Option<BotActivity>,
    /// It starts a run of its turn's messages, under its sender's name.
    pub(super) starts_run: bool,
    /// The turn whose first bot row this is, which shows what started it,
    /// such as the email behind a mail-triggered turn.
    pub(super) opens_turn: Option<Uuid>,
    /// The runs a [`RowKey::QuietRuns`] row stands for, oldest first.
    pub(super) quiet_runs: Vec<Uuid>,
    routine_status: Option<(String, bool)>,
    reaction: Option<crate::model::ReactionKind>,
    state: Option<String>,
}

impl TranscriptRow {
    pub(super) fn contains(&self, key: RowKey) -> bool {
        self.key == key
            || matches!(key, RowKey::Item(id) if self.folded.contains(&id))
            || matches!(key, RowKey::Item(id) if self.activity.as_ref().is_some_and(|activity| activity.contains(id)))
            || self
                .task
                .as_ref()
                .is_some_and(|task| task.cards.iter().any(|card| card.key == key))
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
                && !conversation.no_news(item.turn_id)
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

/// The transcript after folding routine runs that completed with no news.
#[derive(Debug, Default, PartialEq)]
struct QuietFold {
    rows: Vec<RowKey>,
    /// The runs each [`RowKey::QuietRuns`] row stands for, by its first run.
    groups: HashMap<Uuid, Vec<Uuid>>,
    /// Runs whose rows are folded away.
    hidden: HashSet<Uuid>,
}

/// Folds each no-news run to one header row, and consecutive no-news runs
/// of one routine to a single row, except the runs and groups in `open`.
fn fold_quiet(rows: Vec<RowKey>, c: &Conversation, open: &HashSet<RowKey>) -> QuietFold {
    let routine = |turn: Uuid| c.turn(turn)?.routine.as_ref().map(|run| run.routine_id);
    let quiet: Vec<_> = routine_turns(&rows, Some(c))
        .into_iter()
        .map(|turn| turn.filter(|&turn| c.no_news(turn)))
        .collect();
    let mut fold = QuietFold::default();
    let mut ix = 0;
    while ix < rows.len() {
        let Some(first) = quiet[ix] else {
            fold.rows.push(rows[ix]);
            ix += 1;
            continue;
        };
        let mut runs: Vec<(Uuid, Vec<RowKey>)> = Vec::new();
        while let Some(Some(turn)) = quiet.get(ix).copied()
            && routine(turn) == routine(first)
        {
            match runs.last_mut() {
                Some((last, keys)) if *last == turn => keys.push(rows[ix]),
                _ => runs.push((turn, vec![rows[ix]])),
            }
            ix += 1;
        }
        let grouped = runs.len() > 1;
        if grouped {
            fold.rows.push(RowKey::QuietRuns(first));
            fold.groups
                .insert(first, runs.iter().map(|(turn, _)| *turn).collect());
        }
        let turns: Vec<_> = runs.iter().map(|(turn, _)| *turn).collect();
        let shown = !grouped || group_open(&turns, open);
        for (turn, keys) in runs {
            if shown && open.contains(&RowKey::QuietRun(turn)) {
                fold.rows.extend(keys);
            } else {
                fold.hidden.insert(turn);
                if shown && !fold.rows.contains(&RowKey::QuietRun(turn)) {
                    fold.rows.push(RowKey::QuietRun(turn));
                }
            }
        }
    }
    fold
}

/// Whether a group of no-news runs lists its runs. It is keyed by its runs,
/// not its first one, so it stays open as older runs load or newer ones
/// join it, and an opened run keeps it open.
pub(super) fn group_open(runs: &[Uuid], open: &HashSet<RowKey>) -> bool {
    runs.iter().any(|&turn| {
        open.contains(&RowKey::QuietRuns(turn)) || open.contains(&RowKey::QuietRun(turn))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RoutineRow {
    pub(super) turn: Uuid,
    pub(super) first: bool,
    pub(super) last: bool,
}

/// The routine run each row belongs to, a task row's included.
fn routine_turns(rows: &[RowKey], conversation: Option<&Conversation>) -> Vec<Option<Uuid>> {
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
            for notice in &turn.owner_notices {
                turns.insert(RowKey::OwnerNotice(notice.id), turn.id);
            }
            for handoff in &turn.computer_handoffs {
                turns.insert(RowKey::ComputerHandoff(handoff.id), turn.id);
            }
        }
    }
    rows.iter()
        .map(|key| match *key {
            RowKey::Item(id) => conversation
                .and_then(|c| c.item(id).and_then(|item| c.turn(item.turn_id)))
                .filter(|turn| turn.routine.is_some())
                .map(|turn| turn.id),
            RowKey::Task(turn) => conversation
                .and_then(|c| c.turn(turn))
                .filter(|turn| turn.routine.is_some())
                .map(|turn| turn.id),
            RowKey::Question(_)
            | RowKey::SkillInstall(_)
            | RowKey::SecretRequest(_)
            | RowKey::Draft(_)
            | RowKey::SocialSetup(_)
            | RowKey::Handoff(_)
            | RowKey::ComputerHandoff(_)
            | RowKey::OwnerNotice(_) => turns.get(key).copied(),
            RowKey::Pending(_) | RowKey::Gap(_) | RowKey::QuietRun(_) | RowKey::QuietRuns(_) => {
                None
            }
        })
        .collect()
}

/// Each row's place in its routine run's block. A task row and a folded
/// no-news run name the run themselves.
fn routine_rows(rows: &[RowKey], conversation: Option<&Conversation>) -> Vec<Option<RoutineRow>> {
    let turns: Vec<_> = routine_turns(rows, conversation)
        .into_iter()
        .zip(rows)
        .map(|(turn, key)| turn.filter(|_| !matches!(key, RowKey::Task(_))))
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

/// The transcript's rows in reading order, before the owner's unsent
/// messages: each item, task and card where it reads. A task sits after its
/// anchor, which may be an item of another turn, such as the owner message
/// it works on; `requests` are cards with the item they follow.
pub(super) fn transcript_rows(
    c: &Conversation,
    tasks: &HashMap<Uuid, TaskReply>,
    mut requests: Vec<(RowKey, Option<Uuid>)>,
) -> Vec<RowKey> {
    let mut rows = Vec::new();
    let delegated = c.delegated_turns();
    // A received task's row sits where its latest answer landed, like a task card.
    let answered: HashMap<Uuid, Uuid> = c
        .items()
        .filter(|item| item.kind == "assistant" && delegated.contains(&item.turn_id))
        .map(|item| (item.turn_id, item.id))
        .collect();
    // A message joined into a reply anchors both its own queued turn and
    // the reply working on it; the message's own status reads first.
    let mut anchored: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for task in tasks.values() {
        if let Some(anchor) = task.anchor {
            anchored.entry(anchor).or_default().push(task.turn);
        }
    }
    for (anchor, turns) in &mut anchored {
        let own = c.item(*anchor).map(|item| item.turn_id);
        turns.sort_by_key(|&turn| (Some(turn) != own, c.turn(turn).map(|t| t.turn_no)));
    }
    for item in c.items() {
        if c.gap_below(item.seq).is_some() {
            rows.push(RowKey::Gap(item.seq));
        }
        if item.kind == "assistant" && answered.get(&item.turn_id) == Some(&item.id) {
            rows.extend(
                c.task_receipt(item.turn_id)
                    .map(|receipt| RowKey::Item(receipt.id)),
            );
        } else if item.reaction.is_none()
            && !(item.bot_task.is_some() && answered.contains_key(&item.turn_id))
            && (item.kind != "assistant"
                || tasks
                    .get(&item.turn_id)
                    .is_some_and(|reply| reply.answers.contains(&item.id))
                || !(tasks.contains_key(&item.turn_id) || delegated.contains(&item.turn_id)))
        {
            rows.push(RowKey::Item(item.id));
        }
        rows.extend(
            anchored
                .get(&item.id)
                .into_iter()
                .flatten()
                .map(|&turn| RowKey::Task(turn)),
        );
        requests.retain(|(key, anchor)| {
            let here = *anchor == Some(item.id);
            if here {
                rows.push(*key);
            }
            !here
        });
    }
    for turn in c.turns_in_order() {
        if tasks
            .get(&turn.id)
            .is_some_and(|reply| reply.anchor.is_none())
        {
            rows.push(RowKey::Task(turn.id));
        }
    }
    rows.extend(requests.into_iter().map(|(key, _)| key));
    rows
}

/// Whether each row starts a run of one turn's messages of the same kind,
/// which reads under its own sender name: a reply after a folded update of
/// its turn starts one.
pub(super) fn starts_runs(c: Option<&Conversation>, rows: &[RowKey]) -> Vec<bool> {
    let item = |key: &RowKey| match *key {
        RowKey::Item(id) => c?.item(id),
        _ => None,
    };
    rows.iter()
        .enumerate()
        .map(|(ix, key)| {
            let previous = ix.checked_sub(1).and_then(|ix| item(&rows[ix]));
            !previous
                .zip(item(key))
                .is_some_and(|(a, b)| a.kind == b.kind && a.turn_id == b.turn_id)
        })
        .collect()
}

/// The turn each row opens: a turn's first bot row, a plain reply or its
/// task card, carries the turn's trigger once however many rows it spans.
pub(super) fn opens_turns(c: Option<&Conversation>, rows: &[RowKey]) -> Vec<Option<Uuid>> {
    let mut seen = HashSet::new();
    rows.iter()
        .map(|key| {
            let turn = match *key {
                RowKey::Task(turn) => Some(turn),
                RowKey::Item(id) => c?
                    .item(id)
                    .filter(|item| item.kind == "assistant")
                    .map(|item| item.turn_id),
                _ => None,
            }?;
            seen.insert(turn).then_some(turn)
        })
        .collect()
}

/// The mail-triggered turn a row opens, whose emails it offers to view.
pub(super) fn opened_mail(c: &Conversation, opens: Option<Uuid>) -> Option<&crate::model::Turn> {
    c.turn(opens?).filter(|turn| !turn.mail.is_empty())
}

#[cfg(test)]
pub(super) fn request_rows(c: &Conversation) -> Vec<(RowKey, Option<Uuid>)> {
    turn_card_rows(c)
        .into_iter()
        .map(|(_, key, anchor)| (key, anchor))
        .collect()
}

/// Each card row with its turn and the item it follows.
pub(super) fn turn_card_rows(c: &Conversation) -> Vec<(Uuid, RowKey, Option<Uuid>)> {
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
                .chain(turn.skill_installs.iter().filter_map(move |install| {
                    let anchor = c.question_anchor(turn.id, install.settled_at);
                    (install.settled_at.is_none() || anchor.is_some())
                        .then_some((RowKey::SkillInstall(install.id), anchor))
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
                .chain(
                    turn.owner_notices
                        .iter()
                        .filter(|notice| {
                            notice.status != crate::model::OwnerNoticeStatus::Superseded
                        })
                        // A settled notice of a turn outside the loaded page
                        // would otherwise land below newer messages.
                        .filter_map(move |notice| {
                            let anchor = c.question_anchor(turn.id, None);
                            (notice.status == crate::model::OwnerNoticeStatus::Pending
                                || anchor.is_some())
                            .then_some((RowKey::OwnerNotice(notice.id), anchor))
                        }),
                )
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
                .map(move |(key, anchor)| (turn.id, key, anchor))
        })
        .collect()
}

fn card_state(c: &Conversation, key: RowKey) -> Option<String> {
    match key {
        RowKey::Question(id) => c.question(id).map(|q| q.status.clone()),
        RowKey::Draft(id) => c.outgoing_draft(id).map(|d| {
            format!(
                "{}:{}:{:?}:{}:{}",
                d.version, d.updated_at, d.status, d.can_approve, d.in_reply
            )
        }),
        RowKey::SkillInstall(id) => c.skill_install(id).map(|r| format!("{:?}", r.status)),
        RowKey::SecretRequest(id) => c.secret_request(id).map(|r| r.status.clone()),
        RowKey::SocialSetup(id) => c.social_setup(id).map(|s| s.status.clone()),
        RowKey::OwnerNotice(id) => c.owner_notice(id).map(|n| format!("{n:?}")),
        RowKey::ComputerHandoff(id) => c.computer_handoff(id).map(|h| format!("{:?}", h.status)),
        RowKey::Handoff(id) => c
            .handoff(id)
            .map(|(_, group)| format!("{:?}", group.iter().map(|d| d.status).collect::<Vec<_>>())),
        RowKey::Item(id) => c
            .item(id)
            .and_then(|i| c.turn(i.turn_id))
            .map(|t| t.status.clone()),
        RowKey::Task(id) => c.turn(id).map(|t| t.status.clone()),
        RowKey::Gap(above) => c.gap_below(above).map(|count| count.to_string()),
        RowKey::Pending(_) | RowKey::QuietRun(_) | RowKey::QuietRuns(_) => None,
    }
}

const BURST_GAP: TimeDelta = TimeDelta::minutes(5);

/// One timestamp per group of messages: messages from either side less than
/// [`BURST_GAP`] apart on one local day share the stamp of the last one.
fn burst_tail_stamps<Tz: TimeZone>(
    rows: &[(Option<Author>, Option<DateTime<Utc>>)],
    zone: &Tz,
) -> Vec<Option<DateTime<Utc>>> {
    let joins = |(author, at): (Option<Author>, Option<DateTime<Utc>>),
                 (next_author, next_at): (Option<Author>, Option<DateTime<Utc>>)| {
        let (Some(_), Some(_), Some(at), Some(next_at)) = (author, next_author, at, next_at) else {
            return false;
        };
        let gap = next_at - at;
        gap >= TimeDelta::zero()
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

/// Marks the first timed row of each local day, so the transcript names the
/// day once and its stamps can show only the time.
fn mark_days<Tz: TimeZone>(
    rows: &mut [TranscriptRow],
    times: &HashMap<RowKey, DateTime<Utc>>,
    zone: &Tz,
) {
    let mut last = None;
    for row in rows {
        let Some(&at) = times.get(&row.key) else {
            continue;
        };
        let date = at.with_timezone(zone).date_naive();
        if last != Some(date) {
            row.day = Some(at);
            last = Some(date);
        }
    }
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
    /// Where cards that can pause the bot were last painted, so the paused
    /// strip can step aside while one is on screen.
    pub(super) sightings: super::question_card::Sightings,
    /// The typed answer field on each pending single-choice card.
    pub(super) write_ins: HashMap<Uuid, super::question_card::WriteIn>,
    /// The parsed markdown of each question context that formats.
    pub(super) card_contexts:
        HashMap<(Uuid, super::question_card::Extent), super::question_card::FormattedContext>,
    /// The approval scope picked on each pending approval card.
    pub(super) approval_scopes: HashMap<Uuid, String>,
    pub(super) draft_forms: HashMap<Uuid, super::draft_card::DraftForm>,
    pub(super) draft_images: HashMap<Uuid, super::draft_card::DraftImages>,
    pub(super) social_forms: HashMap<Uuid, super::social_card::SocialCard>,
    pub(super) expanded_drafts: HashSet<Uuid>,
    pub(super) expanded_questions: HashSet<Uuid>,
    pub(super) expanded_tasks: HashSet<Uuid>,
    /// Replies whose folded earlier updates are open.
    pub(super) expanded_folds: HashSet<Uuid>,
    /// Delegation groups whose task details are expanded.
    pub(super) expanded_councils: HashSet<Uuid>,
    pub(super) expanded_events: HashSet<Uuid>,
    /// Bot exchanges the owner folded or unfolded, over how each shows by
    /// default.
    pub(super) exchange_folds: HashMap<Uuid, super::bot_activity::Fold>,
    /// The exchange the owner just unfolded: once laid out, its header moves
    /// to the top unless the whole exchange already shows.
    pub(super) landing: Option<Uuid>,
    /// Exchange replies shown in full instead of their excerpt.
    pub(super) full_replies: HashSet<Uuid>,
    /// No-news runs ([`RowKey::QuietRun`]) and groups of them
    /// ([`RowKey::QuietRuns`]) the owner opened.
    pub(super) open_quiet: HashSet<RowKey>,
    /// The waiting tray lists its waits instead of only its header.
    pub(super) tray_expanded: bool,
    /// Waits in the tray whose details are open.
    pub(super) expanded_waits: HashSet<Uuid>,
    /// Waits the owner asked to stop that the server has not settled yet.
    pub(super) stopping_waits: HashSet<Uuid>,
    pub(super) reply_transitions: HashMap<Uuid, std::time::Instant>,
    pub(super) _reply_clock: Option<Task<()>>,
    pub(super) latest_seen: bool,
    pub(super) reply_layout: Option<ReplyLayout>,
    /// Where the last "go to original message" landed.
    pub(super) revealed: Option<Reveal>,
    _reveal_clock: Option<Task<()>>,
    /// A quoted message to find once older history arrives.
    pending_reveal: Option<(RowKey, Author, String)>,
    /// The open find bar.
    pub(super) find: Option<super::find::Find>,
    /// The gap row being loaded, by the loaded item just above it.
    pub(super) loading_gap: Option<(i64, Task<()>)>,
    pub(super) load: Load,
    pub(super) _older: Option<Task<()>>,
}

/// Where a view stands with the server's newest page.
pub(super) enum Load {
    /// Its conversation, if any, is the server's and follows the stream.
    Settled,
    Running {
        _task: Task<()>,
    },
    /// The last load or stream event failed; a shown view retries after
    /// a pause, a warm one when shown again.
    Failed {
        _retry: Option<Task<()>>,
    },
}

/// A message the owner jumped to, flashed until it fades out.
pub(super) struct Reveal {
    /// The row that holds it.
    pub(super) row: RowKey,
    pub(super) at: std::time::Instant,
    pub(super) spot: Spot,
    /// How strongly the highlight paints, as of the last tick, so every
    /// paint until the next tick gives the same color.
    pub(super) shown: f32,
}

pub(super) enum Spot {
    /// The whole row, as for a card or a plain-text message.
    Row,
    /// The quoted text of a Markdown message.
    Passage {
        item: RowKey,
        /// The quoted part, or `None` for the whole message.
        quote: Option<String>,
        /// Whether the list was asked to center the passage, which waits for
        /// the message's Markdown to parse.
        centered: bool,
    },
}

/// How long a revealed message stays highlighted before it starts to fade.
const REVEAL_HOLD: Duration = Duration::from_millis(1200);
/// How long its highlight takes to fade out.
const REVEAL_FADE: Duration = Duration::from_millis(1600);

impl Reveal {
    pub(super) fn new(row: RowKey, spot: Spot) -> Self {
        Self {
            row,
            at: std::time::Instant::now(),
            spot,
            shown: 1.,
        }
    }

    /// How strongly to paint the highlight now, from 1 down to 0 once it
    /// has faded out.
    fn strength(&self) -> f32 {
        let fading = self.at.elapsed().saturating_sub(REVEAL_HOLD);
        let t = (fading.as_secs_f32() / REVEAL_FADE.as_secs_f32()).min(1.);
        1. - t * t * (3. - 2. * t)
    }
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
            sightings: Default::default(),
            write_ins: HashMap::new(),
            card_contexts: HashMap::new(),
            connections: HashMap::new(),
            approval_scopes: HashMap::new(),
            draft_forms: HashMap::new(),
            draft_images: HashMap::new(),
            social_forms: HashMap::new(),
            expanded_drafts: HashSet::new(),
            expanded_questions: HashSet::new(),
            expanded_tasks: HashSet::new(),
            expanded_folds: HashSet::new(),
            expanded_councils: HashSet::new(),
            expanded_events: HashSet::new(),
            exchange_folds: HashMap::new(),
            landing: None,
            full_replies: HashSet::new(),
            open_quiet: HashSet::new(),
            tray_expanded: true,
            expanded_waits: HashSet::new(),
            stopping_waits: HashSet::new(),
            reply_transitions: HashMap::new(),
            _reply_clock: None,
            latest_seen: false,
            reply_layout: None,
            revealed: None,
            _reveal_clock: None,
            pending_reveal: None,
            find: None,
            loading_gap: None,
            load: Load::Settled,
            _older: None,
        }
    }

    /// Settles what only the shown view finishes, before switching away:
    /// older pages and gaps that were loading, a pending jump and the
    /// unread divider. Its newest page keeps loading.
    pub(super) fn park(&mut self) {
        // Forms hold typed credentials and in-flight checks; showing the
        // view again builds them anew.
        self.connections.clear();
        self._reply_clock = None;
        self.unread_since = None;
        self._unread_dismissal = None;
        self.loading_older = false;
        self._older = None;
        self.loading_gap = None;
        self.pending_reveal = None;
        self.revealed = None;
        self._reveal_clock = None;
        self.landing = None;
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
        if let Some(bot) = self.view.as_ref().map(|view| view.bot_id) {
            self.load_into(bot, window, cx);
        }
    }

    /// Loads `bot`'s newest page into its view, shown or warm. A view with
    /// nothing to draw shows the saved page while the server's loads.
    pub(super) fn load_into(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let (client, saved) = (self.client.clone(), self.saved.clone());
        let Some(view) = self.view_of(bot) else {
            return;
        };
        let cold = view.conversation.is_none();
        view.load = Load::Running {
            _task: cx.spawn_in(window, async move |this, cx| {
                let fetch = cx.background_executor().spawn(client.latest(bot));
                if cold
                    && let Some(saved) = saved.clone()
                    && let Some(page) = cx
                        .background_executor()
                        .spawn(async move { saved.load(bot) })
                        .await
                {
                    let _ = this
                        .update_in(cx, |app, window, cx| app.apply_saved(bot, page, window, cx));
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
            }),
        };
    }

    /// Shows the saved page of a view still waiting for its first one.
    fn apply_saved(
        &mut self,
        bot: Uuid,
        page: Snapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = self.shows(bot);
        let Some(view) = self.view_of(bot).filter(|v| v.conversation.is_none()) else {
            return;
        };
        let Ok(conversation) = Conversation::from_saved(page) else {
            return;
        };
        view.conversation = Some(conversation);
        if shown {
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
        let shown = self.shows(bot);
        let view = self.view_of(bot)?;
        view.load = Load::Failed { _retry: None };
        match snapshot.map(Conversation::from_snapshot) {
            Ok(Ok(conversation)) => {
                let cursor = conversation.cursor();
                view.conversation = Some(conversation);
                view.load = Load::Settled;
                view.loading_gap = None;
                view.stream_problem = None;
                if !shown {
                    return None;
                }
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
                if shown {
                    view.load = Load::Failed {
                        _retry: Some(cx.spawn_in(window, async move |this, cx| {
                            cx.background_executor().timer(Duration::from_secs(2)).await;
                            let _ = this.update_in(cx, |app, window, cx| {
                                if app.shows(bot) {
                                    app.load_conversation(window, cx);
                                }
                            });
                        })),
                    };
                }
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
        let shown = self.shows(bot);
        let Some(view) = self.view_of(bot) else {
            return;
        };
        let failed = match message {
            StreamMessage::Frame(frame) => {
                let Some(conversation) = view.conversation.as_mut() else {
                    return;
                };
                match conversation.apply(&frame) {
                    Ok(Applied::Duplicate) => false,
                    // The bot list stays authoritative for a warm bot's
                    // activity: its last frames can trail the list.
                    Ok(Applied::Changed) if !shown => {
                        self.prune_pending(bot);
                        false
                    }
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
        } else if let Some(view) = self.view_of(bot)
            && matches!(view.load, Load::Settled)
        {
            view.load = Load::Failed { _retry: None };
        }
    }

    /// Drops `bot`'s pending sends its conversation now holds.
    pub(super) fn prune_pending(&mut self, bot: Uuid) {
        let view = match self.view.as_ref() {
            Some(view) if view.bot_id == bot => Some(view),
            _ => self.warm.get(bot),
        };
        if let (Some(conversation), Some(pending)) = (
            view.and_then(|v| v.conversation.as_ref()),
            self.pending.get_mut(&bot),
        ) {
            pending.retain(|p| !conversation.has_request(p.request_id));
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
        let older = conversation.older_before_seq.is_some();
        let found = target.and_then(|target| Some((target, self.unfold_item(target, window, cx)?)));
        let Some(view) = self.view.as_mut() else {
            return;
        };
        if let Some((target, ix)) = found
            && let Some(conversation) = view.conversation.as_ref()
        {
            view.list.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: px(0.),
            });
            let item = RowKey::Item(target);
            let spot = if view.texts.get(&item).is_some_and(RowText::is_markdown) {
                let whole = conversation
                    .item(target)
                    .is_some_and(|item| super::reply::quotes_whole(&item.text, original));
                Spot::Passage {
                    item,
                    quote: (!whole).then(|| original.to_owned()),
                    centered: false,
                }
            } else {
                Spot::Row
            };
            let row = view.rows[ix].key;
            self.start_reveal(Reveal::new(row, spot), cx);
        } else if older {
            view.pending_reveal = Some((from, author, original.to_owned()));
            self.load_older(window, cx);
        } else {
            window.push_notification(
                "The original message is no longer in this conversation.",
                cx,
            );
        }
    }

    /// Shows item `target` whole, opening a folded bot exchange or a task's
    /// earlier replies, and returns the index of the row that holds it.
    pub(super) fn unfold_item(
        &mut self,
        target: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.open_quiet_run_of(target, window, cx);
        let view = self.view.as_mut()?;
        let conversation = view.conversation.as_ref()?;
        // An exchange reply may be folded or cut to an excerpt.
        let folded = !view.full_replies.contains(&target)
            && view.rows.iter().any(|row| {
                row.activity
                    .as_ref()
                    .is_some_and(|activity| activity.answers.contains(&target))
            });
        if folded {
            if let Some(receipt) = conversation
                .item(target)
                .and_then(|item| conversation.task_receipt(item.turn_id))
            {
                view.exchange_folds
                    .insert(receipt.id, super::bot_activity::Fold::Open);
            }
            view.full_replies.insert(target);
            self.sync_rows(window, cx);
        }
        let view = self.view.as_mut()?;
        let conversation = view.conversation.as_ref()?;
        let ix = view.row_index(RowKey::Item(target))?;
        if let RowKey::Item(reply) = view.rows[ix].key
            && view.rows[ix].folded.contains(&target)
        {
            view.expanded_folds.insert(reply);
            view.list.remeasure_items(ix..ix + 1);
        }
        if let Some(task) = &view.rows[ix].task
            && (task.history.contains(&target)
                || (task.kept.contains(&target) && task.reply_superseded(conversation, target))
                || (task.latest == Some(target)
                    && !task.latest_kept
                    && matches!(task.state, ReplyState::Queued | ReplyState::Working)))
        {
            view.expanded_tasks.insert(task.turn);
            view.list.remeasure_items(ix..ix + 1);
        }
        Some(ix)
    }

    /// Opens the no-news run holding item `target`, and the group of runs
    /// it folds into, so a find match inside it shows.
    fn open_quiet_run_of(&mut self, target: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let Some(turn) = view
            .conversation
            .as_ref()
            .and_then(|c| c.item(target))
            .map(|item| item.turn_id)
            .filter(|&turn| view.conversation.as_ref().is_some_and(|c| c.no_news(turn)))
        else {
            return;
        };
        if view.open_quiet.insert(RowKey::QuietRun(turn)) {
            self.sync_rows(window, cx);
        }
    }

    /// Shows or folds a no-news run, or a group of them.
    pub(super) fn toggle_quiet(
        &mut self,
        key: RowKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let runs = view
            .rows
            .iter()
            .find(|row| row.key == key)
            .map(|row| row.quiet_runs.clone())
            .unwrap_or_default();
        if !runs.is_empty() && group_open(&runs, &view.open_quiet) {
            for turn in runs {
                view.open_quiet.remove(&RowKey::QuietRuns(turn));
                view.open_quiet.remove(&RowKey::QuietRun(turn));
            }
        } else if !view.open_quiet.remove(&key) {
            view.open_quiet.insert(key);
        }
        self.sync_rows(window, cx);
        // The rows below a folded or unfolded run keep their keys, but their
        // spacing depends on the row above.
        if let Some(view) = self.view.as_ref() {
            view.list.remeasure();
        }
    }

    /// Flashes `reveal` until it fades out, painting its passage on each
    /// frame and asking the list to center the passage once it is found.
    pub(super) fn start_reveal(&mut self, reveal: Reveal, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let previous = view.revealed.replace(reveal);
        if let Some(Reveal {
            spot: Spot::Passage { item, .. },
            ..
        }) = previous
        {
            self.paint_row_quotes(item, cx);
        }
        self.tick_reveal(cx);
        let Some(view) = self.view.as_mut() else {
            return;
        };
        view._reveal_clock = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let live = this.update(cx, |app, cx| app.tick_reveal(cx));
                if !live.unwrap_or(false) {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Repaints the revealed passage, and drops the reveal once it has faded
    /// out. Returns whether it is still showing.
    fn tick_reveal(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(view) = self.view.as_mut() else {
            return false;
        };
        let Some(reveal) = view.revealed.as_mut() else {
            return false;
        };
        reveal.shown = reveal.strength();
        let live = reveal.shown > 0.;
        let passage = match &reveal.spot {
            Spot::Passage { item, .. } => Some(*item),
            Spot::Row => None,
        };
        if !live {
            view.revealed = None;
        }
        match passage {
            Some(item) => self.paint_row_quotes(item, cx),
            None => cx.notify(),
        }
        live
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
        self.sync_settings_changes();
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
        let mut tasks = conversation.map(task_replies).unwrap_or_default();
        let mut texts = Vec::new();
        let mut requests = conversation
            .map(|c| place_cards(c, &mut tasks, turn_card_rows(c)))
            .unwrap_or_default();
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
        let QuietFold {
            mut rows,
            groups,
            hidden,
        } = conversation
            .map(|c| fold_quiet(transcript_rows(c, &tasks, requests), c, &view.open_quiet))
            .unwrap_or_default();
        let delegated = conversation
            .map(Conversation::delegated_turns)
            .unwrap_or_default();
        for item in conversation
            .into_iter()
            .flat_map(|c| c.items())
            .filter(|item| !hidden.contains(&item.turn_id))
        {
            let format = RowFormat::for_item(item);
            // A kept task reply leaves out a file a later reply delivers again.
            let repeated = tasks.get(&item.turn_id).map(|reply| &reply.repeated_files);
            let shown = repeated.and_then(|ids| {
                crate::attachments::without_files(&item.parts, |file| ids.contains(&file.id))
            });
            let (text, parts) = match (&item.bot_task, shown) {
                (Some(task), _) => (task.prompt.clone(), item.parts.clone()),
                (None, Some(shown)) => shown,
                (None, None) => (item.text.clone(), item.parts.clone()),
            };
            // A bot exchange's reply reads inline as an excerpt until the
            // owner asks for all of it.
            let exchange_reply = item.bot_reply.is_some()
                || (item.kind == "assistant" && delegated.contains(&item.turn_id));
            let (text, parts) = if exchange_reply && !view.full_replies.contains(&item.id) {
                super::bot_activity::excerpt_message(&text, &parts).unwrap_or((text, parts))
            } else {
                (text, parts)
            };
            texts.push((RowKey::Item(item.id), text, format, parts));
        }
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
                | RowKey::ComputerHandoff(_)
                | RowKey::OwnerNotice(_)
                | RowKey::Gap(_) => (None, None),
                // A folded run dates the day it ran, under no sender.
                RowKey::QuietRun(turn) | RowKey::QuietRuns(turn) => (
                    None,
                    conversation
                        .and_then(|c| c.turn(turn)?.routine.as_ref())
                        .map(|run| run.due_at),
                ),
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
        let times: HashMap<RowKey, DateTime<Utc>> = rows
            .iter()
            .zip(&senders)
            .filter_map(|(key, (_, at))| Some((*key, (*at)?)))
            .collect();
        let rows: Vec<TranscriptRow> = rows
            .into_iter()
            .zip(burst_tail_stamps(&senders, &Local))
            .zip(routines)
            .map(|((key, footer_stamp), routine)| {
                let routine_turn = routine.and_then(|r| conversation?.turn(r.turn));
                let routine_status = routine_turn.map(super::message_row::turn_label);
                let state = conversation.and_then(|c| match key {
                    // A task row also changes when a card inside it does.
                    RowKey::Task(id) => c.turn(id).map(|t| {
                        tasks
                            .get(&id)
                            .into_iter()
                            .flat_map(|task| &task.cards)
                            .filter_map(|card| card_state(c, card.key))
                            .fold(t.status.clone(), |state, card| state + ":" + &card)
                    }),
                    _ => card_state(c, key),
                });
                TranscriptRow {
                    key,
                    unread_start: Some(key) == unread_start,
                    footer_stamp: footer_stamp.filter(|_| routine.is_none()),
                    day: None,
                    routine,
                    quiet_runs: match key {
                        RowKey::QuietRuns(first) => groups.get(&first).cloned().unwrap_or_default(),
                        _ => Vec::new(),
                    },
                    routine_status,
                    reaction: match key {
                        RowKey::Item(id) => conversation.and_then(|c| c.reaction_for(id)),
                        _ => None,
                    },
                    task: match key {
                        RowKey::Task(id) => tasks.get(&id).cloned(),
                        _ => None,
                    },
                    folded: match key {
                        RowKey::Item(id) => conversation
                            .and_then(|c| c.item(id))
                            .and_then(|item| tasks.get(&item.turn_id)?.folds.get(&id))
                            .cloned()
                            .unwrap_or_default(),
                        _ => Vec::new(),
                    },
                    activity: None,
                    starts_run: true,
                    opens_turn: None,
                    state,
                }
            })
            .collect();
        let mut rows = group_activity(rows, conversation);
        let keys: Vec<_> = rows.iter().map(|row| row.key).collect();
        for ((row, starts), opens) in rows
            .iter_mut()
            .zip(starts_runs(conversation, &keys))
            .zip(opens_turns(conversation, &keys))
        {
            row.starts_run = starts;
            row.opens_turn = opens;
        }
        mark_days(&mut rows, &times, &Local);
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
        self.sync_card_contexts(cx);
        self.sync_connections(window, cx);
        self.sync_draft_forms(window, cx);
        self.sync_social_forms(window, cx);
        self.check_reading(window, cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConversationView, REVEAL_FADE, REVEAL_HOLD, Reveal, RoutineRow, RowKey, Spot,
        TranscriptRow, burst_tail_stamps, first_unread_row, fold_quiet, opened_mail, opens_turns,
        request_rows, routine_rows, transcript_rows,
    };
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    fn reveal_from(ago: Duration) -> Reveal {
        Reveal {
            row: RowKey::Item(Uuid::nil()),
            at: Instant::now() - ago,
            spot: Spot::Row,
            shown: 1.,
        }
    }

    #[test]
    fn a_reveal_holds_its_highlight_then_fades_it_out() {
        assert_eq!(reveal_from(Duration::ZERO).strength(), 1.);
        assert_eq!(reveal_from(REVEAL_HOLD / 2).strength(), 1.);
        let fading = reveal_from(REVEAL_HOLD + REVEAL_FADE / 2).strength();
        assert!(
            fading > 0. && fading < 1.,
            "half way through the fade: {fading}"
        );
        assert_eq!(reveal_from(REVEAL_HOLD + REVEAL_FADE).strength(), 0.);
    }
    use crate::api::ReplyPosition;
    use crate::app::reply::Author::{self, Bot, Owner};
    use chrono::{DateTime, FixedOffset, Utc};

    use crate::model::{Conversation, Snapshot};
    use serde_json::{Value, json};
    use uuid::Uuid;

    fn routine_conversation() -> Conversation {
        Conversation::from_snapshot(serde_json::from_value(routine_snapshot()).unwrap()).unwrap()
    }

    fn routine_snapshot() -> Value {
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
        json!({
            "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
            "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
            "items": items, "turns": [turn(10), turn(11)],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
        })
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
    fn owner_notice_rows_stay_with_the_delegated_turn_after_settlement() {
        let receipt = Uuid::from_u128(1);
        for status in ["pending", "replied", "dismissed"] {
            let mut page = card_page(
                "sent",
                json!([{
                    "id": receipt, "seq": 1, "turn_id": Uuid::from_u128(10),
                    "kind": "user", "text": "Task received",
                    "bot_task": {"sender_bot_id": Uuid::from_u128(90),
                        "sender_name": "Sentry", "prompt": "Triage checkout"}
                }]),
                None,
            );
            page["turns"][0]["outgoing_drafts"] = json!([]);
            page["turns"][0]["owner_notices"] = json!([{
                "id": Uuid::from_u128(400), "bot_id": Uuid::from_u128(80),
                "turn_id": Uuid::from_u128(10), "kind": "blocked",
                "summary": "Start a Dev session", "tried": "Delegate to Dev",
                "status": status, "created_at": "2026-11-01T00:00:01Z",
                "settled_at": if status == "pending" { None } else { Some("2026-11-01T00:01:00Z") }
            }]);
            let c = Conversation::from_snapshot(serde_json::from_value(page).unwrap()).unwrap();
            assert_eq!(
                request_rows(&c),
                [(RowKey::OwnerNotice(Uuid::from_u128(400)), Some(receipt))]
            );
            assert_eq!(
                c.owner_notice(Uuid::from_u128(400)).unwrap().summary,
                "Start a Dev session"
            );
        }
    }

    #[test]
    fn a_settled_owner_notice_outside_the_page_does_not_drop_below_newer_messages() {
        for (status, shown) in [("pending", true), ("dismissed", false)] {
            let mut page = card_page("sent", json!([]), Some(5));
            page["turns"][0]["outgoing_drafts"] = json!([]);
            page["turns"][0]["owner_notices"] = json!([{
                "id": Uuid::from_u128(400), "bot_id": Uuid::from_u128(80),
                "turn_id": Uuid::from_u128(10), "kind": "blocked",
                "summary": "Start a Dev session", "tried": null, "status": status,
                "created_at": "2026-11-01T00:00:01Z",
                "settled_at": if status == "pending" { None } else { Some("2026-11-01T00:01:00Z") }
            }]);
            let rows = card_rows(page);
            assert_eq!(rows.len(), usize::from(shown), "{status}: {rows:?}");
        }
    }

    #[test]
    fn a_superseded_owner_notice_leaves_its_card_to_ask() {
        let mut page = card_page("sent", json!([]), None);
        page["turns"][0]["outgoing_drafts"] = json!([]);
        page["turns"][0]["owner_notices"] = json!([{
            "id": Uuid::from_u128(400), "bot_id": Uuid::from_u128(80),
            "turn_id": Uuid::from_u128(10), "kind": "decision",
            "summary": "Roll back or hotfix?", "tried": null, "status": "superseded",
            "created_at": "2026-11-01T00:00:01Z", "settled_at": "2026-11-01T00:00:02Z"
        }]);
        let c = Conversation::from_snapshot(serde_json::from_value(page).unwrap()).unwrap();
        assert!(request_rows(&c).is_empty());
    }

    #[test]
    fn owner_notice_field_is_optional_on_older_servers() {
        let snapshot: Snapshot =
            serde_json::from_value(card_page("pending", json!([]), None)).unwrap();
        assert!(snapshot.turns[0].owner_notices.is_empty());
    }

    #[test]
    fn owner_request_provenance_comes_from_the_task_receipt() {
        let mut page = card_page(
            "pending",
            json!([{
                "id": Uuid::from_u128(1), "seq": 1, "turn_id": Uuid::from_u128(10),
                "kind": "user", "text": "Task received",
                "bot_task": {"sender_bot_id": Uuid::from_u128(90),
                    "sender_name": "Sentry", "prompt": "Triage checkout"}
            }]),
            None,
        );
        page["turns"][0]["questions"] = json!([{
            "id": Uuid::from_u128(210), "kind": "external", "title": "Restart?",
            "choices": [], "status": "pending"
        }]);
        let c = Conversation::from_snapshot(serde_json::from_value(page).unwrap()).unwrap();
        assert_eq!(
            RowKey::Draft(Uuid::from_u128(310)).task_sender(&c),
            Some("Sentry")
        );
        assert_eq!(
            RowKey::Question(Uuid::from_u128(210)).task_sender(&c),
            Some("Sentry")
        );
        assert_eq!(RowKey::Question(Uuid::nil()).task_sender(&c), None);
        assert_eq!(RowKey::Item(Uuid::from_u128(1)).task_sender(&c), None);
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
        let waiting: Vec<_> = c.waiting_on().iter().map(|w| w.id()).collect();
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

    #[test]
    fn a_gap_row_stands_above_the_message_that_follows_the_missing_ones() {
        let message = |seq: i64| {
            json!({
                "id": Uuid::from_u128(seq as u128), "seq": seq,
                "turn_id": Uuid::from_u128(seq as u128 + 100), "kind": "user",
                "text": format!("Message {seq}")
            })
        };
        let mut c = Conversation::from_snapshot(serde_json::from_value(json!({
            "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
            "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
            "items": [message(90)], "turns": [], "older_before_seq": 90,
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
        })).unwrap()).unwrap();
        let window = json!({
            "items": [message(10), message(11)], "turns": [], "older_gap": 0, "newer_gap": 40,
        });
        c.merge_window(serde_json::from_value(window).unwrap(), None, Some(90));
        assert_eq!(
            transcript_rows(&c, &Default::default(), Vec::new()),
            [item(10), item(11), RowKey::Gap(90), item(90)]
        );
    }

    /// Turn 10 with the bot's replies at `replies` and owner messages joined
    /// into it at `joined`. With `mail`, an email started it; the server
    /// sends no item for the email itself.
    fn mail_turn(mail: bool, status: &str, replies: &[i64], joined: &[i64]) -> Conversation {
        let message = |seq: i64, turn: u128, kind: &str| {
            json!({
                "id": Uuid::from_u128(seq as u128), "seq": seq,
                "turn_id": Uuid::from_u128(turn), "kind": kind, "text": format!("Message {seq}")
            })
        };
        let turn = |id: u128, status: &str| {
            json!({
                "id": Uuid::from_u128(id), "request_id": Uuid::from_u128(id + 100),
                "turn_no": id, "provider_id": "codex", "status": status,
                "effect_uncertain": false
            })
        };
        let mut opened = turn(10, status);
        if mail {
            opened["mail"] = json!([{
                "account_id": Uuid::from_u128(50),
                "message": { "folder": "INBOX", "uid_validity": 1, "uid": 7 }
            }]);
        }
        let mut items = Vec::new();
        let mut turns = vec![opened];
        for &seq in replies {
            items.push(message(seq, 10, "assistant"));
        }
        for &seq in joined {
            items.push(message(seq, seq as u128 + 10, "user"));
            let mut steered = turn(seq as u128 + 10, "queued");
            steered["steered_into"] = json!(Uuid::from_u128(10));
            turns.push(steered);
        }
        items.sort_by_key(|item| item["seq"].as_i64());
        Conversation::from_snapshot(serde_json::from_value(json!({
            "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
            "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
            "items": items, "turns": turns,
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
        })).unwrap()).unwrap()
    }

    /// The rows that offer to view an email.
    fn mail_rows(c: &Conversation) -> Vec<RowKey> {
        let tasks = crate::app::task_reply::state::task_replies(c);
        let rows = transcript_rows(c, &tasks, Vec::new());
        rows.iter()
            .zip(opens_turns(Some(c), &rows))
            .filter(|(_, opens)| opened_mail(c, *opens).is_some())
            .map(|(key, _)| *key)
            .collect()
    }

    #[test]
    fn a_mail_turn_with_one_reply_shows_its_email_on_the_reply() {
        assert_eq!(
            mail_rows(&mail_turn(true, "completed", &[2], &[])),
            [item(2)]
        );
    }

    #[test]
    fn a_mail_turn_shown_as_a_task_card_shows_its_email_on_the_card() {
        let task = RowKey::Task(Uuid::from_u128(10));
        for (status, replies) in [
            ("completed", &[2, 3][..]),
            ("running", &[2][..]),
            ("failed", &[][..]),
            ("interrupted", &[2][..]),
        ] {
            assert_eq!(
                mail_rows(&mail_turn(true, status, replies, &[])),
                [task],
                "a {status} turn with replies {replies:?}"
            );
        }
    }

    #[test]
    fn a_mail_turn_that_joined_owner_messages_shows_its_email_once() {
        for status in ["completed", "running"] {
            assert_eq!(
                mail_rows(&mail_turn(true, status, &[2, 4, 6], &[3, 5])),
                [item(2)],
                "a {status} turn"
            );
        }
    }

    #[test]
    fn a_turn_no_email_started_offers_none() {
        assert_eq!(mail_rows(&mail_turn(false, "completed", &[2, 3], &[])), []);
    }

    fn transcript_row(key: RowKey) -> TranscriptRow {
        TranscriptRow {
            key,
            unread_start: false,
            footer_stamp: None,
            day: None,
            routine: None,
            task: None,
            folded: Vec::new(),
            activity: None,
            starts_run: true,
            opens_turn: None,
            quiet_runs: Vec::new(),
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

    /// Completed runs of routine 99: turns 10 to 13 with items 1 to 5,
    /// all with no news except the turns in `news`.
    fn quiet_conversation(news: &[u128]) -> Conversation {
        let turn = |id: u128| {
            json!({
                "id": Uuid::from_u128(id), "request_id": Uuid::from_u128(id + 100),
                "turn_no": id, "provider_id": "codex", "status": "completed",
                "error": null, "effect_uncertain": false,
                "routine": { "routine_id": Uuid::from_u128(99), "name": "Sentry tickets",
                             "due_at": "2026-11-05T04:00:00Z", "no_news": !news.contains(&id) }
            })
        };
        let items: Vec<_> = [(1, 10), (2, 10), (3, 11), (4, 12), (5, 13)]
            .into_iter()
            .map(|(id, turn)| {
                json!({
                    "id": Uuid::from_u128(id), "seq": id, "turn_id": Uuid::from_u128(turn),
                    "kind": "assistant", "text": "Nothing to hand off or merge.",
                    "provider_id": "codex"
                })
            })
            .collect();
        Conversation::from_snapshot(
            serde_json::from_value(json!({
                "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
                "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
                "items": items, "turns": [turn(10), turn(11), turn(12), turn(13)],
                "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
            }))
            .unwrap(),
        )
        .unwrap()
    }

    fn quiet_rows(c: &Conversation, open: &[RowKey]) -> Vec<RowKey> {
        let tasks = crate::app::task_reply::state::task_replies(c);
        let rows = transcript_rows(c, &tasks, Vec::new());
        fold_quiet(rows, c, &open.iter().copied().collect()).rows
    }

    fn task(id: u128) -> RowKey {
        RowKey::Task(Uuid::from_u128(id))
    }

    #[test]
    fn a_run_with_no_news_shows_only_its_header_until_opened() {
        let c = quiet_conversation(&[11, 12, 13]);
        let run = RowKey::QuietRun(Uuid::from_u128(10));
        assert_eq!(quiet_rows(&c, &[]), [run, task(11), task(12), task(13)]);
        assert_eq!(
            quiet_rows(&c, &[run]),
            [task(10), task(11), task(12), task(13)],
            "opening the run shows its replies"
        );
        let read = ReplyPosition {
            item_seq: 0,
            text_bytes: 0,
        };
        assert_eq!(
            first_unread_row(&[item(1), item(3)], &c, read),
            Some(item(3))
        );
    }

    #[test]
    fn consecutive_runs_with_no_news_fold_into_one_row() {
        let c = quiet_conversation(&[13]);
        let group = RowKey::QuietRuns(Uuid::from_u128(10));
        let run = |id| RowKey::QuietRun(Uuid::from_u128(id));
        let fold = fold_quiet(
            transcript_rows(
                &c,
                &crate::app::task_reply::state::task_replies(&c),
                Vec::new(),
            ),
            &c,
            &HashSet::new(),
        );
        assert_eq!(fold.rows, [group, task(13)]);
        assert_eq!(
            fold.groups[&Uuid::from_u128(10)],
            [10, 11, 12].map(Uuid::from_u128)
        );
        assert_eq!(
            quiet_rows(&c, &[group]),
            [group, run(10), run(11), run(12), task(13)],
            "opening the group lists its runs"
        );
        assert_eq!(
            quiet_rows(&c, &[group, run(11)]),
            [group, run(10), task(11), run(12), task(13)]
        );
        assert_eq!(
            quiet_rows(&c, &[run(11)]),
            [group, run(10), task(11), run(12), task(13)],
            "an opened run keeps its group open"
        );
        assert_eq!(
            quiet_rows(&c, &[RowKey::QuietRuns(Uuid::from_u128(11))]),
            [group, run(10), run(11), run(12), task(13)],
            "a group stays open when an older run loads above its first"
        );
    }

    #[test]
    fn a_silent_run_is_never_the_latest_reply() {
        let c = quiet_conversation(&[10]);
        assert_eq!(
            c.latest_reply().map(|item| item.id),
            Some(Uuid::from_u128(2))
        );
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
            day: None,
            routine: None,
            task: None,
            folded: Vec::new(),
            activity: None,
            starts_run: true,
            opens_turn: None,
            quiet_runs: Vec::new(),
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
    fn a_five_minute_gap_starts_a_new_stamp() {
        let rows = [
            (Some(Bot), Some("10:00:00")),
            (Some(Bot), Some("10:05:00")),
            (Some(Owner), Some("10:10:00")),
        ];
        assert_eq!(
            stamps(&rows),
            [
                Some(at("10:00:00")),
                Some(at("10:05:00")),
                Some(at("10:10:00")),
            ]
        );
    }

    #[test]
    fn an_owner_message_and_its_quick_reply_share_one_stamp() {
        let rows = [
            (Some(Owner), Some("10:00:00")),
            (Some(Bot), Some("10:00:20")),
            (Some(Owner), Some("10:01:00")),
        ];
        assert_eq!(stamps(&rows), [None, None, Some(at("10:01:00"))]);
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

    #[test]
    fn the_first_row_of_each_viewer_day_names_that_day() {
        let keys = [1, 2, 3, 4].map(|n| RowKey::Item(Uuid::from_u128(n)));
        let mut rows: Vec<_> = keys.iter().map(|&key| transcript_row(key)).collect();
        rows.insert(2, transcript_row(RowKey::Question(Uuid::from_u128(9))));
        let times: std::collections::HashMap<_, _> = [
            (keys[0], at("21:00:00")),
            (keys[1], at("21:30:00")),
            (keys[2], at("22:30:00")),
            (keys[3], at("23:00:00")),
        ]
        .into();
        let plus_two = FixedOffset::east_opt(2 * 3600).unwrap();
        super::mark_days(&mut rows, &times, &plus_two);
        let days: Vec<_> = rows.iter().map(|row| row.day).collect();
        assert_eq!(
            days,
            [Some(at("21:00:00")), None, None, Some(at("22:30:00")), None]
        );
    }
}
