//! The client's conversation model: a snapshot plus contiguously applied
//! stream events. Any inconsistency (a gap, an unknown item, an offset
//! mismatch, an unknown kind) asks the caller to reload a snapshot rather than
//! guessing, so a bubble is never duplicated or corrupted.

#[cfg(test)]
mod skills;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use else_wire::bots::Activity;
use else_wire::computers::ComputerHandoff;
use else_wire::conversation::{BotExtras, BotUpdated, Event, EventKind, Snapshot, Window};
use else_wire::delegations::Delegation;
use else_wire::items::{Item, ItemKind, MessagePart, ReactionKind};
use else_wire::notices::{OwnerNotice, OwnerNoticeStatus};
use else_wire::outgoing::{DraftStatus, OutgoingDraft};
use else_wire::questions::{Question, QuestionStatus};
use else_wire::routines::Routine;
use else_wire::secrets::SecretRequest;
use else_wire::skills::{SkillInstall, SkillInstallStatus};
use else_wire::socials::SocialSetup;
use else_wire::turns::Turn;
use else_wire::waits::{ComputerJobWait, McpWait};
use serde::Deserialize;
use uuid::Uuid;

pub(crate) mod outgoing;

/// One event of a `bot:<id>` channel: its cursor and its event, decoded
/// once. An event this build does not know, or one whose data does not
/// parse, waits for `apply` to ask for a snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub id: Option<String>,
    pub event: Result<Event, Reset>,
}

impl Frame {
    /// Decode a channel message's `event` and `data`.
    pub fn decode(id: Option<String>, message: &serde_json::Value) -> Self {
        let event = Event::deserialize(message).map_err(|_| {
            match EventKind::deserialize(&message["event"]) {
                Ok(EventKind::Unknown) | Err(_) => {
                    Reset::UnknownKind(message["event"].as_str().unwrap_or_default().to_owned())
                }
                Ok(_) => Reset::Malformed,
            }
        });
        Self { id, event }
    }
}

pub const APPROVE: &str = "approve";
pub const DENY: &str = "deny";
pub const APPROVE_FOREVER: &str = "approve_forever";
pub const APPROVE_ROUTINE: &str = "approve_routine";
pub const APPROVE_TOOL: &str = "approve_tool";
pub const APPROVE_TASK: &str = "approve_task";

/// One thing a bot waits on, as the owner sees it above the composer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Wait<'a> {
    Mcp(&'a McpWait),
    Job(&'a ComputerJobWait),
    Task(&'a Delegation),
}

impl Wait<'_> {
    /// The ID the server stops it by.
    pub fn id(&self) -> Uuid {
        match self {
            Self::Mcp(wait) => wait.id,
            Self::Job(wait) => wait.id,
            Self::Task(task) => task.id,
        }
    }

    pub fn created_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Mcp(wait) => wait.created_at.as_deref().and_then(|at| at.parse().ok()),
            Self::Job(wait) => wait.created_at.as_deref().and_then(|at| at.parse().ok()),
            Self::Task(task) => Some(task.created_at),
        }
    }

    /// Whether the owner can still stop it: a task that ended only waits
    /// for its sender to read the outcome.
    pub fn stoppable(&self) -> bool {
        match self {
            Self::Mcp(_) | Self::Job(_) => true,
            Self::Task(task) => !task.status.settled(),
        }
    }
}

/// How many shown items an unloaded stretch holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapSize {
    Counted(i64),
    /// A live item landed inside a counted stretch and split it; loading
    /// either side counts it again.
    Uncounted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reset {
    Gap { expected: i64, got: i64 },
    BadCursor,
    UnknownItem,
    OffsetMismatch,
    UnknownKind(String),
    Malformed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Already applied (a replayed duplicate); nothing changed.
    Duplicate,
    Changed,
}

#[derive(Debug, Clone)]
pub struct Conversation {
    /// `<server_instance_id>:<bot_id>`; events must carry the same prefix.
    cursor_prefix: String,
    applied: i64,
    items: BTreeMap<i64, Item>,
    seq_of: HashMap<Uuid, i64>,
    turns: HashMap<Uuid, Turn>,
    pub older_before_seq: Option<i64>,
    /// Unloaded stretches between loaded items, keyed by the seq of the
    /// loaded item just above each.
    gaps: BTreeMap<i64, GapSize>,
    pub selected_provider_id: Option<String>,
    pub activity: Option<Activity>,
    extras: Arc<BotExtras>,
    /// Read from the page saved on disk, so possibly behind the server.
    saved: bool,
}

fn split_cursor(cursor: &str) -> Option<(&str, i64)> {
    let (prefix, seq) = cursor.rsplit_once(':')?;
    Some((prefix, seq.parse().ok()?))
}

impl Conversation {
    pub fn from_snapshot(snapshot: Snapshot) -> Result<Self, Reset> {
        let (prefix, applied) = split_cursor(&snapshot.event_cursor).ok_or(Reset::BadCursor)?;
        let mut conversation = Self {
            cursor_prefix: prefix.to_owned(),
            applied,
            items: BTreeMap::new(),
            seq_of: HashMap::new(),
            turns: HashMap::new(),
            older_before_seq: snapshot.older_before_seq,
            gaps: BTreeMap::new(),
            selected_provider_id: snapshot.selected_provider_id,
            activity: Some(snapshot.activity),
            extras: Arc::new(snapshot.extras),
            saved: false,
        };
        for item in snapshot.items {
            conversation.upsert_item(item);
        }
        for turn in snapshot.turns {
            conversation.turns.insert(turn.id, turn);
        }
        Ok(conversation)
    }

    /// A conversation from a page saved on disk, shown until the server's.
    pub fn from_saved(snapshot: Snapshot) -> Result<Self, Reset> {
        Ok(Self {
            saved: true,
            ..Self::from_snapshot(snapshot)?
        })
    }

    pub fn is_saved(&self) -> bool {
        self.saved
    }

    /// The cursor of the last applied event, for `Last-Event-ID`.
    pub fn cursor(&self) -> String {
        format!("{}:{}", self.cursor_prefix, self.applied)
    }

    fn upsert_item(&mut self, item: Item) {
        if let Some(old_seq) = self.seq_of.insert(item.id, item.seq)
            && old_seq != item.seq
        {
            self.items.remove(&old_seq);
        }
        self.items.insert(item.seq, item);
    }

    /// Apply the next stream event. Replays at or below the applied cursor
    /// are ignored; anything else out of order requires a fresh snapshot.
    pub fn apply(&mut self, frame: &Frame) -> Result<Applied, Reset> {
        let (prefix, seq) = frame
            .id
            .as_deref()
            .and_then(split_cursor)
            .ok_or(Reset::BadCursor)?;
        if prefix != self.cursor_prefix {
            return Err(Reset::BadCursor);
        }
        if seq <= self.applied {
            return Ok(Applied::Duplicate);
        }
        if seq != self.applied + 1 {
            return Err(Reset::Gap {
                expected: self.applied + 1,
                got: seq,
            });
        }
        match frame.event.clone()? {
            Event::ItemUpsert(upsert) => {
                let item = upsert.item;
                if self.seq_of.contains_key(&item.id) {
                    self.upsert_item(item);
                } else {
                    self.insert_unseen(item);
                }
            }
            Event::ItemDelta(delta) => {
                let seq_of_item = *self.seq_of.get(&delta.item_id).ok_or(Reset::UnknownItem)?;
                let item = self.items.get_mut(&seq_of_item).ok_or(Reset::UnknownItem)?;
                if item.text.len() as u64 != delta.offset_utf8 {
                    return Err(Reset::OffsetMismatch);
                }
                item.text.push_str(&delta.text);
            }
            Event::TurnUpdated(update) => {
                self.turns.insert(update.turn.id, update.turn);
            }
            Event::BotUpdated(update) => {
                let BotUpdated { bot, extras, .. } = *update;
                self.activity = bot.activity;
                if bot.selected_provider_id.is_some() {
                    self.selected_provider_id = bot.selected_provider_id;
                }
                Arc::make_mut(&mut self.extras).update(extras);
            }
        }
        self.applied = seq;
        Ok(Applied::Changed)
    }

    /// Merge an older page. It never replaces newer loaded text, the
    /// current routines, or the live cursor.
    pub fn merge_older(&mut self, page: Snapshot) {
        for item in page.items {
            if !self.seq_of.contains_key(&item.id) {
                self.upsert_item(item);
            }
        }
        for turn in page.turns {
            self.turns.entry(turn.id).or_insert(turn);
        }
        self.older_before_seq = page.older_before_seq;
    }

    /// Merge a window loaded strictly between the loaded items `below` and
    /// `above`, recording what is still missing on each side. Returns false
    /// and changes nothing when that region no longer matches what is
    /// loaded, because another load or a snapshot landed first.
    pub fn merge_window(&mut self, window: Window, below: Option<i64>, above: Option<i64>) -> bool {
        let still_loaded = |seq: Option<i64>| seq.is_none_or(|seq| self.items.contains_key(&seq));
        let low = below.map_or(i64::MIN, |seq| seq.saturating_add(1));
        let high = above.unwrap_or(i64::MAX);
        if !still_loaded(below)
            || !still_loaded(above)
            || self.items.range(low..high).next().is_some()
        {
            return false;
        }
        if let Some(above) = above {
            self.gaps.remove(&above);
        }
        let Some(first) = window.items.first().map(|item| item.seq) else {
            if below.is_none() {
                self.older_before_seq = None;
            }
            return true;
        };
        if let Some(above) = above.filter(|_| window.newer_gap > 0) {
            self.gaps.insert(above, GapSize::Counted(window.newer_gap));
        }
        match below {
            Some(_) if window.older_gap > 0 => {
                self.gaps.insert(first, GapSize::Counted(window.older_gap));
            }
            Some(_) => {}
            None => self.older_before_seq = (window.older_gap > 0).then_some(first),
        }
        for item in window.items {
            if !self.seq_of.contains_key(&item.id) {
                self.upsert_item(item);
            }
        }
        for turn in window.turns {
            self.turns.entry(turn.id).or_insert(turn);
        }
        true
    }

    /// Insert an item not loaded yet, keeping both sides of an unloaded
    /// stretch it lands in loadable.
    fn insert_unseen(&mut self, item: Item) {
        let seq = item.seq;
        self.upsert_item(item);
        let (below, Some(above)) = self.loaded_around(seq) else {
            return;
        };
        let unloaded = match below {
            Some(_) => self.gaps.remove(&above).is_some(),
            None => self.older_before_seq.is_some(),
        };
        if !unloaded {
            return;
        }
        if above > seq + 1 {
            self.gaps.insert(above, GapSize::Uncounted);
        }
        match below {
            Some(below) if below < seq - 1 => {
                self.gaps.insert(seq, GapSize::Uncounted);
            }
            Some(_) => {}
            None => self.older_before_seq = Some(seq),
        }
    }

    /// How many shown items are missing just below the loaded item `seq`.
    pub fn gap_below(&self, seq: i64) -> Option<GapSize> {
        self.gaps.get(&seq).copied()
    }

    /// The loaded items just below and just above `seq`.
    pub fn loaded_around(&self, seq: i64) -> (Option<i64>, Option<i64>) {
        (
            self.items.range(..seq).next_back().map(|(seq, _)| *seq),
            self.items.range(seq + 1..).next().map(|(seq, _)| *seq),
        )
    }

    /// Show the canonical owner item from a send response before its event
    /// arrives. Idempotent: the later upsert replaces it by ID.
    pub fn insert_item(&mut self, item: Item) {
        if !self.seq_of.contains_key(&item.id) {
            self.insert_unseen(item);
        }
    }

    /// Record a turn from a send response; a later event replaces it.
    pub fn insert_turn(&mut self, turn: Turn) {
        self.turns.entry(turn.id).or_insert(turn);
    }

    pub fn items(&self) -> impl DoubleEndedIterator<Item = &Item> {
        self.items.values()
    }

    /// The newest reply; a run with no news is silent, so never the latest.
    pub fn latest_reply(&self) -> Option<&Item> {
        self.items
            .values()
            .rev()
            .find(|item| item.kind == ItemKind::Assistant && !self.no_news(item.turn_id))
    }

    pub fn item(&self, id: Uuid) -> Option<&Item> {
        self.seq_of.get(&id).and_then(|seq| self.items.get(seq))
    }

    pub fn turn(&self, id: Uuid) -> Option<&Turn> {
        self.turns.get(&id)
    }

    /// Whether turn `id` is a routine run that completed with no news.
    pub fn no_news(&self, id: Uuid) -> bool {
        self.turn(id)
            .and_then(|turn| turn.routine.as_ref())
            .is_some_and(|run| run.no_news)
    }

    pub fn latest_unshown_turn(&self) -> Option<&Turn> {
        self.turns
            .values()
            .max_by_key(|turn| turn.turn_no)
            .filter(|turn| {
                (self.reaction_in(turn.id).is_some() && turn.status.is_live())
                    || !self.items().any(|item| item.turn_id == turn.id)
            })
    }

    pub fn turns_in_order(&self) -> Vec<&Turn> {
        let mut turns: Vec<_> = self.turns.values().collect();
        turns.sort_by_key(|turn| turn.turn_no);
        turns
    }

    /// Every secret request of the loaded turns, by turn number.
    pub fn secret_requests(&self) -> Vec<(&Turn, &SecretRequest)> {
        self.turns_in_order()
            .into_iter()
            .flat_map(|t| t.secret_requests.iter().map(move |r| (t, r)))
            .collect()
    }

    pub fn skill_install(&self, id: Uuid) -> Option<&SkillInstall> {
        self.turns
            .values()
            .flat_map(|t| &t.skill_installs)
            .find(|r| r.id == id)
    }

    pub fn apply_skill_install(&mut self, install: SkillInstall) {
        for turn in self.turns.values_mut() {
            if let Some(current) = turn.skill_installs.iter_mut().find(|r| r.id == install.id) {
                if current.status == SkillInstallStatus::Pending {
                    *current = install;
                }
                return;
            }
        }
    }

    pub fn owner_notice(&self, id: Uuid) -> Option<&OwnerNotice> {
        self.turns
            .values()
            .flat_map(|t| &t.owner_notices)
            .find(|n| n.id == id)
    }

    pub fn apply_owner_notice(&mut self, notice: OwnerNotice) {
        if let Some(turn) = self.turns.get_mut(&notice.turn_id)
            && let Some(current) = turn.owner_notices.iter_mut().find(|n| n.id == notice.id)
            && current.status == OwnerNoticeStatus::Pending
        {
            *current = notice;
        }
    }

    pub fn computer_handoff(&self, id: Uuid) -> Option<&ComputerHandoff> {
        self.turns
            .values()
            .flat_map(|t| &t.computer_handoffs)
            .find(|h| h.id == id)
    }

    /// The bot's open request to take over its computer (at most one):
    /// waiting on the owner, or taken and not yet handed back.
    pub fn open_handoff(&self) -> Option<&ComputerHandoff> {
        self.turns
            .values()
            .flat_map(|t| &t.computer_handoffs)
            .filter(|h| h.status.is_open())
            .max_by_key(|h| h.created_at)
    }

    pub fn secret_request(&self, id: Uuid) -> Option<&SecretRequest> {
        self.turns
            .values()
            .flat_map(|t| &t.secret_requests)
            .find(|r| r.id == id)
    }

    pub fn questions(&self) -> impl Iterator<Item = &Question> {
        self.turns.values().flat_map(|t| &t.questions)
    }

    pub fn question(&self, id: Uuid) -> Option<&Question> {
        self.turns
            .values()
            .flat_map(|t| &t.questions)
            .find(|q| q.id == id)
    }

    /// The tasks sent in the same provider attempt as delegation `id`.
    pub fn handoff(&self, id: Uuid) -> Option<(&Turn, Vec<&Delegation>)> {
        let turn = self
            .turns
            .values()
            .find(|t| t.delegations.iter().any(|d| d.id == id))?;
        let first = turn.delegations.iter().find(|d| d.id == id)?;
        let group = turn
            .delegations
            .iter()
            .filter(|d| d.shares_handoff(first))
            .collect();
        Some((turn, group))
    }

    /// What this conversation waits on, oldest first: the jobs of its
    /// unfinished turns, and each single task with no outcome yet or whose
    /// outcome no attempt has read. A council keeps its card in the
    /// transcript.
    pub fn waiting_on(&self) -> Vec<Wait<'_>> {
        let mut waits: Vec<Wait<'_>> = self
            .turns
            .values()
            .filter(|turn| turn.status.is_live())
            .flat_map(|turn| {
                let tasks = turn.delegations.iter().filter(|d| {
                    (!d.status.settled() || !d.seen)
                        && turn
                            .delegations
                            .iter()
                            .filter(|other| other.shares_handoff(d))
                            .count()
                            == 1
                });
                turn.mcp_waits
                    .iter()
                    .map(Wait::Mcp)
                    .chain(turn.computer_job_waits.iter().map(Wait::Job))
                    .chain(tasks.map(Wait::Task))
            })
            .collect();
        waits.sort_by_key(|wait| (wait.created_at(), wait.id()));
        waits
    }

    pub fn social_setup(&self, id: Uuid) -> Option<&SocialSetup> {
        self.turns
            .values()
            .flat_map(|t| &t.social_setups)
            .find(|s| s.id == id)
    }

    pub fn outgoing_draft(&self, id: Uuid) -> Option<&OutgoingDraft> {
        self.turns
            .values()
            .flat_map(|t| &t.outgoing_drafts)
            .find(|d| d.id == id)
    }

    /// The draft the owner's next message follows up, which goes with it.
    /// None when the newest such draft is on a channel this build does not
    /// know: the desktop cannot say what it would send back.
    pub fn open_draft(&self) -> Option<&OutgoingDraft> {
        self.turns_in_order()
            .into_iter()
            .flat_map(|t| &t.outgoing_drafts)
            .rfind(|d| d.status.can_follow_up())
            .filter(|d| d.content != else_wire::outgoing::DraftContent::Unknown)
    }

    /// The owner approves `draft`'s kind always.
    pub fn approves_always(&self, draft: &OutgoingDraft) -> bool {
        draft
            .kind
            .as_ref()
            .is_some_and(|kind| self.extras.draft_grants.contains(kind))
    }

    pub fn update_draft(&mut self, draft: OutgoingDraft) {
        for turn in self.turns.values_mut() {
            if let Some(current) = turn.outgoing_drafts.iter_mut().find(|d| d.id == draft.id) {
                if current.status == DraftStatus::Pending && current.updated_at <= draft.updated_at
                {
                    *current = draft;
                }
                return;
            }
        }
    }

    /// The owner message that skipped `question`. The server saves the
    /// message and skips pending questions in one transaction, so it is the
    /// owner message after the question's turn stamped closest to the moment
    /// the question settled.
    pub fn skipping_reply(&self, question: &Question) -> Option<&Item> {
        let settled = question
            .settled_at
            .filter(|_| question.status == QuestionStatus::Skipped)?;
        let turn = self
            .turns
            .values()
            .find(|t| t.questions.iter().any(|q| q.id == question.id))?;
        let after = self
            .items
            .values()
            .filter(|item| item.turn_id == turn.id)
            .map(|item| item.seq)
            .max()?;
        self.items
            .range(after + 1..)
            .map(|(_, item)| item)
            .filter(|item| item.kind == ItemKind::User)
            .filter_map(|item| Some((item, (item.created_at? - settled).abs())))
            .filter(|(_, gap)| *gap <= chrono::TimeDelta::seconds(2))
            .min_by_key(|(_, gap)| *gap)
            .map(|(item, _)| item)
    }

    /// The turn's last loaded item from before the card settled. A settled
    /// card with none (a mail event's source item never loads) follows the
    /// last earlier item of any turn. It has no anchor when that item may be
    /// on an unloaded older page.
    pub fn question_anchor(&self, turn: Uuid, settled_at: Option<DateTime<Utc>>) -> Option<Uuid> {
        if let Some(item) = self.items.values().rev().find(|item| {
            item.turn_id == turn
                && settled_at.is_none_or(|at| item.created_at.is_none_or(|created| created <= at))
        }) {
            return Some(item.id);
        }
        let at = settled_at?;
        self.items
            .values()
            .rev()
            .find(|item| item.created_at.is_some_and(|created| created <= at))
            .or_else(|| {
                self.older_before_seq
                    .is_none()
                    .then(|| self.items.values().next())
                    .flatten()
            })
            .map(|item| item.id)
    }

    /// Shared so a reader can keep them without copying.
    pub fn extras(&self) -> &Arc<BotExtras> {
        &self.extras
    }

    pub fn routines(&self) -> &[Routine] {
        self.extras.routines.as_deref().unwrap_or_default()
    }

    pub fn rules_version(&self) -> i64 {
        self.extras.rules_version.unwrap_or_default()
    }

    /// A routine from a change response. Only a newer version replaces the
    /// one the stream delivered, so a late response never undoes an event.
    pub fn apply_routine(&mut self, routine: Routine) {
        if let Some(current) = Arc::make_mut(&mut self.extras)
            .routines
            .iter_mut()
            .flatten()
            .find(|r| r.id == routine.id)
            && routine.updated_at > current.updated_at
        {
            *current = routine;
        }
    }

    /// The owner's latest reaction to `item_id`. Items are ordered by
    /// sequence, so the result does not depend on how pages were merged.
    pub fn reaction_for(&self, item_id: Uuid) -> Option<ReactionKind> {
        self.items
            .values()
            .rev()
            .find_map(|item| item.reaction.filter(|r| r.item_id == item_id))
            .map(|r| r.kind)
    }

    pub fn reaction_in(&self, turn_id: Uuid) -> Option<ReactionKind> {
        self.turn(turn_id)
            .and_then(|turn| turn.reaction.map(|reaction| reaction.kind))
            .or_else(|| {
                self.items()
                    .filter(|item| item.turn_id == turn_id)
                    .find_map(|item| item.reaction.map(|reaction| reaction.kind))
            })
    }

    /// Turns that ran a task another bot sent. The sender's chat shows their
    /// replies, so this chat keeps only the exchange row.
    pub fn delegated_turns(&self) -> std::collections::HashSet<Uuid> {
        self.items()
            .filter(|item| item.bot_task.is_some())
            .map(|item| item.turn_id)
            .collect()
    }

    /// This bot's replies to the task whose receipt is `task`: each one
    /// carrying a file, then the latest, in order.
    pub fn task_answers(&self, task: &Item) -> Vec<&Item> {
        if task.bot_task.is_none() {
            return Vec::new();
        }
        let replies: Vec<_> = self
            .items()
            .filter(|item| item.turn_id == task.turn_id && item.kind == ItemKind::Assistant)
            .collect();
        let last = replies.len().saturating_sub(1);
        replies
            .into_iter()
            .enumerate()
            .filter(|(ix, item)| {
                *ix == last
                    || item
                        .parts
                        .iter()
                        .any(|part| matches!(part, MessagePart::File { .. }))
            })
            .map(|(_, item)| item)
            .collect()
    }

    /// The receipt of the task another bot sent, for a turn that ran it.
    pub fn task_receipt(&self, turn: Uuid) -> Option<&Item> {
        self.items()
            .find(|item| item.turn_id == turn && item.bot_task.is_some())
    }

    pub fn is_feedback_acknowledgement(&self, item: &Item) -> bool {
        item.kind == ItemKind::Assistant
            && self.reaction_in(item.turn_id).is_some()
            && item
                .text
                .trim()
                .trim_end_matches('.')
                .eq_ignore_ascii_case("Thanks, noted")
            && !item
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::File { .. }))
    }

    pub fn has_request(&self, request_id: Uuid) -> bool {
        self.items
            .values()
            .any(|item| item.request_id == Some(request_id))
    }
}

#[cfg(test)]
mod tests {
    use super::outgoing::OutgoingDraftEdit as _;
    use super::*;
    use else_wire::bots::Bot;
    use else_wire::items::BotReplyStatus;
    use else_wire::routines::RoutineRun;
    use else_wire::routines::{RunStatus, WatcherStatus};
    use else_wire::rules::RulesUpdate;
    use else_wire::rules::{DiffOp, RuleChange, RulesFile};
    use else_wire::secrets::SecretRequestStatus;
    use else_wire::turns::TurnStatus;
    use serde_json::json;

    const PREFIX: &str =
        "11111111-1111-1111-1111-111111111111:22222222-2222-2222-2222-222222222222";

    fn bot() -> Uuid {
        "22222222-2222-2222-2222-222222222222".parse().unwrap()
    }

    fn item(id: u128, seq: i64, kind: &str, text: &str) -> serde_json::Value {
        json!({
            "id": Uuid::from_u128(id), "seq": seq, "turn_id": Uuid::from_u128(900 + id),
            "request_id": if kind == "user" { json!(Uuid::from_u128(500 + id)) } else { json!(null) },
            "kind": kind, "text": text, "provider_id": null, "created_at": "2026-11-02T00:00:00Z",
        })
    }

    fn snapshot_json(
        seq: i64,
        items: Vec<serde_json::Value>,
        older: Option<i64>,
    ) -> serde_json::Value {
        json!({
            "conversation_id": Uuid::new_v4(), "bot_id": bot(), "selected_provider_id": null,
            "event_cursor": format!("{PREFIX}:{seq}"), "items": items, "turns": [],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 },
            "older_before_seq": older,
        })
    }

    fn snapshot(seq: i64, items: Vec<serde_json::Value>, older: Option<i64>) -> Snapshot {
        serde_json::from_value(snapshot_json(seq, items, older)).unwrap()
    }

    fn frame(seq: i64, event: &str, data: serde_json::Value) -> Frame {
        Frame::decode(
            Some(format!("{PREFIX}:{seq}")),
            &json!({ "event": event, "data": data }),
        )
    }

    fn delta(seq: i64, id: u128, offset: usize, text: &str) -> Frame {
        frame(
            seq,
            "item.delta",
            json!({ "item_id": Uuid::from_u128(id), "offset_utf8": offset, "text": text }),
        )
    }

    fn window(items: Vec<serde_json::Value>, older_gap: i64, newer_gap: i64) -> Window {
        serde_json::from_value(json!({
            "items": items, "turns": [], "older_gap": older_gap, "newer_gap": newer_gap,
        }))
        .unwrap()
    }

    fn loaded(c: &Conversation) -> Vec<i64> {
        c.items().map(|item| item.seq).collect()
    }

    #[test]
    fn a_newer_servers_reaction_part_and_draft_channel_decode_without_a_reset() {
        let mut reacted = item(1, 1, "user", "hi [Poll]");
        reacted["reaction"] = json!({ "item_id": Uuid::from_u128(2), "kind": "heart" });
        reacted["parts"] = json!([
            { "type": "text", "text": "hi " },
            { "type": "poll", "question": "Lunch?" },
        ]);
        let upsert = frame(1, "item.upsert", json!({ "item": reacted }));
        let Ok(Event::ItemUpsert(upsert)) = upsert.event else {
            panic!("expected an item, got {:?}", upsert.event);
        };
        assert_eq!(upsert.item.reaction, None);
        assert_eq!(upsert.item.parts, [], "the item reads as its text");
        assert_eq!(upsert.item.text, "hi [Poll]");

        let turn = json!({
            "id": Uuid::nil(), "request_id": Uuid::nil(), "turn_no": 1, "provider_id": "codex",
            "status": "completed", "error": null, "effect_uncertain": false,
            "reaction": { "item_id": Uuid::from_u128(2), "kind": "heart" },
            "outgoing_drafts": [{
                "id": Uuid::nil(), "lineage_id": Uuid::nil(), "revision": 1, "bot_id": null,
                "turn_id": null, "content": { "channel": "fax", "number": "+1 555" },
                "version": 1, "status": "pending", "can_approve": true,
                "approved_version": null, "approved_item_id": null, "approved_at": null,
                "delivery": null, "result_detail": null, "superseded_by": null,
                "created_at": null, "updated_at": "2026-11-02T00:00:00Z", "settled_at": null,
            }],
        });
        let updated = frame(2, "turn.updated", json!({ "turn": turn }));
        let Ok(Event::TurnUpdated(updated)) = updated.event else {
            panic!("expected a turn, got {:?}", updated.event);
        };
        assert_eq!(updated.turn.reaction, None);
        assert_eq!(
            updated.turn.outgoing_drafts[0].content,
            else_wire::outgoing::DraftContent::Unknown
        );
    }

    #[test]
    fn a_window_below_the_tail_leaves_gaps_that_close_when_loaded() {
        let tail = vec![item(90, 90, "user", "latest")];
        let mut c = Conversation::from_snapshot(snapshot(5, tail, Some(90))).unwrap();
        let found = vec![item(40, 40, "user", "a"), item(41, 41, "assistant", "b")];
        assert!(c.merge_window(window(found, 12, 30), None, Some(90)));
        assert_eq!(loaded(&c), [40, 41, 90]);
        assert_eq!(c.older_before_seq, Some(40));
        assert_eq!(c.gap_below(90), Some(GapSize::Counted(30)));
        assert_eq!(c.cursor(), format!("{PREFIX}:5"), "the live cursor stays");

        let between = vec![item(60, 60, "user", "c"), item(70, 70, "user", "d")];
        assert!(c.merge_window(window(between, 3, 0), Some(41), Some(90)));
        assert_eq!(loaded(&c), [40, 41, 60, 70, 90]);
        assert_eq!(c.gap_below(90), None);
        assert_eq!(c.gap_below(60), Some(GapSize::Counted(3)));

        let rest = vec![item(50, 50, "user", "e")];
        assert!(c.merge_window(window(rest, 0, 0), Some(41), Some(60)));
        assert_eq!(c.gap_below(60), None);

        let oldest = vec![item(1, 1, "user", "first")];
        assert!(c.merge_window(window(oldest, 0, 0), None, Some(40)));
        assert_eq!(c.older_before_seq, None);
        assert_eq!(c.loaded_around(45), (Some(41), Some(50)));
    }

    #[test]
    fn an_empty_window_closes_the_gap_it_was_loaded_for() {
        let tail = vec![item(90, 90, "user", "latest")];
        let mut c = Conversation::from_snapshot(snapshot(5, tail, Some(90))).unwrap();
        let found = vec![item(40, 40, "user", "a")];
        assert!(c.merge_window(window(found, 5, 30), None, Some(90)));
        assert!(c.merge_window(window(vec![], 0, 0), Some(40), Some(90)));
        assert_eq!(c.gap_below(90), None);
        assert!(c.merge_window(window(vec![], 0, 0), None, Some(40)));
        assert_eq!(c.older_before_seq, None);
    }

    #[test]
    fn a_window_for_a_region_that_changed_is_dropped() {
        let tail = vec![item(90, 90, "user", "latest")];
        let mut c = Conversation::from_snapshot(snapshot(5, tail, Some(90))).unwrap();
        let found = vec![item(40, 40, "user", "a")];
        assert!(c.merge_window(window(found, 5, 30), None, Some(90)));
        let between = vec![item(60, 60, "user", "b")];
        assert!(c.merge_window(window(between, 10, 19), Some(40), Some(90)));

        let stale = vec![item(50, 50, "user", "c")];
        assert!(!c.merge_window(window(stale, 0, 0), Some(40), Some(90)));
        assert_eq!(loaded(&c), [40, 60, 90]);
        assert_eq!(c.gap_below(90), Some(GapSize::Counted(19)));
        assert_eq!(c.gap_below(60), Some(GapSize::Counted(10)));
        let gone = vec![item(85, 85, "user", "d")];
        assert!(!c.merge_window(window(gone, 0, 0), Some(80), Some(90)));
    }

    #[test]
    fn a_live_edit_inside_a_gap_keeps_both_sides_loadable() {
        let tail = vec![item(90, 90, "user", "latest")];
        let mut c = Conversation::from_snapshot(snapshot(5, tail, Some(90))).unwrap();
        let found = vec![item(20, 20, "user", "a")];
        assert!(c.merge_window(window(found, 0, 69), None, Some(90)));

        let edit = frame(
            6,
            "item.upsert",
            json!({ "item": item(50, 50, "user", "undone") }),
        );
        assert_eq!(c.apply(&edit), Ok(Applied::Changed));
        assert_eq!(loaded(&c), [20, 50, 90]);
        assert_eq!(c.gap_below(90), Some(GapSize::Uncounted));
        assert_eq!(c.gap_below(50), Some(GapSize::Uncounted));
        let read_before_the_edit = vec![item(50, 50, "user", "old")];
        assert!(!c.merge_window(window(read_before_the_edit, 0, 0), Some(20), Some(90)));
        assert_eq!(c.item(Uuid::from_u128(50)).unwrap().text, "undone");

        let upper = vec![item(51, 51, "user", "b"), item(89, 89, "user", "c")];
        assert!(c.merge_window(window(upper, 0, 0), Some(50), Some(90)));
        assert_eq!(c.gap_below(90), None);
        assert_eq!(c.gap_below(50), Some(GapSize::Uncounted));
        let lower = vec![item(21, 21, "user", "d"), item(49, 49, "user", "e")];
        assert!(c.merge_window(window(lower, 0, 0), Some(20), Some(50)));
        assert_eq!(c.gap_below(50), None);
        assert_eq!(loaded(&c), [20, 21, 49, 50, 51, 89, 90]);
    }

    #[test]
    fn a_live_item_next_to_a_loaded_row_leaves_no_empty_gap() {
        let tail = vec![item(90, 90, "user", "latest")];
        let mut c = Conversation::from_snapshot(snapshot(5, tail, Some(90))).unwrap();
        let found = vec![item(20, 20, "user", "a")];
        assert!(c.merge_window(window(found, 0, 69), None, Some(90)));

        let reply = item(21, 21, "assistant", "");
        assert_eq!(
            c.apply(&frame(6, "item.upsert", json!({ "item": reply }))),
            Ok(Applied::Changed)
        );
        assert_eq!(c.apply(&delta(7, 21, 0, "hi")), Ok(Applied::Changed));
        assert_eq!(c.gap_below(21), None);
        assert_eq!(c.gap_below(90), Some(GapSize::Uncounted));

        let edit = item(89, 89, "user", "top");
        assert_eq!(
            c.apply(&frame(8, "item.upsert", json!({ "item": edit }))),
            Ok(Applied::Changed)
        );
        assert_eq!(c.gap_below(90), None);
        assert_eq!(c.gap_below(89), Some(GapSize::Uncounted));
    }

    #[test]
    fn a_late_send_response_inside_a_gap_keeps_both_sides_loadable() {
        let tail = vec![item(90, 90, "user", "latest")];
        let mut c = Conversation::from_snapshot(snapshot(5, tail, Some(90))).unwrap();
        let found = vec![item(20, 20, "user", "a")];
        assert!(c.merge_window(window(found, 0, 69), None, Some(90)));
        let sent: Item = serde_json::from_value(item(50, 50, "user", "sent")).unwrap();
        c.insert_item(sent);
        assert_eq!(c.gap_below(90), Some(GapSize::Uncounted));
        assert_eq!(c.gap_below(50), Some(GapSize::Uncounted));
    }

    #[test]
    fn a_live_edit_below_the_oldest_row_keeps_older_rows_loadable() {
        let tail = vec![item(90, 90, "user", "latest")];
        let mut c = Conversation::from_snapshot(snapshot(5, tail, Some(90))).unwrap();
        let edit = frame(
            6,
            "item.upsert",
            json!({ "item": item(50, 50, "user", "undone") }),
        );
        assert_eq!(c.apply(&edit), Ok(Applied::Changed));
        assert_eq!(c.older_before_seq, Some(50));
        assert_eq!(c.gap_below(90), Some(GapSize::Uncounted));

        let found = vec![item(20, 20, "user", "a")];
        assert!(c.merge_window(window(found, 4, 29), None, Some(50)));
        assert_eq!(c.gap_below(90), Some(GapSize::Uncounted));
        assert_eq!(c.gap_below(50), Some(GapSize::Counted(29)));
        assert_eq!(c.older_before_seq, Some(20));
    }

    #[test]
    fn owner_notices_follow_turn_events_and_reload_from_snapshots() {
        let mut c = Conversation::from_snapshot(snapshot(0, vec![], None)).unwrap();
        let mut turn = json!({
            "id": Uuid::from_u128(900), "request_id": Uuid::from_u128(901), "turn_no": 1,
            "provider_id": "codex", "status": "completed", "effect_uncertain": false,
            "owner_notices": [{
                "id": Uuid::from_u128(400), "bot_id": bot(), "turn_id": Uuid::from_u128(900),
                "kind": "decision", "summary": "Choose a bot", "tried": null,
                "status": "pending", "created_at": "2026-11-01T00:00:00Z", "settled_at": null
            }]
        });
        for (seq, status) in [(1, "pending"), (2, "replied")] {
            turn["owner_notices"][0]["status"] = json!(status);
            if status == "replied" {
                turn["owner_notices"][0]["settled_at"] = json!("2026-11-01T00:01:00Z");
            }
            c.apply(&frame(seq, "turn.updated", json!({"turn": turn})))
                .unwrap();
            let mut snap = snapshot(seq, vec![], None);
            snap.turns = serde_json::from_value(json!([turn])).unwrap();
            let reloaded = Conversation::from_snapshot(snap).unwrap();
            assert_eq!(
                reloaded.owner_notice(Uuid::from_u128(400)),
                c.owner_notice(Uuid::from_u128(400))
            );
        }
        assert_eq!(
            c.owner_notice(Uuid::from_u128(400)).unwrap().status,
            OwnerNoticeStatus::Replied
        );
    }

    #[test]
    fn conversation_loads_with_new_states_in_turn_cards() {
        let turn = Uuid::from_u128(900);
        let mut data = snapshot_json(1, vec![], None);
        data["turns"] = json!([{
            "id": turn, "request_id": Uuid::from_u128(901), "turn_no": 1,
            "provider_id": "codex", "status": "waiting", "effect_uncertain": false,
            "outgoing_drafts": [{
                "id": Uuid::from_u128(700), "lineage_id": Uuid::from_u128(700), "revision": 1,
                "content": {"channel":"message", "platform":"matrix", "body":"Hi"},
                "version": 1, "status": "reviewing", "can_approve": false,
                "approval_source": "automation", "updated_at": "2026-11-01T00:00:00Z"
            }],
            "delegations": [{
                "id": Uuid::from_u128(600), "attempt": 1, "target_bot_id": bot(),
                "target_name": "Scout", "task": "Check it", "status": "paused",
                "created_at": "2026-11-01T00:00:00Z"
            }],
            "owner_notices": [{
                "id": Uuid::from_u128(400), "bot_id": bot(), "turn_id": turn,
                "kind": "question", "summary": "Choose a bot", "status": "acknowledged",
                "created_at": "2026-11-01T00:00:00Z"
            }],
            "computer_job_waits": [{
                "id": Uuid::from_u128(200), "job_id": Uuid::from_u128(201), "job_status": "paused"
            }]
        }]);
        let conversation =
            Conversation::from_snapshot(serde_json::from_value(data).unwrap()).unwrap();
        let shown = conversation.turn(turn).unwrap();
        assert_eq!(shown.outgoing_drafts[0].status, DraftStatus::Unknown);
        assert_eq!(shown.outgoing_drafts[0].content.label(), "matrix");
        assert_eq!(
            shown.delegations[0].status,
            else_wire::delegations::DelegationStatus::Unknown
        );
        assert_eq!(shown.owner_notices[0].status.as_str(), "acknowledged");
        assert_eq!(
            shown.computer_job_waits[0]
                .job_status
                .as_ref()
                .unwrap()
                .as_str(),
            "paused"
        );
    }

    #[test]
    fn malformed_wait_timestamps_reject_the_turn_update() {
        let mut conversation = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        let update = json!({"turn": {
            "id": Uuid::from_u128(900), "request_id": Uuid::from_u128(901), "turn_no": 1,
            "provider_id": "codex", "status": "waiting", "effect_uncertain": false,
            "mcp_waits": [{
                "id": Uuid::from_u128(200), "mcp_id": "t3-code", "deadline_at": "tomorrow"
            }]
        }});
        assert_eq!(
            conversation.apply(&frame(2, "turn.updated", update)),
            Err(Reset::Malformed)
        );
    }

    #[test]
    fn a_skipped_question_finds_the_owner_message_that_skipped_it() {
        let stamped = |id, seq, kind, text, at: &str| {
            let mut item = item(id, seq, kind, text);
            item["turn_id"] = json!(Uuid::from_u128(9));
            item["created_at"] = json!(at);
            item
        };
        let mut answer = item(3, 3, "user", "Markdown, for Notion");
        answer["created_at"] = json!("2026-11-02T00:00:01.500Z");
        let mut c = Conversation::from_snapshot(snapshot(
            3,
            vec![
                stamped(1, 1, "user", "make a digest", "2026-11-02T00:00:00.000Z"),
                stamped(
                    2,
                    2,
                    "assistant",
                    "Which format?",
                    "2026-11-02T00:00:00.500Z",
                ),
                answer,
            ],
            None,
        ))
        .unwrap();
        let question = Uuid::from_u128(40);
        let turn = |status: &str| {
            json!({ "turn": {
                "id": Uuid::from_u128(9), "request_id": Uuid::from_u128(8), "turn_no": 1,
                "provider_id": "synthetic", "status": "completed", "error": null,
                "effect_uncertain": false,
                "questions": [{ "id": question, "kind": "choice", "title": "Which format?",
                    "choices": [], "status": status,
                    "settled_at": "2026-11-02T00:00:01.510Z" }],
            }})
        };
        c.apply(&frame(4, "turn.updated", turn("skipped"))).unwrap();
        let reply = c.skipping_reply(c.question(question).unwrap());
        assert_eq!(reply.map(|i| i.text.as_str()), Some("Markdown, for Notion"));
        c.apply(&frame(5, "turn.updated", turn("cancelled")))
            .unwrap();
        assert!(c.skipping_reply(c.question(question).unwrap()).is_none());
    }

    #[test]
    fn states_a_newer_server_adds_still_load() {
        let mut page = snapshot(1, vec![item(1, 1, "summary", "Digest")], None);
        page.turns = vec![
            serde_json::from_value(json!({
                "id": Uuid::from_u128(901), "request_id": Uuid::from_u128(501), "turn_no": 1,
                "provider_id": "codex", "status": "rewinding", "error": null,
                "effect_uncertain": false,
                "computer_job_waits": [{ "id": Uuid::from_u128(902), "job_id": Uuid::from_u128(903) }]
            }))
            .unwrap(),
        ];
        let c = Conversation::from_snapshot(page).unwrap();
        assert_eq!(c.item(Uuid::from_u128(1)).unwrap().kind, ItemKind::Unknown);
        assert!(c.latest_reply().is_none(), "an unknown item is not a reply");
        assert_eq!(
            c.turn(Uuid::from_u128(901)).unwrap().status,
            TurnStatus::Unknown
        );
        assert!(c.waiting_on().is_empty(), "an unknown turn is not live");
    }

    #[test]
    fn a_state_that_is_not_a_string_is_malformed() {
        let mut c = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        let turn = json!({"id": Uuid::from_u128(901), "request_id": Uuid::from_u128(501), "turn_no": 1,
            "provider_id": "codex", "status": {"running": null}, "error": null, "effect_uncertain": false});
        assert!(matches!(
            c.apply(&frame(2, "turn.updated", json!({"turn": turn}))),
            Err(Reset::Malformed)
        ));
        assert!(c.turn(Uuid::from_u128(901)).is_none());
    }

    #[test]
    fn hidden_turn_status_is_available_until_its_first_visible_item() {
        let mut c = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        assert!(c.latest_unshown_turn().is_none());
        for (seq, status, expected) in [
            (2, "running", TurnStatus::Running),
            (3, "failed", TurnStatus::Failed),
            (4, "interrupted", TurnStatus::Interrupted),
        ] {
            let turn = json!({"id": Uuid::from_u128(901), "request_id": Uuid::from_u128(501), "turn_no": 1,
                "provider_id": "codex", "status": status, "error": null, "effect_uncertain": false});
            c.apply(&frame(seq, "turn.updated", json!({"turn":turn})))
                .unwrap();
            assert_eq!(c.latest_unshown_turn().unwrap().status, expected);
        }
        c.apply(&frame(
            5,
            "item.upsert",
            json!({"item":item(1, 1, "assistant", "What is this inbox for?")}),
        ))
        .unwrap();
        assert!(c.latest_unshown_turn().is_none());
    }

    #[test]
    fn feedback_acknowledgements_are_hidden_but_useful_replies_and_files_stay_visible() {
        let mut feedback = reaction(2, 2, 1, "thumbs_up");
        let mut reply = item(3, 3, "assistant", "  Thanks, noted.\n");
        reply["turn_id"] = feedback["turn_id"].clone();
        let normal = item(4, 4, "assistant", "Thanks, noted.");
        let c = Conversation::from_snapshot(snapshot(
            1,
            vec![
                item(1, 1, "assistant", "Done"),
                feedback.clone(),
                reply.clone(),
                normal,
            ],
            None,
        ))
        .unwrap();
        assert!(c.is_feedback_acknowledgement(c.item(Uuid::from_u128(3)).unwrap()));
        assert!(!c.is_feedback_acknowledgement(c.item(Uuid::from_u128(4)).unwrap()));
        assert_eq!(c.latest_reply().unwrap().id, Uuid::from_u128(4));
        for text in [
            "What went wrong?",
            "Thanks, noted. I updated the task.",
            "Thanks, noted. Should I update the task?",
        ] {
            let mut useful: Item = serde_json::from_value(reply.clone()).unwrap();
            useful.text = text.into();
            assert!(!c.is_feedback_acknowledgement(&useful));
        }
        let mut file: Item = serde_json::from_value(reply.clone()).unwrap();
        file.parts.push(else_wire::items::MessagePart::File {
            file: else_wire::items::Attachment {
                id: Uuid::new_v4(),
                name: "report.txt".into(),
                media_type: "text/plain".into(),
                size: 12,
            },
        });
        assert!(!c.is_feedback_acknowledgement(&file));
        feedback["reaction"]["kind"] = json!("thumbs_down");
        let c = Conversation::from_snapshot(snapshot(1, vec![feedback, reply], None)).unwrap();
        assert!(c.is_feedback_acknowledgement(c.latest_reply().unwrap()));
        assert_eq!(
            c.latest_reply().unwrap().seq,
            3,
            "hidden text still has a read position"
        );
    }

    #[test]
    fn feedback_keeps_its_working_status_available() {
        let feedback = reaction(2, 2, 1, "thumbs_up");
        let turn_id = feedback["turn_id"].clone();
        let mut page = snapshot(1, vec![feedback], None);
        for (status, expected) in [
            ("queued", TurnStatus::Queued),
            ("running", TurnStatus::Running),
            ("waiting", TurnStatus::Waiting),
            ("cancelling", TurnStatus::Cancelling),
        ] {
            page.turns = vec![
                serde_json::from_value(json!({
                    "id": turn_id, "request_id": Uuid::new_v4(), "turn_no": 2,
                    "provider_id": "synthetic", "status": status, "error": null,
                    "effect_uncertain": false
                }))
                .unwrap(),
            ];
            let c = Conversation::from_snapshot(page.clone()).unwrap();
            assert_eq!(c.latest_unshown_turn().unwrap().status, expected);
        }
    }

    #[test]
    fn child_replies_keep_their_attribution_across_replay_and_reload() {
        let own_reply = item(1, 1, "assistant", "I'll ask Marketing.");
        let mut child_reply = item(2, 2, "status", "**Conversions:** 120");
        child_reply["bot_reply"] = json!({
            "sender_bot_id": Uuid::from_u128(10), "sender_name": "Marketing",
            "prompt": "Report this week's KPIs.", "status": "completed",
        });
        let mut live =
            Conversation::from_snapshot(snapshot(1, vec![own_reply.clone()], None)).unwrap();
        let event = frame(2, "item.upsert", json!({ "item": child_reply.clone() }));
        assert_eq!(live.apply(&event), Ok(Applied::Changed));
        assert_eq!(live.apply(&event), Ok(Applied::Duplicate));
        let reloaded =
            Conversation::from_snapshot(snapshot(2, vec![own_reply, child_reply], None)).unwrap();
        let shown = live.item(Uuid::from_u128(2)).unwrap();
        assert_eq!(Some(shown), reloaded.item(shown.id));
        let reply = shown.bot_reply.as_ref().unwrap();
        assert_eq!(reply.sender_name, "Marketing");
        assert_eq!(reply.prompt, "Report this week's KPIs.");
        assert_eq!(reply.status, BotReplyStatus::Completed);
        assert_eq!(live.items().count(), 2);
        assert_eq!(live.latest_reply().unwrap().id, Uuid::from_u128(1));
    }

    #[test]
    fn routine_update_notices_survive_live_replays_reload_and_paging() {
        let mut notice = item(2, 2, "status", "Updated routine: Daily digest");
        notice["routine_update"] = json!({
            "routine_id": Uuid::from_u128(71), "name": "Daily digest"
        });
        let initial = snapshot(1, vec![notice.clone()], None);
        let reloaded = Conversation::from_snapshot(initial.clone()).unwrap();
        let mut live = Conversation::from_snapshot(snapshot(0, vec![], None)).unwrap();
        let event = frame(1, "item.upsert", json!({ "item": notice }));
        live.apply(&event).unwrap();
        assert_eq!(live.apply(&event), Ok(Applied::Duplicate));
        assert_eq!(live.items().count(), 1);
        let mut paged = Conversation::from_snapshot(snapshot(2, vec![], Some(3))).unwrap();
        paged.merge_older(initial);
        let expected = reloaded.items().next().unwrap();
        assert_eq!(
            expected.routine_update.as_ref().unwrap().name,
            "Daily digest"
        );
        assert_eq!(live.items().next(), Some(expected));
        assert_eq!(paged.items().next(), Some(expected));
    }

    #[test]
    fn routine_provenance_survives_events_reload_and_history_paging() {
        let id = Uuid::from_u128(901);
        let turn = json!({
            "id": id, "request_id": Uuid::from_u128(501), "turn_no": 1,
            "provider_id": "codex", "status": "waiting", "error": null,
            "effect_uncertain": false,
            "routine": { "routine_id": Uuid::from_u128(71), "name": "Merged branch cleanup",
                         "due_at": "2026-11-05T04:00:00Z" },
        });
        let mut initial = snapshot(
            1,
            vec![item(1, 1, "assistant", "Ready for approval.")],
            None,
        );
        initial.turns = vec![serde_json::from_value(turn.clone()).unwrap()];
        let mut live = Conversation::from_snapshot(snapshot(0, vec![], None)).unwrap();
        live.apply(&frame(1, "turn.updated", json!({ "turn": turn })))
            .unwrap();
        let reloaded = Conversation::from_snapshot(initial.clone()).unwrap();
        let mut paged = Conversation::from_snapshot(snapshot(2, vec![], Some(2))).unwrap();
        paged.merge_older(initial);
        let expected = reloaded.turn(id).unwrap().routine.as_ref().unwrap();
        assert_eq!(expected.name, "Merged branch cleanup");
        assert_eq!(live.turn(id).unwrap().routine.as_ref(), Some(expected));
        assert_eq!(paged.turn(id).unwrap().routine.as_ref(), Some(expected));
    }

    #[test]
    fn a_run_is_labelled_by_whether_a_reminder_started_it() {
        let run = |extra: serde_json::Value| {
            let mut run = json!({ "routine_id": Uuid::from_u128(71), "name": "DevOps check",
                                  "due_at": "2026-11-13T13:37:00Z" });
            run.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value::<RoutineRun>(run).unwrap().label()
        };
        assert_eq!(run(json!({ "reminder": true })), "Reminder");
        assert_eq!(run(json!({ "reminder": false })), "Scheduled routine");
        assert_eq!(run(json!({})), "Scheduled routine", "an older server");
    }

    #[test]
    fn utf8_deltas_duplicates_and_cursor() {
        let mut c =
            Conversation::from_snapshot(snapshot(3, vec![item(1, 1, "user", "hi")], None)).unwrap();
        assert_eq!(c.cursor(), format!("{PREFIX}:3"));
        c.apply(&frame(
            4,
            "item.upsert",
            json!({ "item": item(2, 2, "assistant", "é") }),
        ))
        .unwrap();
        assert_eq!(c.apply(&delta(5, 2, 2, "✓日")), Ok(Applied::Changed));
        assert_eq!(c.apply(&delta(5, 2, 2, "✓日")), Ok(Applied::Duplicate));
        assert_eq!(c.apply(&delta(4, 2, 0, "x")), Ok(Applied::Duplicate));
        assert_eq!(c.item(Uuid::from_u128(2)).unwrap().text, "é✓日");
        assert_eq!(c.cursor(), format!("{PREFIX}:5"));
        assert_eq!(c.items().count(), 2);
    }

    #[test]
    fn inconsistencies_require_a_snapshot() {
        let mut c =
            Conversation::from_snapshot(snapshot(1, vec![item(2, 1, "assistant", "ab")], None))
                .unwrap();
        assert_eq!(
            c.apply(&delta(3, 2, 2, "c")),
            Err(Reset::Gap {
                expected: 2,
                got: 3
            })
        );
        assert_eq!(c.apply(&delta(2, 2, 1, "c")), Err(Reset::OffsetMismatch));
        assert_eq!(c.apply(&delta(2, 9, 0, "c")), Err(Reset::UnknownItem));
        assert_eq!(
            c.apply(&frame(2, "mystery", json!({}))),
            Err(Reset::UnknownKind("mystery".into()))
        );
        assert_eq!(
            c.apply(&frame(2, "item.delta", json!({ "item_id": "x" }))),
            Err(Reset::Malformed)
        );
        let mut other = delta(2, 2, 2, "c");
        other.id = Some(
            "33333333-3333-3333-3333-333333333333:22222222-2222-2222-2222-222222222222:2".into(),
        );
        assert_eq!(c.apply(&other), Err(Reset::BadCursor));
        assert_eq!(c.item(Uuid::from_u128(2)).unwrap().text, "ab");
        assert_eq!(c.apply(&delta(2, 2, 2, "c")), Ok(Applied::Changed));
    }

    #[test]
    fn older_pages_merge_without_replacing_newer_text() {
        let mut c = Conversation::from_snapshot(snapshot(
            9,
            vec![item(3, 3, "user", "c"), item(4, 4, "assistant", "newest")],
            Some(3),
        ))
        .unwrap();
        c.merge_older(snapshot(
            5,
            vec![
                item(1, 1, "user", "a"),
                item(2, 2, "assistant", "b"),
                item(4, 4, "assistant", "stale"),
            ],
            None,
        ));
        let texts: Vec<_> = c.items().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "b", "c", "newest"]);
        assert_eq!(c.older_before_seq, None);
        assert_eq!(c.cursor(), format!("{PREFIX}:9"), "live cursor unchanged");
        assert!(c.has_request(Uuid::from_u128(501)));
    }

    fn reaction(id: u128, seq: i64, target: u128, kind: &str) -> serde_json::Value {
        let mut item = item(
            id,
            seq,
            "user",
            "Feedback on your earlier message:\n> Done.",
        );
        item["reaction"] = json!({ "item_id": Uuid::from_u128(target), "kind": kind });
        item
    }

    #[test]
    fn the_newest_committed_reaction_marks_its_exact_target() {
        let history = || {
            vec![
                item(1, 1, "user", "Do it twice"),
                item(2, 2, "assistant", "Done."),
                reaction(3, 3, 2, "thumbs_up"),
                item(4, 4, "assistant", "Done."),
                reaction(5, 5, 2, "thumbs_down"),
                item(6, 6, "assistant", "Noted."),
            ]
        };
        let whole = Conversation::from_snapshot(snapshot(6, history(), None)).unwrap();
        let all = history();
        let (older, newer) = all.split_at(3);
        let mut paged = Conversation::from_snapshot(snapshot(6, newer.to_vec(), Some(4))).unwrap();
        assert_eq!(
            paged.reaction_for(Uuid::from_u128(2)),
            Some(ReactionKind::ThumbsDown),
            "a reaction newer than the loaded target is already loaded"
        );
        paged.merge_older(snapshot(6, older.to_vec(), None));
        for c in [&whole, &paged] {
            assert_eq!(
                c.reaction_for(Uuid::from_u128(2)),
                Some(ReactionKind::ThumbsDown)
            );
            assert_eq!(
                c.reaction_for(Uuid::from_u128(4)),
                None,
                "same text, other item"
            );
            assert_eq!(c.reaction_for(Uuid::from_u128(6)), None);
        }

        paged
            .apply(&frame(
                7,
                "item.upsert",
                json!({ "item": reaction(7, 7, 2, "thumbs_up") }),
            ))
            .unwrap();
        assert_eq!(
            paged.reaction_for(Uuid::from_u128(2)),
            Some(ReactionKind::ThumbsUp)
        );
    }

    fn routine(enabled: bool, updated: &str, status: Option<&str>) -> serde_json::Value {
        json!({
            "id": Uuid::from_u128(40), "bot_id": bot(), "name": "Morning digest",
            "prompt": "Summarize my mail.", "cron": "0 9 * * *", "timezone": "Europe/Paris",
            "enabled": enabled, "next_run_at": if enabled { json!("2026-11-05T08:00:00Z") } else { json!(null) },
            "created_at": "2026-11-01T00:00:00Z", "updated_at": updated,
            "last_run": status.map(|s| json!({ "due_at": "2026-11-04T08:00:00Z", "status": s,
                "skip_reason": null, "turn_id": Uuid::from_u128(41),
                "error": if s == "failed" { json!({ "code": "provider_refused", "message": "Refused." }) } else { json!(null) },
                "finished_at": null })),
        })
    }

    fn bot_updated(seq: i64, routines: Option<Vec<serde_json::Value>>) -> Frame {
        let mut data = json!({ "bot_id": bot(), "bot": {
            "id": bot(), "name": "Assistant",
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 } } });
        if let Some(routines) = routines {
            data["routines"] = json!(routines);
        }
        frame(seq, "bot.updated", data)
    }

    #[test]
    fn routines_come_from_the_snapshot_and_each_bot_update() {
        let mut first = snapshot(1, vec![], Some(5));
        first.extras.routines = Some(vec![
            serde_json::from_value(routine(true, "2026-11-01T00:00:00Z", None)).unwrap(),
        ]);
        let mut c = Conversation::from_snapshot(first).unwrap();
        assert_eq!(c.routines().len(), 1);
        assert!(c.routines()[0].enabled);

        c.apply(&bot_updated(
            2,
            Some(vec![routine(true, "2026-11-01T00:00:00Z", Some("failed"))]),
        ))
        .unwrap();
        let last = c.routines()[0].last_run.as_ref().unwrap();
        assert_eq!(last.status, RunStatus::Failed);
        assert_eq!(last.error.as_ref().unwrap().code, "provider_refused");

        c.apply(&bot_updated(3, None)).unwrap();
        assert_eq!(c.routines().len(), 1);
        c.merge_older(snapshot(3, vec![], None));
        assert_eq!(c.routines().len(), 1);

        c.apply(&bot_updated(4, Some(vec![]))).unwrap();
        assert!(c.routines().is_empty(), "deleted");
        let malformed = frame(
            5,
            "bot.updated",
            json!({ "bot": { "id": bot(), "name": "A" }, "routines": [{ "id": 1 }] }),
        );
        assert_eq!(c.apply(&malformed), Err(Reset::Malformed));
    }

    #[test]
    fn the_rules_version_follows_the_snapshot_and_bot_updates() {
        let mut first = snapshot(1, vec![], None);
        first.extras.rules_version = Some(2);
        let mut c = Conversation::from_snapshot(first).unwrap();
        assert_eq!(c.rules_version(), 2);
        c.apply(&bot_updated(2, None)).unwrap();
        assert_eq!(c.rules_version(), 2, "an older server sends no version");
        let update = frame(
            3,
            "bot.updated",
            json!({ "bot_id": bot(), "rules_version": 3, "bot": {
                "id": bot(), "name": "Assistant",
                "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 } } }),
        );
        c.apply(&update).unwrap();
        assert_eq!(c.rules_version(), 3);
    }

    #[test]
    fn a_rules_card_and_the_rules_file_parse_and_mark_changed_rules() {
        let update: RulesUpdate = serde_json::from_value(json!({
            "version": 2, "section": "New Linear issue", "change": "changed",
            "owner_words": "Just send fix issue ELSE-142.",
            "diff": [{ "op": "remove", "text": "Do: describe it" },
                     { "op": "add", "text": "Do: send `fix issue {ID}`" }]
        }))
        .unwrap();
        assert_eq!(update.change, RuleChange::Changed);
        assert_eq!(update.diff[1].op, DiffOp::Add);
        let file: RulesFile = serde_json::from_value(json!({
            "version": 3, "markdown": "## New Linear issue\nDo: x\n\n## Tone\nShort.\n",
            "sections": [{ "title": "New Linear issue", "body": "Do: x" },
                         { "title": "Tone", "body": "Short." }],
            "history": [
                { "version": 3, "section": "New Linear issue", "change": "changed",
                  "owner_words": "x", "author": "bot", "restored_from": null, "diff": [],
                  "created_at": "2026-11-08T10:00:00Z" },
                { "version": 2, "section": "Tone", "change": "added", "owner_words": "y",
                  "author": "bot", "restored_from": null, "diff": [],
                  "created_at": "2026-11-07T10:00:00Z" }
            ]
        }))
        .unwrap();
        let changed = file.changed_since(2);
        assert!(changed.contains("new linear issue"));
        assert!(!changed.contains("tone"));
        assert!(file.changed_since(3).is_empty());
    }

    #[test]
    fn a_rules_card_loads_new_change_and_diff_kinds() {
        let update: RulesUpdate = serde_json::from_value(json!({
            "version": 2, "section": "Tone", "change": "renamed",
            "owner_words": "Rename this rule.",
            "diff": [{ "op": "replace", "text": "Keep it short." }]
        }))
        .unwrap();
        assert_eq!(update.change, RuleChange::Unknown);
        assert_eq!(update.diff[0].op, DiffOp::Unknown);
    }

    fn social_trigger(enabled: bool, provider: &str) -> serde_json::Value {
        json!({
            "id": Uuid::from_u128(80), "connection_id": Uuid::from_u128(81),
            "bot_id": bot(), "provider": provider, "connection_name": "Team bot",
            "username": "team_bot", "destination": "-100123", "destination_name": "Proliquid",
            "wake": "mentions", "instructions": "Summarize questions.", "enabled": enabled,
            "created_at": "2026-11-01T07:00:00.123+00:00", "updated_at": "2026-11-02T07:00:00.456+00:00"
        })
    }

    #[test]
    fn social_triggers_parse_from_snapshots_and_bot_updates() {
        let first: Snapshot = serde_json::from_value(json!({
            "event_cursor": format!("{PREFIX}:1"), "items": [], "turns": [],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 },
            "social_triggers": [social_trigger(true, "telegram")]
        }))
        .unwrap();
        let mut conversation = Conversation::from_snapshot(first).unwrap();
        assert_eq!(conversation.extras().social_triggers.len(), 1);
        let trigger = &conversation.extras().social_triggers[0];
        assert_eq!(trigger.id, Uuid::from_u128(80));
        assert_eq!(trigger.connection_id, Uuid::from_u128(81));
        assert_eq!(trigger.bot_id, bot());
        assert_eq!(trigger.title(), "Proliquid");
        assert_eq!(trigger.wake.label(), "on mention");
        assert_eq!(trigger.instructions, "Summarize questions.");
        assert!(trigger.enabled);
        let update = |seq, triggers| {
            frame(
                seq,
                "bot.updated",
                json!({
                    "bot": { "id": bot(), "name": "Assistant" }, "social_triggers": triggers
                }),
            )
        };
        conversation
            .apply(&update(2, json!([social_trigger(false, "discord")])))
            .unwrap();
        assert!(!conversation.extras().social_triggers[0].enabled);
        assert_eq!(
            conversation.extras().social_triggers[0].provider.label(),
            "Discord"
        );
        let mut older = snapshot(1, vec![], None);
        older.extras.social_triggers =
            vec![serde_json::from_value(social_trigger(true, "telegram")).unwrap()];
        conversation.merge_older(older);
        assert!(
            !conversation.extras().social_triggers[0].enabled,
            "older pages keep the current triggers"
        );
        conversation.apply(&update(3, json!([]))).unwrap();
        assert!(
            conversation.extras().social_triggers.is_empty(),
            "deleted triggers leave the list"
        );
        assert_eq!(
            conversation.apply(&update(4, json!([{ "id": 1 }]))),
            Err(Reset::Malformed)
        );
    }

    #[test]
    fn mail_triggers_parse_from_snapshots_and_bot_updates() {
        let inbox = |wake| {
            json!({
                "account_id": Uuid::from_u128(90), "address": "owner@example.com",
                "state": "active", "health": "connected", "wake": wake,
                "instructions": "Flag invoices.", "last_woke_at": "2026-11-02T08:00:00.000+00:00"
            })
        };
        let first: Snapshot = serde_json::from_value(json!({
            "event_cursor": format!("{PREFIX}:1"), "items": [], "turns": [],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 },
            "mail_triggers": [inbox(true)]
        }))
        .unwrap();
        let mut conversation = Conversation::from_snapshot(first).unwrap();
        let trigger = &conversation.extras().mail_triggers[0];
        assert_eq!(trigger.account_id, Uuid::from_u128(90));
        assert_eq!(trigger.address, "owner@example.com");
        assert!(trigger.wake);
        assert_eq!(trigger.instructions.as_deref(), Some("Flag invoices."));
        assert!(trigger.last_woke_at.is_some());
        let update = |seq, triggers| {
            frame(
                seq,
                "bot.updated",
                json!({ "bot": { "id": bot(), "name": "Assistant" }, "mail_triggers": triggers }),
            )
        };
        conversation
            .apply(&update(2, json!([inbox(false)])))
            .unwrap();
        assert!(!conversation.extras().mail_triggers[0].wake);
        conversation.apply(&update(3, json!([]))).unwrap();
        assert!(
            conversation.extras().mail_triggers.is_empty(),
            "removed inboxes leave the list"
        );
    }

    #[test]
    fn webhooks_parse_from_snapshots_and_bot_updates() {
        let webhook = |enabled| {
            json!({
                "id": Uuid::from_u128(91), "bot_id": bot(), "name": "Deploys",
                "prompt": "Open an issue when a deploy fails.", "min_interval_minutes": 15,
                "daily_limit": 24, "enabled": enabled, "requested_at": null,
                "wake_not_before": null, "created_at": "2026-11-01T00:00:00Z",
                "updated_at": "2026-11-01T00:00:00Z",
                "last_wake": { "woke_at": "2026-11-02T08:00:00Z", "status": "completed",
                               "turn_id": Uuid::from_u128(92) }
            })
        };
        let mut first = snapshot(1, vec![], None);
        first.extras.webhooks = vec![serde_json::from_value(webhook(true)).unwrap()];
        let mut conversation = Conversation::from_snapshot(first).unwrap();
        let listed = &conversation.extras().webhooks[0];
        assert_eq!(listed.name, "Deploys");
        assert_eq!(
            listed.last_wake.as_ref().unwrap().woke_at.to_rfc3339(),
            "2026-11-02T08:00:00+00:00"
        );
        let update = |seq, webhooks| {
            frame(
                seq,
                "bot.updated",
                json!({ "bot": { "id": bot(), "name": "Assistant" }, "webhooks": webhooks }),
            )
        };
        conversation
            .apply(&update(2, json!([webhook(false)])))
            .unwrap();
        assert!(!conversation.extras().webhooks[0].enabled);
        conversation.apply(&update(3, json!([]))).unwrap();
        assert!(conversation.extras().webhooks.is_empty());
    }

    #[test]
    fn watchers_default_to_empty_on_older_servers() {
        let conversation = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        assert!(conversation.extras().watchers.is_empty());
        let bot: Bot = serde_json::from_value(json!({ "id": bot(), "name": "Assistant" })).unwrap();
        assert_eq!(bot.watching, 0);
    }

    #[test]
    fn watchers_follow_the_snapshot_and_live_updates() {
        let mut page = snapshot_json(1, vec![], None);
        let mut watcher = json!({
            "id": "c7947c9e-2f91-4fb5-8fcc-ec4b20ca9307", "purpose": "Domain availability",
            "every_minutes": 15, "until": "2026-12-04T10:00:00.000+00:00", "status": "waiting",
            "last_status": null, "last_checked_at": null, "next_check_at": "2026-11-20T08:13:00Z",
            "last_output": null, "checks": 0, "failures": 0, "created_at": "2026-11-20T08:00:00Z",
            "created_turn_id": null,
        });
        page["watchers"] = json!([watcher]);
        let mut conversation =
            Conversation::from_snapshot(serde_json::from_value(page).unwrap()).unwrap();
        let [active] = conversation.extras().watchers.as_slice() else {
            panic!("expected one active watcher")
        };
        assert_eq!(active.purpose, "Domain availability");
        assert_eq!(active.until.to_rfc3339(), "2026-12-04T10:00:00+00:00");
        assert_eq!(active.status, WatcherStatus::Waiting);
        assert_eq!(
            active.next_check_at.unwrap().to_rfc3339(),
            "2026-11-20T08:13:00+00:00"
        );
        assert_eq!(active.last_checked_at, None);
        let update = |seq, count, watchers| {
            frame(
                seq,
                "bot.updated",
                json!({ "bot": { "id": bot(), "name": "Assistant", "watching": count }, "watchers": watchers }),
            )
        };
        watcher["status"] = json!("checking");
        watcher["last_status"] = json!("unavailable");
        watcher["last_output"] = json!("Domain is registered");
        conversation.apply(&update(2, 1, json!([watcher]))).unwrap();
        let active = &conversation.extras().watchers[0];
        assert_eq!(active.status, WatcherStatus::Checking);
        assert_eq!(active.last_status.as_deref(), Some("unavailable"));
        assert_eq!(active.last_output.as_deref(), Some("Domain is registered"));
        conversation.apply(&update(3, 0, json!([]))).unwrap();
        assert!(conversation.extras().watchers.is_empty());
    }

    #[test]
    fn a_new_watcher_status_does_not_prevent_loading_a_snapshot() {
        let mut page = snapshot_json(1, vec![], None);
        page["watchers"] = json!([{
            "id": "c7947c9e-2f91-4fb5-8fcc-ec4b20ca9307",
            "purpose": "Domain availability",
            "until": "2026-12-04T10:00:00Z",
            "status": "new_status",
        }]);
        let conversation =
            Conversation::from_snapshot(serde_json::from_value(page).unwrap()).unwrap();
        assert_eq!(
            conversation.extras().watchers[0].status,
            WatcherStatus::Unknown
        );
    }

    #[test]
    fn older_servers_without_social_triggers_use_an_empty_list() {
        let mut conversation = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        assert!(conversation.extras().social_triggers.is_empty());
        Arc::make_mut(&mut conversation.extras).social_triggers =
            vec![serde_json::from_value(social_trigger(true, "telegram")).unwrap()];
        conversation.apply(&bot_updated(2, None)).unwrap();
        assert!(conversation.extras().social_triggers.is_empty());
    }

    #[test]
    fn a_late_change_response_never_undoes_a_newer_event() {
        let mut first = snapshot(1, vec![], None);
        first.extras.routines = Some(vec![
            serde_json::from_value(routine(true, "2026-11-01T00:00:00Z", None)).unwrap(),
        ]);
        let mut c = Conversation::from_snapshot(first).unwrap();
        let paused: Routine =
            serde_json::from_value(routine(false, "2026-11-02T00:00:00Z", None)).unwrap();
        c.apply_routine(paused.clone());
        assert!(!c.routines()[0].enabled, "a newer response applies");
        c.apply(&bot_updated(
            2,
            Some(vec![routine(true, "2026-11-03T00:00:00Z", None)]),
        ))
        .unwrap();
        c.apply_routine(paused);
        assert!(c.routines()[0].enabled);
    }

    #[test]
    fn turn_updates_carry_secret_request_metadata() {
        let mut c = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        let request = Uuid::from_u128(77);
        let turn = |status: &str, request_status: &str| {
            json!({ "turn": {
                "id": Uuid::from_u128(9), "request_id": Uuid::from_u128(8), "turn_no": 1,
                "provider_id": "synthetic", "status": status, "error": null,
                "effect_uncertain": false,
                "secret_requests": [{ "id": request, "name": "deploy", "reason": "Deploy",
                    "env": ["DEPLOY_KEY"], "status": request_status }],
            }})
        };
        c.apply(&frame(2, "turn.updated", turn("waiting", "pending")))
            .unwrap();
        assert!(c.secret_request(request).unwrap().is_pending());
        assert_eq!(c.secret_requests().len(), 1);
        c.apply(&frame(3, "turn.updated", turn("queued", "stored")))
            .unwrap();
        assert_eq!(
            c.secret_request(request).unwrap().status,
            SecretRequestStatus::Stored
        );
        let plain = json!({ "turn": {
            "id": Uuid::from_u128(10), "request_id": Uuid::from_u128(11), "turn_no": 2,
            "provider_id": "synthetic", "status": "queued", "error": null,
            "effect_uncertain": false,
        }});
        assert_eq!(
            c.apply(&frame(4, "turn.updated", plain)),
            Ok(Applied::Changed)
        );
    }

    #[test]
    fn answered_questions_stay_before_their_continuation() {
        let mut before = item(2, 2, "assistant", "Please choose.");
        before["turn_id"] = json!(Uuid::from_u128(901));
        let mut after = item(3, 3, "assistant", "Continuing with your answer.");
        after["turn_id"] = json!(Uuid::from_u128(901));
        after["created_at"] = json!("2026-11-02T00:00:02Z");
        let conversation = Conversation::from_snapshot(snapshot(
            3,
            vec![item(1, 1, "user", "Help me choose."), before, after],
            None,
        ))
        .unwrap();
        assert_eq!(
            conversation.question_anchor(
                Uuid::from_u128(901),
                Some("2026-11-02T00:00:01Z".parse().unwrap())
            ),
            Some(Uuid::from_u128(2))
        );
        assert_eq!(
            conversation.question_anchor(Uuid::from_u128(901), None),
            Some(Uuid::from_u128(3))
        );
    }

    #[test]
    fn outgoing_revisions_and_receipts_survive_events_and_snapshot_reload() {
        let id = Uuid::from_u128(700);
        let next = Uuid::from_u128(701);
        let make_turn = |turn: u128, number, draft_id, status: &str, version| {
            json!({
                "id": Uuid::from_u128(turn), "request_id": Uuid::from_u128(turn + 10),
                "turn_no": number, "provider_id": "synthetic", "status": "waiting", "error": null,
                "effect_uncertain": false,
                "outgoing_drafts": [{"id": draft_id, "lineage_id": id, "revision": number,
                    "content": {"channel":"email", "from": "main@example.invalid", "to": ["ji@example.invalid"], "cc": [], "bcc": [],
                    "subject": "Edited subject", "body": "Edited message"}, "version": version,
                    "status": status, "can_approve": status == "pending", "result_detail": null,
                    "updated_at": "2026-11-05T00:00:02Z", "settled_at": null}]
            })
        };
        let mut c = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        c.apply(&frame(
            2,
            "turn.updated",
            json!({"turn": make_turn(900, 1, id, "pending", 2)}),
        ))
        .unwrap();
        assert_eq!(
            c.open_draft().unwrap().edit().subject.as_deref(),
            Some("Edited subject")
        );
        let old = make_turn(900, 1, id, "superseded", 2);
        let revision = make_turn(901, 2, next, "pending", 1);
        c.apply(&frame(3, "turn.updated", json!({"turn": old})))
            .unwrap();
        c.apply(&frame(4, "turn.updated", json!({"turn": revision})))
            .unwrap();
        assert_eq!(c.open_draft().unwrap().id, next);
        let mut snap = snapshot(4, vec![], None);
        snap.turns = serde_json::from_value(json!([old, revision])).unwrap();
        let reloaded = Conversation::from_snapshot(snap).unwrap();
        assert_eq!(reloaded.open_draft(), c.open_draft());
        let approval: Turn =
            serde_json::from_value(make_turn(901, 2, next, "approved", 1)).unwrap();
        let response = approval.outgoing_drafts[0].clone();
        c.apply(&frame(
            5,
            "turn.updated",
            json!({"turn": make_turn(901, 2, next, "approved", 1)}),
        ))
        .unwrap();
        assert!(c.open_draft().is_none());
        assert_eq!(
            c.outgoing_draft(next).unwrap().status,
            DraftStatus::Approved
        );
        c.apply(&frame(
            6,
            "turn.updated",
            json!({"turn": make_turn(901, 2, next, "sent", 1)}),
        ))
        .unwrap();
        c.update_draft(response);
        assert_eq!(
            c.outgoing_draft(next).unwrap().status,
            DraftStatus::Sent,
            "a delayed approval response cannot undo a sent event"
        );
    }

    #[test]
    fn a_reply_follows_up_no_draft_when_the_newest_is_on_an_unknown_channel() {
        let turn = |turn: u128, number: i64, content: serde_json::Value| {
            json!({
                "id": Uuid::from_u128(turn), "request_id": Uuid::from_u128(turn + 10),
                "turn_no": number, "provider_id": "synthetic", "status": "waiting", "error": null,
                "effect_uncertain": false,
                "outgoing_drafts": [{"id": Uuid::from_u128(turn + 100),
                    "lineage_id": Uuid::from_u128(turn + 100), "revision": 1, "content": content,
                    "version": 1, "status": "failed", "can_approve": false,
                    "result_detail": null, "updated_at": "2026-11-05T00:00:02Z",
                    "settled_at": null}]
            })
        };
        let mut c = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        let telegram = json!({"channel": "message", "platform": "telegram", "body": "Hi"});
        c.apply(&frame(
            2,
            "turn.updated",
            json!({"turn": turn(900, 1, telegram)}),
        ))
        .unwrap();
        assert_eq!(c.open_draft().unwrap().id, Uuid::from_u128(1000));
        let fax = json!({"channel": "fax", "body": "Hi"});
        c.apply(&frame(
            3,
            "turn.updated",
            json!({"turn": turn(901, 2, fax)}),
        ))
        .unwrap();
        assert_eq!(c.open_draft(), None, "nor the older Telegram draft");
    }

    #[test]
    fn turn_updates_carry_owner_questions_through_their_states() {
        let mut c = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        let question = Uuid::from_u128(78);
        let turn = |status: &str, question_status: &str, answer: Option<&str>| {
            json!({ "turn": {
                "id": Uuid::from_u128(9), "request_id": Uuid::from_u128(8), "turn_no": 1,
                "provider_id": "synthetic", "status": status, "error": null,
                "effect_uncertain": false,
                "questions": [{ "id": question, "kind": "destructive",
                    "title": "Remove the old site", "context": "rm -rf ~/work/old-site",
                    "choices": [{ "id": "approve", "label": "Approve once" },
                                { "id": "deny", "label": "Deny" },
                                { "id": "approve_forever", "label": "Always allow destructive actions" }],
                    "status": question_status, "answer": answer }],
            }})
        };
        c.apply(&frame(2, "turn.updated", turn("waiting", "pending", None)))
            .unwrap();
        let shown = c.question(question).unwrap();
        assert!(shown.is_pending() && shown.is_approval());
        assert_eq!(shown.choices.len(), 3);
        c.apply(&frame(
            3,
            "turn.updated",
            turn("queued", "answered", Some("approve")),
        ))
        .unwrap();
        let shown = c.question(question).unwrap();
        assert_eq!(shown.chosen().unwrap().label, "Approve once");
        c.apply(&frame(
            4,
            "turn.updated",
            turn("interrupted", "skipped", None),
        ))
        .unwrap();
        assert_eq!(
            c.question(question).unwrap().status,
            QuestionStatus::Skipped
        );
        assert_eq!(c.question(question).unwrap().chosen(), None);
    }

    #[test]
    fn multiple_question_answers_survive_events_and_snapshot_reload() {
        let mut c = Conversation::from_snapshot(snapshot(1, vec![], None)).unwrap();
        let question = Uuid::from_u128(79);
        let turn = |status: &str, answer: serde_json::Value| {
            json!({
                "id": Uuid::from_u128(9), "request_id": Uuid::from_u128(8), "turn_no": 1,
                "provider_id": "synthetic", "status": "waiting", "error": null,
                "effect_uncertain": false,
                "questions": [{ "id": question, "kind": "choice", "title": "Which tasks?",
                    "choices": [{"id": "a", "label": "Archive"}, {"id": "b", "label": "Summarize"}],
                    "multiple": true, "status": status, "answer": answer }],
            })
        };
        c.apply(&frame(
            2,
            "turn.updated",
            json!({"turn": turn("pending", serde_json::Value::Null)}),
        ))
        .unwrap();
        assert!(c.question(question).unwrap().multiple);
        let answered = turn("answered", json!(["a", "b"]));
        c.apply(&frame(3, "turn.updated", json!({"turn": answered})))
            .unwrap();
        let answer = c.question(question).unwrap().answer.as_ref().unwrap();
        assert!(answer.contains("a") && answer.contains("b"));
        assert!(answer.single().is_none());
        let mut snap = snapshot(3, vec![], None);
        snap.turns = serde_json::from_value(json!([answered])).unwrap();
        let reloaded = Conversation::from_snapshot(snap).unwrap();
        assert_eq!(reloaded.question(question), c.question(question));
        c.apply(&frame(
            4,
            "turn.updated",
            json!({"turn": turn("skipped", serde_json::Value::Null)}),
        ))
        .unwrap();
        assert!(!c.question(question).unwrap().is_pending());
        assert!(c.question(question).unwrap().answer.is_none());
    }
}

#[cfg(test)]
mod vault_request_tests {
    use else_wire::secrets::SecretRequest;
    #[test]
    fn vault_requests_read_optional_metadata_and_older_payloads() {
        let mut data = serde_json::json!({"id": uuid::Uuid::nil(), "name": "x_login", "reason": "Sign in", "env": ["USERNAME", "PASSWORD"], "status": "pending"});
        let old: SecretRequest = serde_json::from_value(data.clone()).unwrap();
        assert!(old.description.is_none());
        assert!(old.website.is_none());
        data["description"] = serde_json::json!("X, @owner");
        data["website"] = serde_json::json!("x.com");
        let request: SecretRequest = serde_json::from_value(data).unwrap();
        assert_eq!(request.description.as_deref(), Some("X, @owner"));
        assert_eq!(request.website.as_deref(), Some("x.com"));
        assert!(request.is_pending());
    }
}
