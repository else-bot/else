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
    /// Replies before and after owner messages joined into the turn, each
    /// read as a bot message of its own at its place in the transcript.
    pub answers: Vec<Uuid>,
    pub kept: Vec<Uuid>,
    /// Files of `kept` replies that a later reply delivers again; the kept
    /// reply shows its text without them, so a file appears once.
    pub repeated_files: HashSet<Uuid>,
    /// Cards that follow an earlier item of the turn, such as a settled draft
    /// or question; [`TaskReply::earlier`] places each after the item it followed.
    pub cards: Vec<TaskCard>,
    pub reactions: Vec<(Uuid, crate::model::ReactionKind)>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub attempt: Option<i64>,
}

impl TaskReply {
    /// Whether the latest reply reads as a reply rather than folding into
    /// the updates: it does unless an attempt is still generating or about
    /// to, and it was not kept for a reaction or a file.
    pub(in crate::app) fn shows_latest(&self) -> bool {
        self.latest_kept || !matches!(self.state, ReplyState::Queued | ReplyState::Working)
    }

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
    /// replies and cards the latest reply has not superseded, and the rest
    /// with the folded `history` once expanded. The current reply must read
    /// last, because an earlier one can describe a state the turn has since
    /// left, like a draft the owner then discarded.
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
            .copied()
            .filter(|&id| expanded || !self.reply_superseded(conversation, id))
            .chain(folded.iter().copied())
            .map(|id| ((seq(id), 0), Earlier::Reply(id)))
            .chain(
                self.cards
                    .iter()
                    .filter(|card| expanded || !self.card_superseded(conversation, card))
                    .map(|card| ((seq(card.after), 1), Earlier::Card(card.key))),
            )
            .collect();
        entries.sort_by_key(|(at, _)| *at);
        entries.into_iter().map(|(_, entry)| entry).collect()
    }

    /// Kept replies and cards that fold with `history` while collapsed.
    pub(in crate::app) fn superseded_count(&self, conversation: &Conversation) -> usize {
        let replies = self
            .kept
            .iter()
            .filter(|&&id| self.reply_superseded(conversation, id))
            .count();
        let cards = self
            .cards
            .iter()
            .filter(|card| self.card_superseded(conversation, card))
            .count();
        replies + cards
    }

    /// The attempt of the newest reply the card shows: a resumed reply
    /// carries the task's whole current answer, so what came before it
    /// folds, as T3 Code folds its work log under the final message. While a
    /// working attempt's progress reply is itself folded, the newest kept
    /// reply holds that place, so a fold does not reopen mid-task.
    fn superseding_attempt(&self, conversation: &Conversation) -> Option<i64> {
        let attempt = |id| conversation.item(id).and_then(|item| item.attempt);
        let shown =
            self.latest_kept || !matches!(self.state, ReplyState::Queued | ReplyState::Working);
        if shown {
            self.latest.and_then(attempt)
        } else {
            self.kept.iter().filter_map(|&id| attempt(id)).max()
        }
    }

    /// A kept reply from an earlier attempt folds, unless it delivers a file
    /// no later reply delivers, because no later text can carry the file.
    pub(in crate::app) fn reply_superseded(&self, conversation: &Conversation, id: Uuid) -> bool {
        let Some(latest) = self.superseding_attempt(conversation) else {
            return false;
        };
        let Some(item) = conversation.item(id) else {
            return false;
        };
        let delivers = item.parts.iter().any(|part| {
            matches!(part, crate::attachments::MessagePart::File { file }
                if !self.repeated_files.contains(&file.id))
        });
        item.attempt.is_some_and(|earlier| earlier < latest) && !delivers
    }

    /// A card from an earlier attempt folds with the replies around it.
    fn card_superseded(&self, conversation: &Conversation, card: &TaskCard) -> bool {
        self.superseding_attempt(conversation)
            .is_some_and(|latest| card.attempt < latest)
    }

    /// The fold's label: it counts only the `hidden` entries, so beside
    /// entries the card still shows it says "more".
    pub(in crate::app) fn fold_label(&self, conversation: &Conversation, hidden: usize) -> String {
        let more = self.kept.len() + self.cards.len() > self.superseded_count(conversation);
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
        .filter(|turn| !turn.taken_by_reply())
        .filter_map(|turn| {
            let state = match turn.status.as_str() {
                "queued" => ReplyState::Queued,
                "preparing" | "sending" | "running" => ReplyState::Working,
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
                    answers: Vec::new(),
                    kept: Vec::new(),
                    repeated_files: HashSet::new(),
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
    let mut joined: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for item in conversation.items().filter(|item| item.kind == "user") {
        if let Some(into) = conversation
            .turn(item.turn_id)
            .and_then(|turn| turn.steered_into)
        {
            joined.entry(into).or_default().push(item.id);
        }
    }
    for reply in replies.values_mut() {
        if let Some(joined) = joined.get(&reply.turn) {
            split_answers(conversation, &reactions, reply, joined);
        }
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
        let finished = reply.state.terminal();
        let notice_only = finished && reply.latest.is_none() && notice_turns.contains(turn);
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
            .and_then(|item| else_plain_text::lines(&item.text).into_iter().next())
            .map(|line| else_plain_text::cut(&line, 180));
    }
    replies
}

/// An owner action card that reads inside its task, after the item it followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) struct TaskCard {
    pub after: Uuid,
    pub key: RowKey,
    /// The attempt that raised it: a delegation's own, or else the attempt
    /// of the reply it follows (attempt 1 before any reply).
    pub attempt: i64,
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
/// follows the turn's last item, so it keeps its own row below the task, and
/// so does a card that follows one of the task's `answers`, which reads in
/// the transcript. Returns the rows left in the transcript.
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
            if turn_of(after) != Some(turn) || !before_row || task.answers.contains(&after) {
                return Some((key, anchor));
            }
            let attempt = match key {
                RowKey::Handoff(id) => conversation
                    .turn(turn)
                    .and_then(|t| t.delegations.iter().find(|d| d.id == id))
                    .map(|d| d.attempt),
                _ => None,
            }
            .or_else(|| conversation.item(after).and_then(|item| item.attempt))
            .unwrap_or(1);
            task.cards.push(TaskCard {
                after,
                key,
                attempt,
            });
            if let Some(at) = task.history.iter().position(|&id| id == after) {
                task.kept.push(task.history.remove(at));
                task.kept.sort_by_key(|&id| seq(id));
            }
            None
        })
        .collect()
}

/// Moves the replies around owner messages joined into the turn out of the
/// fold into `answers`, so the chat reads message, answer, message, answer.
/// `joined` holds those messages' items, oldest first, pending or taken; each
/// one starts a segment of the turn's replies by seq. The last reply before
/// each message is what the owner saw when writing it. After a message, its
/// segment's first reply and the reply ending that reply's attempt answer
/// it: seq cannot tell where the provider took the message, so both stay in
/// view. The newest segment stays the task, anchored after its message until
/// the bot writes there, and every other reply folds into it. A finished turn
/// that wrote nothing after its last messages ends on the reply before them.
fn split_answers(
    conversation: &Conversation,
    reactions: &HashMap<Uuid, crate::model::ReactionKind>,
    reply: &mut TaskReply,
    joined: &[Uuid],
) {
    let seq = |id| conversation.item(id).map_or(i64::MAX, |item| item.seq);
    let attempt = |id| conversation.item(id).and_then(|item| item.attempt);
    let mut boundaries: Vec<_> = joined.to_vec();
    let mut segments = vec![Vec::new(); boundaries.len() + 1];
    for id in reply.history.iter().copied().chain(reply.latest) {
        let index = boundaries.partition_point(|&message| seq(message) < seq(id));
        segments[index].push(id);
    }
    while reply.state.terminal() && segments.len() > 1 && segments.last().is_some_and(Vec::is_empty)
    {
        segments.pop();
        boundaries.pop();
    }
    let Some((current, earlier)) = segments.split_last() else {
        return;
    };
    let mut answers: Vec<Uuid> = earlier
        .iter()
        .filter_map(|segment| segment.last().copied())
        .collect();
    for segment in segments.iter().skip(1) {
        let Some(&first) = segment.first() else {
            continue;
        };
        let took = segment
            .iter()
            .copied()
            .rfind(|&id| attempt(id) == attempt(first));
        answers.push(first);
        answers.extend(took);
    }
    answers.retain(|&id| current.last() != Some(&id));
    answers.sort_by_key(|&id| seq(id));
    answers.dedup();
    if answers.is_empty() {
        return;
    }
    reply.latest = current.last().copied();
    reply.anchor = reply.latest.or_else(|| boundaries.last().copied());
    reply.latest_kept = reply.latest.is_some_and(|id| {
        reactions.contains_key(&id)
            || conversation.item(id).is_some_and(|item| {
                item.parts
                    .iter()
                    .any(|part| matches!(part, crate::attachments::MessagePart::File { .. }))
            })
    });
    reply
        .history
        .retain(|id| !answers.contains(id) && reply.latest != Some(*id));
    reply.answers = answers;
}

/// Moves the earlier replies the owner still needs from `history` to `kept`:
/// a reacted reply, the reply that ended each earlier attempt, and a reply
/// with a file no later reply delivers again. A resumed attempt reports only
/// what changed, so every attempt's ending reply stays in view. A bot that
/// re-uploads a file after a park, sometimes renaming an image, delivers the
/// same file again: an image of the same type and byte size counts as the same
/// file, and the kept reply shows its text without it. An answer to a joined
/// owner message always shows, so it counts among the replies around it the
/// same way.
fn keep_earlier_replies(
    conversation: &Conversation,
    reactions: &HashMap<Uuid, crate::model::ReactionKind>,
    reply: &mut TaskReply,
) {
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
    let seq = |id| conversation.item(id).map_or(i64::MAX, |item| item.seq);
    let mut replies: Vec<_> = reply
        .history
        .iter()
        .chain(&reply.answers)
        .copied()
        .chain(reply.latest)
        .collect();
    replies.sort_by_key(|&id| seq(id));
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
    let has_text = |id| {
        conversation
            .item(id)
            .is_some_and(|item| crate::attachments::has_prose(&item.text, &item.parts))
    };
    let mut delivered_later = reply.latest.map(files).unwrap_or_default();
    let mut kept = Vec::new();
    let mut repeated = HashSet::new();
    for &id in replies.iter().rev().filter(|&&id| reply.latest != Some(id)) {
        let files = files(id);
        let (again, new): (Vec<&&crate::attachments::Attachment>, Vec<_>) = files
            .iter()
            .partition(|file| delivered_later.iter().any(|later| same(file, later)));
        let answer = reply.answers.contains(&id);
        let keep = !answer
            && (!new.is_empty()
                || ((reactions.contains_key(&id) || ended.contains(&id)) && has_text(id)));
        if keep {
            kept.push(id);
        }
        if keep || answer {
            repeated.extend(again.iter().map(|file| file.id));
        }
        delivered_later.extend(files);
    }
    reply.history.retain(|id| !kept.contains(id));
    kept.reverse();
    reply.kept = kept;
    reply.repeated_files = repeated;
}

#[cfg(test)]
mod tests;
