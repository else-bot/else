//! The client's conversation model: a snapshot plus contiguously applied
//! stream events. Any inconsistency (a gap, an unknown item, an offset
//! mismatch, an unknown kind) asks the caller to reload a snapshot rather than
//! guessing, so a bubble is never duplicated or corrupted.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{Activity, Bot};

/// One event of a `bot:<id>` channel: its cursor, kind, and JSON data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub id: Option<String>,
    pub event: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Item {
    pub id: Uuid,
    pub seq: i64,
    pub turn_id: Uuid,
    pub request_id: Option<Uuid>,
    pub kind: String,
    pub text: String,
    pub provider_id: Option<String>,
    /// Absent from older servers.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// Set on the owner's feedback message sent as a reaction.
    #[serde(default)]
    pub reaction: Option<Reaction>,
    #[serde(default)]
    pub bot_task: Option<BotTask>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BotTask {
    pub sender_bot_id: Uuid,
    pub sender_name: String,
    pub prompt: String,
}

/// The owner's reaction to one of the bot's messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub item_id: Uuid,
    pub kind: ReactionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactionKind {
    ThumbsUp,
    ThumbsDown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TurnError {
    pub code: String,
    pub message: Option<String>,
}

/// A bot's request for credentials: metadata only, never a value.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SecretRequest {
    pub id: Uuid,
    pub name: String,
    pub reason: String,
    pub env: Vec<String>,
    /// `pending`, `stored`, or `cancelled`.
    pub status: String,
}

impl SecretRequest {
    pub fn is_pending(&self) -> bool {
        self.status == "pending"
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Turn {
    pub id: Uuid,
    pub request_id: Uuid,
    pub turn_no: i64,
    pub provider_id: String,
    pub status: String,
    pub error: Option<TurnError>,
    pub effect_uncertain: bool,
    #[serde(default)]
    pub secret_requests: Vec<SecretRequest>,
}

/// The last due occurrence of a routine: the turn it started, or why it
/// was skipped.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LastRun {
    pub due_at: DateTime<Utc>,
    /// `queued`, `running`, `skipped`, or the turn's terminal status.
    pub status: String,
    pub skip_reason: Option<String>,
    pub error: Option<TurnError>,
}

/// A bot's saved task, run on a cron schedule in its conversation.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Routine {
    pub id: Uuid,
    pub name: String,
    pub prompt: String,
    pub cron: String,
    pub timezone: String,
    pub enabled: bool,
    pub next_run_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub last_run: Option<LastRun>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    pub selected_provider_id: Option<String>,
    pub event_cursor: String,
    pub items: Vec<Item>,
    pub turns: Vec<Turn>,
    pub activity: Activity,
    pub older_before_seq: Option<i64>,
    /// Absent from servers without routines.
    #[serde(default)]
    pub routines: Vec<Routine>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub first: bool,
    pub last: bool,
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
    pub selected_provider_id: Option<String>,
    pub activity: Option<Activity>,
    routines: Vec<Routine>,
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
            selected_provider_id: snapshot.selected_provider_id,
            activity: Some(snapshot.activity),
            routines: snapshot.routines,
        };
        for item in snapshot.items {
            conversation.upsert_item(item);
        }
        for turn in snapshot.turns {
            conversation.turns.insert(turn.id, turn);
        }
        Ok(conversation)
    }

    /// The cursor of the last applied event, for `Last-Event-ID`.
    pub fn cursor(&self) -> String {
        format!("{}:{}", self.cursor_prefix, self.applied)
    }

    fn upsert_item(&mut self, item: Item) {
        if let Some(old_seq) = self.seq_of.insert(item.id, item.seq) {
            if old_seq != item.seq {
                self.items.remove(&old_seq);
            }
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
        let data: serde_json::Value =
            serde_json::from_str(&frame.data).map_err(|_| Reset::Malformed)?;
        match frame.event.as_str() {
            "item.upsert" => {
                let item: Item =
                    serde_json::from_value(data["item"].clone()).map_err(|_| Reset::Malformed)?;
                self.upsert_item(item);
            }
            "item.delta" => {
                let item_id: Uuid = serde_json::from_value(data["item_id"].clone())
                    .map_err(|_| Reset::Malformed)?;
                let offset = data["offset_utf8"].as_u64().ok_or(Reset::Malformed)? as usize;
                let text = data["text"].as_str().ok_or(Reset::Malformed)?;
                let seq_of_item = *self.seq_of.get(&item_id).ok_or(Reset::UnknownItem)?;
                let item = self.items.get_mut(&seq_of_item).ok_or(Reset::UnknownItem)?;
                if item.text.len() != offset {
                    return Err(Reset::OffsetMismatch);
                }
                item.text.push_str(text);
            }
            "turn.updated" => {
                let turn: Turn =
                    serde_json::from_value(data["turn"].clone()).map_err(|_| Reset::Malformed)?;
                self.turns.insert(turn.id, turn);
            }
            "bot.updated" => {
                let bot: Bot =
                    serde_json::from_value(data["bot"].clone()).map_err(|_| Reset::Malformed)?;
                self.activity = bot.activity;
                if bot.selected_provider_id.is_some() {
                    self.selected_provider_id = bot.selected_provider_id;
                }
                if let Some(routines) = data.get("routines") {
                    self.routines =
                        serde_json::from_value(routines.clone()).map_err(|_| Reset::Malformed)?;
                }
            }
            other => return Err(Reset::UnknownKind(other.to_owned())),
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

    /// Show the canonical owner item from a send response before its event
    /// arrives. Idempotent: the later upsert replaces it by ID.
    pub fn insert_item(&mut self, item: Item) {
        if !self.seq_of.contains_key(&item.id) {
            self.upsert_item(item);
        }
    }

    /// Record a turn from a send response; a later event replaces it.
    pub fn insert_turn(&mut self, turn: Turn) {
        self.turns.entry(turn.id).or_insert(turn);
    }

    pub fn items(&self) -> impl DoubleEndedIterator<Item = &Item> {
        self.items.values()
    }

    pub fn latest_reply(&self) -> Option<&Item> {
        self.items
            .values()
            .rev()
            .find(|item| item.kind == "assistant")
    }

    pub fn item(&self, id: Uuid) -> Option<&Item> {
        self.seq_of.get(&id).and_then(|seq| self.items.get(seq))
    }

    pub fn turn(&self, id: Uuid) -> Option<&Turn> {
        self.turns.get(&id)
    }

    /// Where `id` sits in its run of same-kind items from one turn, such as
    /// the agent messages of one reply.
    pub fn run(&self, id: Uuid) -> Run {
        let Some((seq, item)) = self.seq_of.get(&id).zip(self.item(id)) else {
            return Run {
                first: true,
                last: true,
            };
        };
        let same = |other: Option<(&i64, &Item)>| {
            other.is_some_and(|(_, o)| o.kind == item.kind && o.turn_id == item.turn_id)
        };
        Run {
            first: !same(self.items.range(..seq).next_back()),
            last: !same(self.items.range(seq + 1..).next()),
        }
    }

    /// Every secret request of the loaded turns, by turn number.
    pub fn secret_requests(&self) -> Vec<(&Turn, &SecretRequest)> {
        let mut turns: Vec<&Turn> = self.turns.values().collect();
        turns.sort_by_key(|t| t.turn_no);
        turns
            .into_iter()
            .flat_map(|t| t.secret_requests.iter().map(move |r| (t, r)))
            .collect()
    }

    pub fn secret_request(&self, id: Uuid) -> Option<&SecretRequest> {
        self.turns
            .values()
            .flat_map(|t| &t.secret_requests)
            .find(|r| r.id == id)
    }

    pub fn routines(&self) -> &[Routine] {
        &self.routines
    }

    /// A routine from a change response. Only a newer version replaces the
    /// one the stream delivered, so a late response never undoes an event.
    pub fn apply_routine(&mut self, routine: Routine) {
        if let Some(current) = self.routines.iter_mut().find(|r| r.id == routine.id)
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

    pub fn has_request(&self, request_id: Uuid) -> bool {
        self.items
            .values()
            .any(|item| item.request_id == Some(request_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn snapshot(seq: i64, items: Vec<serde_json::Value>, older: Option<i64>) -> Snapshot {
        serde_json::from_value(json!({
            "conversation_id": Uuid::new_v4(), "bot_id": bot(), "selected_provider_id": null,
            "event_cursor": format!("{PREFIX}:{seq}"), "items": items, "turns": [],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 },
            "older_before_seq": older,
        }))
        .unwrap()
    }

    fn frame(seq: i64, event: &str, data: serde_json::Value) -> Frame {
        Frame {
            id: Some(format!("{PREFIX}:{seq}")),
            event: event.into(),
            data: data.to_string(),
        }
    }

    fn delta(seq: i64, id: u128, offset: usize, text: &str) -> Frame {
        frame(
            seq,
            "item.delta",
            json!({ "item_id": Uuid::from_u128(id), "offset_utf8": offset, "text": text }),
        )
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
        // A replay of an applied event is ignored and changes nothing.
        assert_eq!(c.apply(&delta(5, 2, 2, "✓日")), Ok(Applied::Duplicate));
        assert_eq!(c.apply(&delta(4, 2, 0, "x")), Ok(Applied::Duplicate));
        assert_eq!(c.item(Uuid::from_u128(2)).unwrap().text, "é✓日");
        assert_eq!(c.cursor(), format!("{PREFIX}:5"));
        assert_eq!(c.items().count(), 2);
    }

    #[test]
    fn agent_messages_of_one_turn_form_a_run() {
        let reply = |id, seq, text| {
            let mut item = item(id, seq, "assistant", text);
            item["turn_id"] = json!(Uuid::from_u128(901));
            item
        };
        let c = Conversation::from_snapshot(snapshot(
            1,
            vec![
                item(1, 1, "user", "hi"),
                reply(2, 2, "one"),
                reply(3, 3, "two"),
                reply(4, 4, "three"),
                item(5, 5, "user", "next"),
            ],
            None,
        ))
        .unwrap();
        let runs: Vec<_> = (1..=5)
            .map(|id| c.run(Uuid::from_u128(id)))
            .map(|r| (r.first, r.last))
            .collect();
        assert_eq!(
            runs,
            vec![
                (true, true),
                (true, false),
                (false, false),
                (false, true),
                (true, true)
            ]
        );
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
        let mut other = delta(2, 2, 2, "c");
        other.id = Some(
            "33333333-3333-3333-3333-333333333333:22222222-2222-2222-2222-222222222222:2".into(),
        );
        assert_eq!(c.apply(&other), Err(Reset::BadCursor));
        // Nothing was applied by the failed attempts.
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
        first.routines =
            vec![serde_json::from_value(routine(true, "2026-11-01T00:00:00Z", None)).unwrap()];
        let mut c = Conversation::from_snapshot(first).unwrap();
        assert_eq!(c.routines().len(), 1);
        assert!(c.routines()[0].enabled);

        c.apply(&bot_updated(
            2,
            Some(vec![routine(true, "2026-11-01T00:00:00Z", Some("failed"))]),
        ))
        .unwrap();
        let last = c.routines()[0].last_run.as_ref().unwrap();
        assert_eq!(last.status, "failed");
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
    fn a_late_change_response_never_undoes_a_newer_event() {
        let mut first = snapshot(1, vec![], None);
        first.routines =
            vec![serde_json::from_value(routine(true, "2026-11-01T00:00:00Z", None)).unwrap()];
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
        assert_eq!(c.secret_request(request).unwrap().status, "stored");
        // Servers without secret requests still parse.
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
}
