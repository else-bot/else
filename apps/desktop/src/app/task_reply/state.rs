use crate::app::RowKey;
use crate::model::Conversation;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum ReplyState {
    Queued,
    Working,
    Waiting,
    Stopping,
    Complete,
    Stopped,
    Failed,
    Uncertain,
}

impl ReplyState {
    pub(in crate::app) fn label(self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Working => "Working",
            Self::Waiting => "Waiting",
            Self::Stopping => "Stopping",
            Self::Complete => "Complete",
            Self::Stopped => "Stopped",
            Self::Failed => "Failed",
            Self::Uncertain => "Unknown result",
        }
    }

    pub(in crate::app) fn ticking(self) -> bool {
        matches!(self, Self::Working | Self::Waiting | Self::Stopping)
    }

    pub(in crate::app) fn terminal(self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Stopped | Self::Failed | Self::Uncertain
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct TaskReply {
    pub turn: Uuid,
    pub state: ReplyState,
    pub anchor: Option<Uuid>,
    pub latest: Option<Uuid>,
    pub latest_kept: bool,
    pub summary: Option<String>,
    pub history: Vec<Uuid>,
    pub kept: Vec<Uuid>,
    /// Cards that follow an earlier item of the turn, such as a settled draft
    /// or question; [`TaskReply::earlier`] places each after the item it followed.
    pub cards: Vec<TaskCard>,
    pub reactions: Vec<(Uuid, crate::model::ReactionKind)>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub attempt: Option<i64>,
}

impl TaskReply {
    pub(in crate::app) fn elapsed(&self, now: DateTime<Utc>) -> Option<String> {
        // `started_at` restarts with each attempt, so a resumed task's
        // finished span would cover only its last attempt.
        if self.state.terminal() && self.attempt.is_some_and(|attempt| attempt > 1) {
            return None;
        }
        let start = self.started_at?;
        let end = if self.state.terminal() {
            self.finished_at?
        } else {
            now
        };
        Some(format_elapsed((end - start).num_seconds().max(0)))
    }

    /// What the card shows above its current reply, oldest first: kept
    /// replies and cards always, the folded `history` once expanded. The
    /// current reply must read last, because an earlier one can describe a
    /// state the turn has since left, like a draft the owner then discarded.
    pub(in crate::app) fn earlier(
        &self,
        conversation: &Conversation,
        history: &[Uuid],
        expanded: bool,
    ) -> Vec<Earlier> {
        let seq = |id| conversation.item(id).map_or(i64::MAX, |item| item.seq);
        let folded = if expanded { history } else { &[] };
        let mut entries: Vec<_> = self
            .kept
            .iter()
            .chain(folded)
            .map(|&id| ((seq(id), 0), Earlier::Reply(id)))
            .chain(
                self.cards
                    .iter()
                    .map(|card| ((seq(card.after), 1), Earlier::Card(card.key))),
            )
            .collect();
        entries.sort_by_key(|(at, _)| *at);
        entries.into_iter().map(|(_, entry)| entry).collect()
    }

    /// The fold's label: it counts only the `hidden` replies, so beside
    /// entries the card already shows it says "more".
    pub(in crate::app) fn fold_label(&self, hidden: usize) -> String {
        let more = !self.kept.is_empty() || !self.cards.is_empty();
        format!(
            "{hidden} {}{}",
            if more { "more " } else { "" },
            if hidden == 1 { "update" } else { "updates" }
        )
    }
}

pub(in crate::app) fn format_elapsed(seconds: i64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds / 60 % 60),
    }
}

pub(in crate::app) fn task_replies(conversation: &Conversation) -> HashMap<Uuid, TaskReply> {
    let reactions: HashMap<_, _> = conversation
        .items()
        .filter_map(|item| {
            item.reaction
                .map(|reaction| (reaction.item_id, reaction.kind))
        })
        .collect();
    let mut task_turns = HashSet::new();
    let mut notice_turns = HashSet::new();
    let delegated = conversation.delegated_turns();
    let mut replies: HashMap<_, _> = conversation
        .turns_in_order()
        .into_iter()
        .filter(|turn| conversation.reaction_in(turn.id).is_none() && !delegated.contains(&turn.id))
        .filter_map(|turn| {
            let state = match turn.status.as_str() {
                "queued" => ReplyState::Queued,
                "preparing" | "sending" | "running" => ReplyState::Working,
                "waiting" if turn.waits_on_job() => ReplyState::Working,
                "waiting" => ReplyState::Waiting,
                "cancelling" => ReplyState::Stopping,
                "completed" => ReplyState::Complete,
                "interrupted" => ReplyState::Stopped,
                "failed" => ReplyState::Failed,
                "uncertain" => ReplyState::Uncertain,
                _ => return None,
            };
            if turn.routine.is_some()
                || !turn.questions.is_empty()
                || !turn.secret_requests.is_empty()
                || !turn.skill_installs.is_empty()
                || !turn.outgoing_drafts.is_empty()
                || !turn.social_setups.is_empty()
            {
                task_turns.insert(turn.id);
            }
            Some((
                turn.id,
                TaskReply {
                    turn: turn.id,
                    state,
                    anchor: None,
                    latest: None,
                    latest_kept: false,
                    summary: None,
                    history: Vec::new(),
                    kept: Vec::new(),
                    cards: Vec::new(),
                    reactions: Vec::new(),
                    started_at: turn.started_at,
                    finished_at: turn.finished_at,
                    attempt: turn.attempt,
                },
            ))
        })
        .collect();
    let mut seen_turns = HashSet::new();
    let mut loaded = HashSet::new();
    for item in conversation.items() {
        loaded.insert(item.turn_id);
        let Some(reply) = replies.get_mut(&item.turn_id) else {
            continue;
        };
        if conversation.older_before_seq.is_some()
            && seen_turns.insert(item.turn_id)
            && item.kind != "user"
        {
            task_turns.insert(item.turn_id);
        }
        if item.bot_task.is_some() || item.bot_reply.is_some() {
            task_turns.insert(item.turn_id);
        }
        if item.kind == "assistant" {
            let has_file = item
                .parts
                .iter()
                .any(|part| matches!(part, crate::attachments::MessagePart::File { .. }));
            if has_file {
                task_turns.insert(item.turn_id);
            }
            let reaction = reactions.get(&item.id).copied();
            if let Some(kind) = reaction {
                reply.reactions.push((item.id, kind));
            }
            if let Some(previous) = reply.latest.replace(item.id) {
                reply.history.push(previous);
            }
            reply.latest_kept = reaction.is_some() || has_file;
            reply.anchor = Some(item.id);
        } else if item.bot_created.is_some() || item.rules_update.is_some() {
            notice_turns.insert(item.turn_id);
        } else if reply.latest.is_none() && item.routine_update.is_none() {
            reply.anchor = Some(item.id);
        }
    }
    for reply in replies.values_mut() {
        keep_earlier_replies(conversation, &reactions, reply);
    }
    // A queued turn runs no attempt, so its latest reply ended one.
    for (turn, reply) in &mut replies {
        let attempt = reply.latest.and_then(|id| conversation.item(id)?.attempt);
        reply.latest_kept |= attempt.is_some()
            && (reply.state == ReplyState::Queued
                || conversation
                    .turn(*turn)
                    .is_some_and(|turn| turn.attempt > attempt));
    }
    // A finished turn kept only for its pending computer handoff has no
    // history on this page; the composer bar shows it instead of a row.
    let handoff_only: HashSet<Uuid> = conversation
        .turns_in_order()
        .into_iter()
        .filter(|turn| !turn.computer_handoffs.is_empty() && !loaded.contains(&turn.id))
        .map(|turn| turn.id)
        .collect();
    replies.retain(|turn, reply| {
        let notice_only = reply.latest.is_none() && notice_turns.contains(turn);
        let finished = matches!(
            reply.state,
            ReplyState::Complete | ReplyState::Stopped | ReplyState::Failed | ReplyState::Uncertain
        );
        !notice_only
            && !(finished && handoff_only.contains(turn))
            && (reply.state != ReplyState::Complete
                || reply.latest.is_none()
                || !reply.history.is_empty()
                || !reply.kept.is_empty()
                || task_turns.contains(turn))
    });
    for reply in replies
        .values_mut()
        .filter(|reply| reply.state == ReplyState::Working)
    {
        reply.summary = reply
            .latest
            .and_then(|id| conversation.item(id))
            .and_then(|item| {
                let line: String = item
                    .text
                    .lines()
                    .find(|line| !line.trim().is_empty())?
                    .chars()
                    .take(180)
                    .collect();
                let plain = markdown::to_mdast(&line, &markdown::ParseOptions::default())
                    .ok()?
                    .to_string();
                (!plain.trim().is_empty()).then_some(plain)
            });
    }
    replies
}

/// An owner action card that reads inside its task, after the item it followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) struct TaskCard {
    pub after: Uuid,
    pub key: RowKey,
}

/// What a task card shows above its current reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum Earlier {
    Reply(Uuid),
    Card(RowKey),
}

/// Moves each card of a task's turn that follows an item before the task's
/// row into that task, so it reads where the owner acted instead of above
/// the bot's name, with the reply it followed kept in view. A pending card
/// follows the turn's last item, so it keeps its own row below the task.
/// Returns the rows left in the transcript.
pub(in crate::app) fn place_cards(
    conversation: &Conversation,
    tasks: &mut HashMap<Uuid, TaskReply>,
    cards: Vec<(Uuid, RowKey, Option<Uuid>)>,
) -> Vec<(RowKey, Option<Uuid>)> {
    let seq = |id| conversation.item(id).map(|item| item.seq);
    let turn_of = |id| conversation.item(id).map(|item| item.turn_id);
    cards
        .into_iter()
        .filter_map(|(turn, key, anchor)| {
            let Some((task, after)) = tasks.get_mut(&turn).zip(anchor) else {
                return Some((key, anchor));
            };
            let before_row = task
                .anchor
                .and_then(seq)
                .is_some_and(|row| seq(after) < Some(row));
            if turn_of(after) != Some(turn) || !before_row {
                return Some((key, anchor));
            }
            task.cards.push(TaskCard { after, key });
            if let Some(at) = task.history.iter().position(|&id| id == after) {
                task.kept.push(task.history.remove(at));
                task.kept.sort_by_key(|&id| seq(id));
            }
            None
        })
        .collect()
}

/// Moves the earlier replies the owner still needs from `history` to `kept`:
/// a reacted reply, the reply that ended each earlier attempt, and a reply
/// with a file no later reply delivers again. A resumed attempt reports only
/// what changed, so every attempt's ending reply stays in view. Turns from
/// before that restated their work after each park and re-uploaded the same
/// file, sometimes renaming an image: an image of the same type and byte size
/// counts as the same file, and only the most recent of those restated ending
/// replies stays.
fn keep_earlier_replies(
    conversation: &Conversation,
    reactions: &HashMap<Uuid, crate::model::ReactionKind>,
    reply: &mut TaskReply,
) {
    let Some(latest) = reply.latest else {
        return;
    };
    let attempt = |id| conversation.item(id).and_then(|item| item.attempt);
    let files = |id| {
        conversation
            .item(id)
            .into_iter()
            .flat_map(|item| &item.parts)
            .filter_map(|part| match part {
                crate::attachments::MessagePart::File { file } => Some(file),
                crate::attachments::MessagePart::Text { .. } => None,
            })
            .collect::<Vec<_>>()
    };
    let replies: Vec<_> = reply.history.iter().copied().chain([latest]).collect();
    let ended: Vec<Uuid> = replies
        .windows(2)
        .filter(|pair| attempt(pair[0]).is_some_and(|ended| attempt(pair[1]) > Some(ended)))
        .map(|pair| pair[0])
        .collect();
    let same = |a: &crate::attachments::Attachment, b: &crate::attachments::Attachment| {
        a.media_type == b.media_type
            && a.size == b.size
            && (a.name == b.name || a.media_type.starts_with("image/"))
    };
    let mut delivered_later = files(latest);
    let mut kept = Vec::new();
    for &id in reply.history.iter().rev() {
        let files = files(id);
        let new_file = files
            .iter()
            .any(|file| !delivered_later.iter().any(|later| same(file, later)));
        let ended_attempt = ended.contains(&id) && (files.is_empty() || ended.last() == Some(&id));
        delivered_later.extend(files);
        if new_file || reactions.contains_key(&id) || ended_attempt {
            kept.push(id);
        }
    }
    reply.history.retain(|id| !kept.contains(id));
    kept.reverse();
    reply.kept = kept;
}

#[cfg(test)]
mod tests;
