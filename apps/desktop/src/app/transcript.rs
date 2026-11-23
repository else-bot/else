use super::bot_activity::{BotActivity, group_activity};
use super::pending_send::PendingSend;
use super::reply::Author;
use super::row_text::{RowFormat, RowSource};
use super::task_reply::state::{TaskReply, place_cards, task_replies};
use crate::model::{Conversation, GapSize};
use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use else_wire::bots::ReplyPosition;
use else_wire::computers::HandoffStatus;
use else_wire::delegations::DelegationStatus;
use else_wire::items::ItemKind;
use else_wire::notices::OwnerNotice;
use else_wire::notices::OwnerNoticeStatus;
use else_wire::outgoing::DraftStatus;
use else_wire::questions::QuestionStatus;
use else_wire::secrets::SecretRequestStatus;
use else_wire::skills::SkillInstallStatus;
use else_wire::socials::SetupStatus;
use else_wire::turns::TurnStatus;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum RowKey {
    Item(Uuid),
    Task(Uuid),
    Pending(Uuid),
    Card(CardKey),
    /// Unloaded messages just below the loaded item with this seq.
    Gap(i64),
    /// A routine run that completed with no news, folded to its header.
    QuietRun(Uuid),
    /// Consecutive no-news runs of one routine, by the first run's turn.
    QuietRuns(Uuid),
}

impl RowKey {
    pub(super) fn task_sender(self, c: &Conversation) -> Option<&str> {
        let Self::Card(card) = self else {
            return None;
        };
        if !card.names_sender() {
            return None;
        }
        Some(
            &c.task_receipt(card.turn(c)?.id)?
                .bot_task
                .as_ref()?
                .sender_name,
        )
    }
}

/// A card the bot raised in a turn for the owner to see or act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum CardKey {
    SecretRequest(Uuid),
    SkillInstall(Uuid),
    Question(Uuid),
    Draft(Uuid),
    SocialSetup(Uuid),
    /// The first task of a provider attempt's delegations.
    Handoff(Uuid),
    ComputerHandoff(Uuid),
    OwnerNotice(Uuid),
}

impl CardKey {
    /// Whether it draws as a bordered card, rather than a status line.
    pub(super) fn is_boxed(self) -> bool {
        !matches!(self, Self::Handoff(_) | Self::ComputerHandoff(_))
    }

    /// Whether it waits on the owner's answer.
    pub(super) fn needs_owner(self) -> bool {
        matches!(
            self,
            Self::Question(_) | Self::SecretRequest(_) | Self::Draft(_) | Self::OwnerNotice(_)
        )
    }

    /// Whether it names the bot whose task raised it.
    fn names_sender(self) -> bool {
        !matches!(
            self,
            Self::SkillInstall(_) | Self::SocialSetup(_) | Self::OwnerNotice(_)
        )
    }

    /// The turn that raised it.
    pub(super) fn turn(self, c: &Conversation) -> Option<&else_wire::turns::Turn> {
        c.turns_in_order()
            .into_iter()
            .find(|turn| Self::raised_by(turn).any(|card| card == self))
    }

    /// Every card `turn` raised.
    fn raised_by(turn: &else_wire::turns::Turn) -> impl Iterator<Item = Self> + '_ {
        (turn
            .secret_requests
            .iter()
            .map(|r| Self::SecretRequest(r.id)))
        .chain(turn.skill_installs.iter().map(|i| Self::SkillInstall(i.id)))
        .chain(turn.questions.iter().map(|q| Self::Question(q.id)))
        .chain(turn.outgoing_drafts.iter().map(|d| Self::Draft(d.id)))
        .chain(turn.social_setups.iter().map(|s| Self::SocialSetup(s.id)))
        .chain(turn.delegations.iter().map(|d| Self::Handoff(d.id)))
        .chain(
            turn.computer_handoffs
                .iter()
                .map(|h| Self::ComputerHandoff(h.id)),
        )
        .chain(turn.owner_notices.iter().map(|n| Self::OwnerNotice(n.id)))
    }
}

#[derive(Debug, Clone, PartialEq)]
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
    reaction: Option<else_wire::items::ReactionKind>,
    state: Option<RowState>,
}

impl TranscriptRow {
    pub(super) fn contains(&self, key: RowKey) -> bool {
        self.key == key
            || matches!(key, RowKey::Item(id) if self.folded.contains(&id))
            || matches!(key, RowKey::Item(id) if self.activity.as_ref().is_some_and(|activity| activity.contains(id)))
            || self
                .task
                .as_ref()
                .is_some_and(|task| task.cards.iter().any(|card| RowKey::Card(card.key) == key))
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
            item.kind == ItemKind::Assistant
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
    let cards: HashMap<CardKey, Uuid> = conversation
        .into_iter()
        .flat_map(|c| c.turns_in_order())
        .filter(|turn| turn.routine.is_some())
        .flat_map(|turn| CardKey::raised_by(turn).map(|card| (card, turn.id)))
        .collect();
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
            RowKey::Card(card) => cards.get(&card).copied(),
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
        .filter(|item| item.kind == ItemKind::Assistant && delegated.contains(&item.turn_id))
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
    // A task with no shown message, such as a mail turn that failed before
    // replying, sits at the time it finished, which also dates it; an
    // unfinished one stays last.
    let (mut finished, unfinished): (Vec<_>, Vec<_>) = c
        .turns_in_order()
        .into_iter()
        .filter(|turn| {
            tasks
                .get(&turn.id)
                .is_some_and(|reply| reply.anchor.is_none())
        })
        .partition(|turn| turn.finished_at.is_some());
    finished.sort_by_key(|turn| turn.finished_at);
    let mut finished = finished.into_iter().peekable();
    for item in c.items() {
        if let Some(at) = item.created_at {
            while let Some(turn) = finished.next_if(|turn| turn.finished_at < Some(at)) {
                rows.push(RowKey::Task(turn.id));
            }
        }
        if c.gap_below(item.seq).is_some() {
            rows.push(RowKey::Gap(item.seq));
        }
        if item.kind == ItemKind::Assistant && answered.get(&item.turn_id) == Some(&item.id) {
            rows.extend(
                c.task_receipt(item.turn_id)
                    .map(|receipt| RowKey::Item(receipt.id)),
            );
        } else if item.reaction.is_none()
            && !(item.bot_task.is_some() && answered.contains_key(&item.turn_id))
            && (item.kind != ItemKind::Assistant
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
    rows.extend(finished.chain(unfinished).map(|turn| RowKey::Task(turn.id)));
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
                    .filter(|item| item.kind == ItemKind::Assistant)
                    .map(|item| item.turn_id),
                _ => None,
            }?;
            seen.insert(turn).then_some(turn)
        })
        .collect()
}

/// The mail-triggered turn a row opens, whose emails it offers to view.
pub(super) fn opened_mail(
    c: &Conversation,
    opens: Option<Uuid>,
) -> Option<&else_wire::turns::Turn> {
    c.turn(opens?).filter(|turn| !turn.mail.is_empty())
}

#[cfg(test)]
pub(super) fn request_rows(c: &Conversation) -> Vec<(RowKey, Option<Uuid>)> {
    turn_card_rows(c)
        .into_iter()
        .map(|(_, key, anchor)| (RowKey::Card(key), anchor))
        .collect()
}

/// Each card row with its turn and the item it follows.
pub(super) fn turn_card_rows(c: &Conversation) -> Vec<(Uuid, CardKey, Option<Uuid>)> {
    c.turns_in_order()
        .into_iter()
        .flat_map(|turn| {
            turn.secret_requests
                .iter()
                .filter_map(move |request| {
                    let anchor = c.question_anchor(turn.id, request.settled_at);
                    (request.is_pending() || anchor.is_some())
                        .then_some((CardKey::SecretRequest(request.id), anchor))
                })
                .chain(turn.skill_installs.iter().filter_map(move |install| {
                    let anchor = c.question_anchor(turn.id, install.settled_at);
                    (install.settled_at.is_none() || anchor.is_some())
                        .then_some((CardKey::SkillInstall(install.id), anchor))
                }))
                .chain(turn.questions.iter().filter_map(move |question| {
                    let anchor = c.question_anchor(turn.id, question.settled_at);
                    (question.is_pending() || anchor.is_some())
                        .then_some((CardKey::Question(question.id), anchor))
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
                            shown.then_some((CardKey::Handoff(d.id), anchor))
                        }),
                )
                // A pending handoff of a turn outside the loaded page shows
                // only in the composer bar.
                .chain(turn.computer_handoffs.iter().filter_map(move |handoff| {
                    let anchor = c.question_anchor(turn.id, None)?;
                    Some((CardKey::ComputerHandoff(handoff.id), Some(anchor)))
                }))
                .chain(
                    turn.owner_notices
                        .iter()
                        .filter(|notice| notice.status != OwnerNoticeStatus::Superseded)
                        // A settled notice of a turn outside the loaded page
                        // would otherwise land below newer messages.
                        .filter_map(move |notice| {
                            let anchor = c.question_anchor(turn.id, None);
                            (notice.status == OwnerNoticeStatus::Pending || anchor.is_some())
                                .then_some((CardKey::OwnerNotice(notice.id), anchor))
                        }),
                )
                .chain(turn.social_setups.iter().map(move |setup| {
                    (
                        CardKey::SocialSetup(setup.id),
                        c.question_anchor(turn.id, None),
                    )
                }))
                .chain(
                    turn.outgoing_drafts
                        .iter()
                        .filter(|d| d.status != DraftStatus::Superseded)
                        .filter_map(move |draft| {
                            let anchor = c.question_anchor(turn.id, draft.settled_at);
                            (draft.settled_at.is_none() || anchor.is_some())
                                .then_some((CardKey::Draft(draft.id), anchor))
                        }),
                )
                .map(move |(key, anchor)| (turn.id, key, anchor))
        })
        .collect()
}

/// What a row draws from the conversation beyond its key; a row whose
/// state changes measures again.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum RowState {
    /// A message, by its turn's status.
    Item(TurnStatus),
    /// A task also changes when a card inside it does.
    Task(TurnStatus, Vec<CardState>),
    Card(CardState),
    Gap(GapSize),
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum CardState {
    Question(QuestionStatus),
    Draft {
        version: i64,
        updated_at: DateTime<Utc>,
        status: DraftStatus,
        can_approve: bool,
        in_reply: bool,
    },
    SkillInstall(SkillInstallStatus),
    SecretRequest(SecretRequestStatus),
    SocialSetup(SetupStatus),
    OwnerNotice(Box<OwnerNotice>),
    ComputerHandoff(HandoffStatus),
    /// Each task of the handoff's attempt.
    Handoff(Vec<DelegationStatus>),
}

impl CardState {
    fn of(c: &Conversation, key: CardKey) -> Option<Self> {
        Some(match key {
            CardKey::Question(id) => Self::Question(c.question(id)?.status),
            CardKey::Draft(id) => {
                let d = c.outgoing_draft(id)?;
                Self::Draft {
                    version: d.version,
                    updated_at: d.updated_at,
                    status: d.status,
                    can_approve: d.can_approve,
                    in_reply: d.in_reply,
                }
            }
            CardKey::SkillInstall(id) => Self::SkillInstall(c.skill_install(id)?.status),
            CardKey::SecretRequest(id) => Self::SecretRequest(c.secret_request(id)?.status),
            CardKey::SocialSetup(id) => Self::SocialSetup(c.social_setup(id)?.status),
            CardKey::OwnerNotice(id) => Self::OwnerNotice(Box::new(c.owner_notice(id)?.clone())),
            CardKey::ComputerHandoff(id) => Self::ComputerHandoff(c.computer_handoff(id)?.status),
            CardKey::Handoff(id) => {
                Self::Handoff(c.handoff(id)?.1.iter().map(|d| d.status).collect())
            }
        })
    }
}

impl RowState {
    fn of(c: &Conversation, key: RowKey, tasks: &HashMap<Uuid, TaskReply>) -> Option<Self> {
        match key {
            RowKey::Item(id) => Some(Self::Item(c.turn(c.item(id)?.turn_id)?.status)),
            RowKey::Task(id) => Some(Self::Task(
                c.turn(id)?.status,
                tasks
                    .get(&id)
                    .into_iter()
                    .flat_map(|task| &task.cards)
                    .filter_map(|card| CardState::of(c, card.key))
                    .collect(),
            )),
            RowKey::Card(card) => CardState::of(c, card).map(Self::Card),
            RowKey::Gap(above) => c.gap_below(above).map(Self::Gap),
            RowKey::Pending(_) | RowKey::QuietRun(_) | RowKey::QuietRuns(_) => None,
        }
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

/// What the owner opened or unfolded, which changes the rows.
pub(super) struct Folds<'a> {
    /// No-news runs ([`RowKey::QuietRun`]) and groups of them
    /// ([`RowKey::QuietRuns`]) the owner opened.
    pub(super) quiet: &'a HashSet<RowKey>,
    /// Exchange replies shown in full instead of their excerpt.
    pub(super) full_replies: &'a HashSet<Uuid>,
}

/// The rows a conversation shows, with the text each message row draws.
pub(super) struct Transcript {
    pub(super) rows: Vec<TranscriptRow>,
    pub(super) texts: Vec<(RowKey, RowSource)>,
    /// Each turn's task reply, including those of folded no-news runs.
    pub(super) tasks: HashMap<Uuid, TaskReply>,
    /// A shown task or a wait counts up each second.
    pub(super) ticking: bool,
}

/// Whether any of `tasks` or a wait of `c` counts up each second.
pub(super) fn ticking<'a>(
    mut tasks: impl Iterator<Item = &'a TaskReply>,
    c: Option<&Conversation>,
) -> bool {
    tasks.any(|task| task.state.ticking()) || c.is_some_and(|c| !c.waiting_on().is_empty())
}

/// Projects `conversation` and the owner's unsent messages into rows in
/// reading order, as seen from `zone`.
pub(super) fn build<Tz: TimeZone>(
    conversation: Option<&Conversation>,
    pending: &[PendingSend],
    folds: &Folds,
    unread_since: Option<ReplyPosition>,
    zone: &Tz,
) -> Transcript {
    let mut tasks = conversation.map(task_replies).unwrap_or_default();
    let mut texts = Vec::new();
    let mut requests = conversation
        .map(|c| place_cards(c, &mut tasks, turn_card_rows(c)))
        .unwrap_or_default();
    requests.extend(conversation.into_iter().flat_map(|c| {
        c.turns_in_order()
            .into_iter()
            .filter(|turn| c.reaction_in(turn.id).is_some() && turn.status.is_unsuccessful())
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
        .map(|c| fold_quiet(transcript_rows(c, &tasks, requests), c, folds.quiet))
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
            || (item.kind == ItemKind::Assistant && delegated.contains(&item.turn_id));
        let (text, parts) = if exchange_reply && !folds.full_replies.contains(&item.id) {
            super::bot_activity::excerpt_message(&text, &parts).unwrap_or((text, parts))
        } else {
            (text, parts)
        };
        texts.push((
            RowKey::Item(item.id),
            RowSource {
                text,
                parts,
                format,
            },
        ));
    }
    for pending in pending {
        rows.push(RowKey::Pending(pending.request_id));
        texts.push((
            RowKey::Pending(pending.request_id),
            RowSource {
                text: pending.text.clone(),
                parts: pending.parts.clone(),
                format: RowFormat::for_owner(&pending.text, &pending.parts),
            },
        ));
    }
    let senders: Vec<_> =
        rows.iter()
            .map(|key| match *key {
                RowKey::Item(id) => {
                    conversation
                        .and_then(|c| c.item(id))
                        .map_or((None, None), |item| {
                            let author = Author::of(item.kind).filter(|_| {
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
                    .iter()
                    .find(|p| p.request_id == request)
                    .map_or((None, None), |p| {
                        let author =
                            (!p.is_rejected() && p.reaction.is_none()).then_some(Author::Owner);
                        (author, Some(p.sent_at))
                    }),
                RowKey::Card(_) | RowKey::Gap(_) => (None, None),
                // A folded run dates the day it ran, under no sender.
                RowKey::QuietRun(turn) | RowKey::QuietRuns(turn) => (
                    None,
                    conversation
                        .and_then(|c| c.turn(turn)?.routine.as_ref())
                        .map(|run| run.due_at),
                ),
            })
            .collect();
    let unread_start = conversation.and_then(|c| first_unread_row(&rows, c, unread_since?));
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
        .zip(burst_tail_stamps(&senders, zone))
        .zip(routines)
        .map(|((key, footer_stamp), routine)| {
            let routine_turn = routine.and_then(|r| conversation?.turn(r.turn));
            let routine_status = routine_turn.map(super::message_row::turn_label);
            let state = conversation.and_then(|c| RowState::of(c, key, &tasks));
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
    mark_days(&mut rows, &times, zone);
    let ticking = ticking(tasks.values(), conversation);
    Transcript {
        rows,
        texts,
        tasks,
        ticking,
    }
}

#[cfg(test)]
mod tests;
