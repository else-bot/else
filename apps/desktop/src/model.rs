//! The client's conversation model: a snapshot plus contiguously applied
//! stream events. Any inconsistency (a gap, an unknown item, an offset
//! mismatch, an unknown kind) asks the caller to reload a snapshot rather than
//! guessing, so a bubble is never duplicated or corrupted.

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;
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

impl Turn {
    pub fn is_finished(&self) -> bool {
        matches!(
            self.status.as_str(),
            "completed" | "interrupted" | "failed" | "uncertain"
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    pub selected_provider_id: Option<String>,
    pub event_cursor: String,
    pub items: Vec<Item>,
    pub turns: Vec<Turn>,
    pub activity: Activity,
    pub older_before_seq: Option<i64>,
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
            }
            other => return Err(Reset::UnknownKind(other.to_owned())),
        }
        self.applied = seq;
        Ok(Applied::Changed)
    }

    /// Merge an older page. It never replaces newer loaded text or moves
    /// the live cursor.
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
