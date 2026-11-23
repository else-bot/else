//! One bot's loaded conversation: its newest pages and the live events
//! applied on top, in cursor order, as the desktop applies them.

use std::collections::{BTreeMap, HashMap};

use else_wire::bots::{Activity, Phase};
use else_wire::conversation::{BotExtras, BotUpdated, Event, Snapshot};
use else_wire::items::{Attachment, Item, ItemKind, MessagePart};
use else_wire::outgoing::{DraftStatus, OutgoingDraft};
use else_wire::questions::{Question, QuestionStatus};
use else_wire::secrets::{SecretRequest, SecretRequestStatus};
use else_wire::turns::{Turn, TurnStatus};
use uuid::Uuid;

/// Why the loaded conversation no longer follows the stream; the fix is
/// always a fresh snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reset {
    Gap,
    BadCursor,
    UnknownItem,
    OffsetMismatch,
    Undecodable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Already applied (a replay); nothing changed.
    Duplicate,
    Changed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Conversation {
    /// `<server_instance_id>:<bot_id>`; events must carry the same prefix.
    cursor_prefix: String,
    applied: i64,
    items: BTreeMap<i64, Item>,
    seq_of: HashMap<Uuid, i64>,
    turns: HashMap<Uuid, Turn>,
    pub older_before_seq: Option<i64>,
    pub activity: Activity,
    /// The bot's routines, reminders, watchers and triggers.
    pub extras: BotExtras,
}

/// An owner message's text and its files. A message with files keeps its
/// text parts; the markers its plain text spells the files with are left
/// out, since the files show on their own.
fn owner_parts(item: &Item) -> (String, Vec<Attachment>) {
    if item.parts.is_empty() {
        return (item.text.clone(), Vec::new());
    }
    let mut text = String::new();
    let mut files = Vec::new();
    for part in &item.parts {
        match part {
            MessagePart::Text { text: t } => text.push_str(t),
            MessagePart::File { file } => files.push(file.clone()),
        }
    }
    (text.trim().to_owned(), files)
}

fn split_cursor(cursor: &str) -> Option<(&str, i64)> {
    let (prefix, seq) = cursor.rsplit_once(':')?;
    Some((prefix, seq.parse().ok()?))
}

/// One row of the transcript, as the phone shows it.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Owner {
        id: Uuid,
        /// Its text without the files' markers.
        text: String,
        files: Vec<Attachment>,
        /// What became of the reply to it, when it did not complete.
        outcome: Option<Outcome>,
    },
    Bot {
        id: Uuid,
        text: String,
        streaming: bool,
    },
    /// A receipt or notice, centered and quiet.
    Note { id: Uuid, text: String },
}

/// Something the bot waits on the owner to settle, shown as a card.
#[derive(Debug, Clone, PartialEq)]
pub enum Ask {
    Question(Box<Question>),
    Draft(Box<OutgoingDraft>),
    Secret(Box<SecretRequest>),
}

impl Ask {
    pub fn key(&self) -> String {
        match self {
            Self::Question(question) => format!("q-{}", question.id),
            Self::Draft(draft) => format!("d-{}", draft.id),
            Self::Secret(request) => format!("s-{}", request.id),
        }
    }
}

/// A reply that did not complete, as the owner's message shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The owner stopped it.
    Stopped,
    Failed(String),
    /// The server cannot tell whether it ran.
    Unknown,
}

impl Outcome {
    pub fn text(&self) -> &str {
        match self {
            Self::Stopped => "Stopped",
            Self::Failed(reason) => reason,
            Self::Unknown => "The outcome of this reply is unknown",
        }
    }
}

impl Row {
    pub fn key(&self) -> String {
        match self {
            Self::Owner { id, .. } | Self::Bot { id, .. } | Self::Note { id, .. } => id.to_string(),
        }
    }
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
            activity: snapshot.activity,
            extras: snapshot.extras,
        };
        for item in snapshot.items {
            conversation.upsert_item(item);
        }
        for turn in snapshot.turns {
            conversation.turns.insert(turn.id, turn);
        }
        Ok(conversation)
    }

    /// The cursor of the last applied event.
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

    /// Applies the next channel message. Replays at or below the applied
    /// cursor are ignored; anything else out of order needs a snapshot.
    pub fn apply(&mut self, id: &str, message: &serde_json::Value) -> Result<Applied, Reset> {
        let (prefix, seq) = split_cursor(id).ok_or(Reset::BadCursor)?;
        if prefix != self.cursor_prefix {
            return Err(Reset::BadCursor);
        }
        if seq <= self.applied {
            return Ok(Applied::Duplicate);
        }
        if seq != self.applied + 1 {
            return Err(Reset::Gap);
        }
        let event =
            serde_json::from_value::<Event>(message.clone()).map_err(|_| Reset::Undecodable)?;
        match event {
            Event::ItemUpsert(upsert) => self.upsert_item(upsert.item),
            Event::ItemDelta(delta) => {
                let at = *self.seq_of.get(&delta.item_id).ok_or(Reset::UnknownItem)?;
                let item = self.items.get_mut(&at).ok_or(Reset::UnknownItem)?;
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
                if let Some(activity) = bot.activity {
                    self.activity = activity;
                }
                self.extras.update(extras);
            }
        }
        self.applied = seq;
        Ok(Applied::Changed)
    }

    /// Records the owner message the server accepted, before its event
    /// arrives; the event replaces it with the same item.
    /// A late response never replaces what the stream already delivered.
    pub fn insert_sent(&mut self, item: Item, turn: Turn) {
        if !self.seq_of.contains_key(&item.id) {
            self.upsert_item(item);
        }
        self.turns.entry(turn.id).or_insert(turn);
    }

    /// Merges an older page; it never replaces newer loaded text.
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

    /// Whether the bot is replying or has replies queued.
    pub fn busy(&self) -> bool {
        !matches!(self.activity.phase, Phase::Idle | Phase::Unknown)
    }

    /// The request of the turn the bot works on now, to stop it.
    pub fn active_request(&self) -> Option<Uuid> {
        let turn = self.activity.active_turn_id?;
        self.turns.get(&turn).map(|turn| turn.request_id)
    }

    /// The transcript, oldest first.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::with_capacity(self.items.len());
        let mut previous: Option<Uuid> = None;
        for item in self.items.values() {
            if previous.is_some_and(|turn| turn != item.turn_id) {
                self.receipts(previous, &mut rows);
            }
            previous = Some(item.turn_id);
            let turn = self.turns.get(&item.turn_id);
            match item.kind {
                ItemKind::User => {
                    if let Some(task) = &item.bot_task {
                        let title = task.title.as_deref().unwrap_or("a task");
                        rows.push(Row::Note {
                            id: item.id,
                            text: format!("{} sent {title}", task.sender_name),
                        });
                        continue;
                    }
                    let (text, files) = owner_parts(item);
                    rows.push(Row::Owner {
                        id: item.id,
                        text,
                        files,
                        outcome: turn.and_then(outcome),
                    });
                }
                ItemKind::Assistant => {
                    if item.text.trim().is_empty() {
                        continue;
                    }
                    let streaming = turn.is_some_and(|turn| turn.status.is_working())
                        && self.activity.active_turn_id == Some(item.turn_id);
                    rows.push(Row::Bot {
                        id: item.id,
                        text: item.text.clone(),
                        streaming,
                    });
                }
                ItemKind::Status | ItemKind::Routine => {
                    if let Some(reply) = &item.bot_reply {
                        let title = reply.title.as_deref().unwrap_or("its task");
                        rows.push(Row::Note {
                            id: item.id,
                            text: format!("{} finished {title}", reply.sender_name),
                        });
                    } else if !item.text.trim().is_empty() {
                        rows.push(Row::Note {
                            id: item.id,
                            text: item.text.clone(),
                        });
                    }
                }
                ItemKind::Connector | ItemKind::Unknown => {}
            }
        }
        self.receipts(previous, &mut rows);
        rows
    }

    /// The settled questions and drafts of `turn`, after its messages.
    fn receipts(&self, turn: Option<Uuid>, rows: &mut Vec<Row>) {
        let Some(turn) = turn.and_then(|turn| self.turns.get(&turn)) else {
            return;
        };
        for question in &turn.questions {
            let text = match (&question.status, &question.answer) {
                (QuestionStatus::Answered, Some(answer)) => {
                    let labels: Vec<&str> = question
                        .choices
                        .iter()
                        .filter(|choice| answer.contains(&choice.id))
                        .map(|choice| choice.label.as_str())
                        .collect();
                    format!("You answered “{}”: {}", question.title, labels.join(", "))
                }
                (QuestionStatus::Skipped | QuestionStatus::Cancelled, _) => {
                    format!("“{}” was closed without an answer", question.title)
                }
                _ => continue,
            };
            rows.push(Row::Note {
                id: question.id,
                text,
            });
        }
        for request in &turn.secret_requests {
            let text = match request.status {
                SecretRequestStatus::Stored => format!("You saved {}", request.name),
                // Declined by the owner or withdrawn with its turn.
                SecretRequestStatus::Cancelled => format!("{} was not saved", request.name),
                SecretRequestStatus::Pending | SecretRequestStatus::Unknown => continue,
            };
            rows.push(Row::Note {
                id: request.id,
                text,
            });
        }
        for draft in &turn.outgoing_drafts {
            if !draft.is_open() && draft.status != DraftStatus::Superseded {
                rows.push(Row::Note {
                    id: draft.id,
                    text: format!("{}: {}", draft.content.title(), draft.label()),
                });
            }
        }
    }

    /// The open questions and drafts, oldest turn first.
    pub fn asks(&self) -> Vec<Ask> {
        let mut turns: Vec<&Turn> = self.turns.values().collect();
        turns.sort_by_key(|turn| turn.turn_no);
        let mut asks = Vec::new();
        for turn in turns {
            asks.extend(
                turn.questions
                    .iter()
                    .filter(|q| q.is_pending())
                    .cloned()
                    .map(|q| Ask::Question(Box::new(q))),
            );
            asks.extend(
                turn.outgoing_drafts
                    .iter()
                    .filter(|d| d.is_open())
                    .cloned()
                    .map(|d| Ask::Draft(Box::new(d))),
            );
            asks.extend(
                turn.secret_requests
                    .iter()
                    .filter(|r| r.is_pending())
                    .cloned()
                    .map(|r| Ask::Secret(Box::new(r))),
            );
        }
        asks
    }

    /// Whether the newest item is the owner's and the bot has not started
    /// writing: shown as a typing indicator.
    pub fn thinking(&self) -> bool {
        if !self.busy() {
            return false;
        }
        self.items
            .values()
            .next_back()
            .is_none_or(|item| item.kind != ItemKind::Assistant)
    }
}

fn outcome(turn: &Turn) -> Option<Outcome> {
    match turn.status {
        TurnStatus::Failed => Some(Outcome::Failed(
            turn.error
                .as_ref()
                .and_then(|error| error.message.clone())
                .unwrap_or_else(|| "The reply failed".into()),
        )),
        TurnStatus::Uncertain => Some(Outcome::Unknown),
        TurnStatus::Interrupted => Some(Outcome::Stopped),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TURN: &str = "00000000-0000-0000-0000-000000000001";
    const ITEM: &str = "00000000-0000-0000-0000-000000000002";
    const REQUEST: &str = "00000000-0000-0000-0000-000000000003";

    fn snapshot(cursor: &str) -> Snapshot {
        serde_json::from_value(json!({
            "selected_provider_id": null,
            "event_cursor": cursor,
            "items": [{
                "id": ITEM, "seq": 1, "turn_id": TURN, "request_id": null,
                "kind": "assistant", "text": "Hel", "provider_id": "codex",
            }],
            "turns": [{
                "id": TURN, "request_id": REQUEST, "turn_no": 1, "provider_id": "codex",
                "status": "running", "error": null, "effect_uncertain": false,
            }],
            "activity": { "phase": "working", "active_turn_id": TURN, "queued_count": 0 },
            "older_before_seq": null,
        }))
        .unwrap()
    }

    fn delta(offset: u64, text: &str) -> serde_json::Value {
        json!({ "event": "item.delta", "data": { "item_id": ITEM, "offset_utf8": offset, "text": text } })
    }

    #[test]
    fn deltas_in_cursor_order_extend_the_streaming_reply() {
        let mut conversation = Conversation::from_snapshot(snapshot("srv:bot:4")).unwrap();
        assert_eq!(
            conversation.apply("srv:bot:5", &delta(3, "lo")),
            Ok(Applied::Changed)
        );
        assert_eq!(
            conversation.rows(),
            vec![Row::Bot {
                id: ITEM.parse().unwrap(),
                text: "Hello".into(),
                streaming: true,
            }]
        );
        assert_eq!(conversation.cursor(), "srv:bot:5");
        assert_eq!(
            conversation.active_request(),
            Some(REQUEST.parse().unwrap())
        );
    }

    #[test]
    fn a_replayed_event_changes_nothing() {
        let mut conversation = Conversation::from_snapshot(snapshot("srv:bot:4")).unwrap();
        assert_eq!(
            conversation.apply("srv:bot:4", &delta(3, "lo")),
            Ok(Applied::Duplicate)
        );
        assert_eq!(conversation.cursor(), "srv:bot:4");
    }

    #[test]
    fn a_skipped_event_or_a_wrong_offset_needs_a_snapshot() {
        let mut conversation = Conversation::from_snapshot(snapshot("srv:bot:4")).unwrap();
        assert_eq!(
            conversation.apply("srv:bot:6", &delta(3, "lo")),
            Err(Reset::Gap)
        );
        assert_eq!(
            conversation.apply("srv:bot:5", &delta(1, "lo")),
            Err(Reset::OffsetMismatch)
        );
        assert_eq!(
            conversation.apply("other:bot:5", &delta(3, "lo")),
            Err(Reset::BadCursor)
        );
    }

    #[test]
    fn a_late_send_response_keeps_the_streamed_turn() {
        let mut conversation = Conversation::from_snapshot(snapshot("srv:bot:4")).unwrap();
        let stale: Turn = serde_json::from_value(json!({
            "id": TURN, "request_id": REQUEST, "turn_no": 1, "provider_id": "codex",
            "status": "queued", "error": null, "effect_uncertain": false,
        }))
        .unwrap();
        let item = conversation.items.values().next().unwrap().clone();
        conversation.insert_sent(item, stale);
        assert!(conversation.rows().iter().any(|row| matches!(
            row,
            Row::Bot {
                streaming: true,
                ..
            }
        )));
    }

    #[test]
    fn a_pending_question_is_an_ask_until_answered() {
        let mut conversation = Conversation::from_snapshot(snapshot("srv:bot:4")).unwrap();
        let question = |status: &str| {
            json!({ "event": "turn.updated", "data": { "turn": {
                "id": TURN, "request_id": REQUEST, "turn_no": 1, "provider_id": "codex",
                "status": "waiting", "error": null, "effect_uncertain": false,
                "questions": [{
                    "id": "00000000-0000-0000-0000-00000000000a", "kind": "choice",
                    "title": "Which plan?", "choices": [{ "id": "a", "label": "A" }],
                    "status": status,
                }],
            }}})
        };
        conversation
            .apply("srv:bot:5", &question("pending"))
            .unwrap();
        assert!(matches!(&conversation.asks()[..], [Ask::Question(q)] if q.title == "Which plan?"));
        let answered = question("answered");
        let mut answered = answered;
        answered["data"]["turn"]["questions"][0]["answer"] = json!("a");
        conversation.apply("srv:bot:6", &answered).unwrap();
        assert_eq!(conversation.asks(), vec![]);
        assert_eq!(
            conversation.rows().last(),
            Some(&Row::Note {
                id: "00000000-0000-0000-0000-00000000000a".parse().unwrap(),
                text: "You answered “Which plan?”: A".into(),
            })
        );
    }

    #[test]
    fn a_pending_secret_request_is_an_ask_until_settled() {
        let mut conversation = Conversation::from_snapshot(snapshot("srv:bot:4")).unwrap();
        let request = |status: &str| {
            json!({ "event": "turn.updated", "data": { "turn": {
                "id": TURN, "request_id": REQUEST, "turn_no": 1, "provider_id": "codex",
                "status": "waiting", "error": null, "effect_uncertain": false,
                "secret_requests": [{
                    "id": "00000000-0000-0000-0000-00000000000b", "name": "smtp",
                    "reason": "To send mail", "env": ["SMTP_PASSWORD"],
                    "description": null, "website": null, "status": status,
                }],
            }}})
        };
        conversation
            .apply("srv:bot:5", &request("pending"))
            .unwrap();
        assert!(matches!(&conversation.asks()[..], [Ask::Secret(r)] if r.name == "smtp"));
        conversation
            .apply("srv:bot:6", &request("cancelled"))
            .unwrap();
        assert_eq!(conversation.asks(), vec![]);
        assert_eq!(
            conversation.rows().last(),
            Some(&Row::Note {
                id: "00000000-0000-0000-0000-00000000000b".parse().unwrap(),
                text: "smtp was not saved".into(),
            })
        );
    }

    #[test]
    fn a_failed_turn_marks_the_owner_message() {
        let mut conversation = Conversation::from_snapshot(snapshot("srv:bot:4")).unwrap();
        let owner = json!({ "event": "item.upsert", "data": { "item": {
            "id": "00000000-0000-0000-0000-000000000009", "seq": 2, "turn_id": TURN,
            "request_id": REQUEST, "kind": "user", "text": "hi", "provider_id": null,
        }}});
        let failed = json!({ "event": "turn.updated", "data": { "turn": {
            "id": TURN, "request_id": REQUEST, "turn_no": 1, "provider_id": "codex",
            "status": "failed", "error": { "code": "provider_failed", "message": "Codex is signed out." },
            "effect_uncertain": false,
        }}});
        conversation.apply("srv:bot:5", &owner).unwrap();
        conversation.apply("srv:bot:6", &failed).unwrap();
        assert_eq!(
            conversation.rows().last(),
            Some(&Row::Owner {
                id: "00000000-0000-0000-0000-000000000009".parse().unwrap(),
                text: "hi".into(),
                files: vec![],
                outcome: Some(Outcome::Failed("Codex is signed out.".into())),
            })
        );
    }

    #[test]
    fn a_bot_update_refreshes_its_routines() {
        let mut conversation = Conversation::from_snapshot(snapshot("srv:bot:4")).unwrap();
        assert_eq!(conversation.extras.routines, None);
        let update = json!({ "event": "bot.updated", "data": {
            "bot": { "id": Uuid::nil(), "name": "Inbox" }, "routines": [],
        } });
        conversation.apply("srv:bot:5", &update).unwrap();
        assert_eq!(conversation.extras.routines, Some(vec![]));
    }
}
