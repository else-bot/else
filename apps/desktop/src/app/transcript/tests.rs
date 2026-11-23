use super::*;
use crate::app::conversation::row_index;
use crate::app::reply::Author::{self, Bot, Owner};
use chrono::{DateTime, FixedOffset, Utc};
use else_wire::bots::ReplyPosition;

use crate::model::Conversation;

use else_wire::conversation::Snapshot;
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
            [(
                RowKey::Card(CardKey::OwnerNotice(Uuid::from_u128(400))),
                Some(receipt)
            )]
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
    let snapshot: Snapshot = serde_json::from_value(card_page("pending", json!([]), None)).unwrap();
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
        RowKey::Card(CardKey::Draft(Uuid::from_u128(310))).task_sender(&c),
        Some("Sentry")
    );
    assert_eq!(
        RowKey::Card(CardKey::Question(Uuid::from_u128(210))).task_sender(&c),
        Some("Sentry")
    );
    assert_eq!(
        RowKey::Card(CardKey::Question(Uuid::nil())).task_sender(&c),
        None
    );
    assert_eq!(RowKey::Item(Uuid::from_u128(1)).task_sender(&c), None);
}

#[test]
fn a_draft_sent_by_a_turn_without_visible_items_follows_earlier_history() {
    let earlier = json!([{"id": Uuid::from_u128(1), "seq": 1, "turn_id": Uuid::from_u128(20),
        "kind": "user", "text": "Earlier request", "created_at": "2026-11-01T00:00:00Z"}]);
    assert_eq!(
        card_rows(card_page("sent", earlier, None)),
        [(
            RowKey::Card(CardKey::Draft(Uuid::from_u128(310))),
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
            RowKey::Card(CardKey::Draft(Uuid::from_u128(310))),
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
    let mut c = Conversation::from_snapshot(draft_page("sent", 2, "2026-11-02T00:00:00Z")).unwrap();
    assert!(
        request_rows(&c).is_empty(),
        "a sent draft must not reappear at the conversation tail"
    );
    c.merge_older(draft_page("sent", 1, "2026-11-01T00:00:00Z"));
    assert_eq!(
        request_rows(&c),
        [(
            RowKey::Card(CardKey::Draft(Uuid::from_u128(310))),
            Some(Uuid::from_u128(1))
        )]
    );
    let pending =
        Conversation::from_snapshot(draft_page("pending", 2, "2026-11-02T00:00:00Z")).unwrap();
    assert_eq!(
        request_rows(&pending),
        [(
            RowKey::Card(CardKey::Draft(Uuid::from_u128(310))),
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
            RowKey::Card(CardKey::Draft(Uuid::from_u128(310))),
            Some(Uuid::from_u128(1))
        )]
    );
}

#[test]
fn closed_questions_wait_for_their_history_page() {
    for status in ["cancelled", "answered", "skipped"] {
        let mut c =
            Conversation::from_snapshot(question_page(status, 2, "2026-11-02T00:00:00Z")).unwrap();
        assert!(
            request_rows(&c).is_empty(),
            "a closed connection card must not reappear at the conversation tail"
        );
        c.merge_older(question_page("pending", 1, "2026-11-01T00:00:00Z"));
        assert_eq!(
            request_rows(&c),
            [(
                RowKey::Card(CardKey::Question(Uuid::from_u128(210))),
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
                RowKey::Card(CardKey::Handoff(Uuid::from_u128(20))),
                Some(Uuid::from_u128(1))
            ),
            (
                RowKey::Card(CardKey::Handoff(Uuid::from_u128(22))),
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
fn tasks_carried_from_earlier_messages_stand_alone_and_a_live_council_stays_in_the_transcript() {
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
        [(RowKey::Card(CardKey::Handoff(Uuid::from_u128(22))), None)]
    );
}

#[test]
fn pending_questions_remain_visible_without_loaded_messages() {
    let mut page = question_page("pending", 2, "2026-11-02T00:00:00Z");
    page.items.clear();
    let c = Conversation::from_snapshot(page).unwrap();
    assert_eq!(
        request_rows(&c),
        [(RowKey::Card(CardKey::Question(Uuid::from_u128(210))), None)]
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
    let question = RowKey::Card(CardKey::Question(Uuid::from_u128(40)));
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
    assert_eq!(row_index(&rows, Some(&c), item(2)), Some(0));
    assert_eq!(row_index(&rows, Some(&c), item(5)), Some(2));
    assert_eq!(row_index(&rows, Some(&c), item(6)), Some(3));
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
    assert_eq!(
        row_index(&rows, Some(&c), item(5)),
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
    let c = routine_conversation();
    let rows: Vec<_> = [
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
    assert_eq!(row_index(&rows, Some(&c), item(3)), Some(0));
    assert_eq!(row_index(&rows, Some(&c), item(1)), Some(1));
    assert_eq!(row_index(&rows, Some(&c), item(2)), Some(1));
    assert_eq!(row_index(&rows, Some(&c), item(999)), None);
}

#[test]
fn routine_card_contains_messages_and_requests_once() {
    let c = routine_conversation();
    let rows = [
        item(1),
        item(2),
        RowKey::Card(CardKey::Question(Uuid::from_u128(210))),
        RowKey::Card(CardKey::SecretRequest(Uuid::from_u128(310))),
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
        RowKey::Card(CardKey::Question(Uuid::from_u128(210))),
        RowKey::Task(Uuid::from_u128(10)),
        RowKey::Card(CardKey::SecretRequest(Uuid::from_u128(310))),
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

fn instant(text: &str) -> DateTime<Utc> {
    text.parse().unwrap()
}

fn built(c: &Conversation, pending: &[PendingSend], full_replies: &[u128]) -> Transcript {
    let full_replies = full_replies.iter().copied().map(Uuid::from_u128).collect();
    build(
        Some(c),
        pending,
        &Folds {
            quiet: &HashSet::new(),
            full_replies: &full_replies,
        },
        None,
        &Utc,
    )
}

#[test]
fn build_lays_out_messages_and_unsent_ones_without_a_window() {
    let message = |seq: i64, turn: u128, kind: &str, at: &str| {
        json!({
            "id": Uuid::from_u128(seq as u128), "seq": seq, "turn_id": Uuid::from_u128(turn),
            "kind": kind, "text": format!("Message {seq}"), "created_at": at
        })
    };
    let turn = |id: u128| {
        json!({"id": Uuid::from_u128(id), "request_id": Uuid::from_u128(id + 100),
               "turn_no": id, "provider_id": "codex", "status": "completed",
               "effect_uncertain": false})
    };
    let c = Conversation::from_snapshot(serde_json::from_value(json!({
        "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
        "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
        "items": [
            message(1, 10, "user", "2026-11-04T23:58:00Z"),
            message(2, 10, "assistant", "2026-11-04T23:59:00Z"),
            message(3, 11, "user", "2026-11-05T09:00:00Z"),
        ],
        "turns": [turn(10)],
        "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
    })).unwrap()).unwrap();
    let mut unsent = PendingSend::new("Thanks".into(), Vec::new(), None, None, None);
    unsent.sent_at = instant("2026-11-05T09:01:00Z");
    let unsent_key = RowKey::Pending(unsent.request_id);

    let transcript = built(&c, &[unsent], &[]);

    assert_eq!(
        transcript
            .rows
            .iter()
            .map(|row| (row.key, row.footer_stamp, row.day, row.starts_run))
            .collect::<Vec<_>>(),
        [
            (item(1), None, Some(instant("2026-11-04T23:58:00Z")), true),
            (item(2), Some(instant("2026-11-04T23:59:00Z")), None, true),
            (item(3), None, Some(instant("2026-11-05T09:00:00Z")), true),
            (
                unsent_key,
                Some(instant("2026-11-05T09:01:00Z")),
                None,
                true
            ),
        ]
    );
    assert_eq!(
        transcript
            .texts
            .iter()
            .map(|(key, source)| (*key, source.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (item(1), "Message 1"),
            (item(2), "Message 2"),
            (item(3), "Message 3"),
            (unsent_key, "Thanks"),
        ]
    );
    assert!(!transcript.ticking);
}

#[test]
fn a_mail_turn_that_failed_without_a_reply_stays_above_later_messages() {
    let message = |seq: i64, turn: u128, kind: &str, at: &str| {
        json!({
            "id": Uuid::from_u128(seq as u128), "seq": seq, "turn_id": Uuid::from_u128(turn),
            "kind": kind, "text": format!("Message {seq}"), "created_at": at
        })
    };
    let c = Conversation::from_snapshot(serde_json::from_value(json!({
        "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
        "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
        "items": [
            message(1, 9, "user", "2026-11-19T10:00:00Z"),
            message(2, 9, "assistant", "2026-11-19T10:01:00Z"),
            message(4, 11, "user", "2026-11-23T09:00:00Z"),
            message(5, 11, "assistant", "2026-11-23T09:01:00Z"),
        ],
        "turns": [
            {"id": Uuid::from_u128(9), "request_id": Uuid::from_u128(109), "turn_no": 9,
             "provider_id": "codex", "status": "completed", "effect_uncertain": false},
            {"id": Uuid::from_u128(10), "request_id": Uuid::from_u128(110), "turn_no": 10,
             "provider_id": "codex", "status": "failed", "effect_uncertain": false,
             "error": {"code": "provider_failed", "message": "codex debug models failed"},
             "started_at": "2026-11-20T11:35:00Z", "finished_at": "2026-11-20T11:36:00Z",
             "mail": [{"account_id": Uuid::from_u128(50),
                       "message": {"folder": "INBOX", "uid_validity": 1, "uid": 7}}]},
            {"id": Uuid::from_u128(11), "request_id": Uuid::from_u128(111), "turn_no": 11,
             "provider_id": "codex", "status": "completed", "effect_uncertain": false},
        ],
        "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
    })).unwrap()).unwrap();

    let transcript = built(&c, &[], &[]);

    assert_eq!(
        transcript
            .rows
            .iter()
            .map(|row| (row.key, row.day))
            .collect::<Vec<_>>(),
        [
            (item(1), Some(instant("2026-11-19T10:00:00Z"))),
            (item(2), None),
            (
                RowKey::Task(Uuid::from_u128(10)),
                Some(instant("2026-11-20T11:36:00Z"))
            ),
            (item(4), Some(instant("2026-11-23T09:00:00Z"))),
            (item(5), None),
        ]
    );
}

#[test]
fn a_mail_turn_that_failed_after_a_later_message_sits_at_its_failure_time() {
    let message = |seq: i64, turn: u128, kind: &str, at: &str| {
        json!({
            "id": Uuid::from_u128(seq as u128), "seq": seq, "turn_id": Uuid::from_u128(turn),
            "kind": kind, "text": format!("Message {seq}"), "created_at": at
        })
    };
    let c = Conversation::from_snapshot(serde_json::from_value(json!({
        "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
        "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
        "items": [
            message(2, 11, "user", "2026-11-20T23:59:00Z"),
            message(3, 11, "assistant", "2026-11-21T00:05:00Z"),
        ],
        "turns": [
            {"id": Uuid::from_u128(10), "request_id": Uuid::from_u128(110), "turn_no": 10,
             "provider_id": "codex", "status": "failed", "effect_uncertain": false,
             "started_at": "2026-11-20T23:57:00Z", "finished_at": "2026-11-21T00:02:00Z",
             "mail": [{"account_id": Uuid::from_u128(50),
                       "message": {"folder": "INBOX", "uid_validity": 1, "uid": 7}}]},
            {"id": Uuid::from_u128(11), "request_id": Uuid::from_u128(111), "turn_no": 11,
             "provider_id": "codex", "status": "completed", "effect_uncertain": false},
        ],
        "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
    })).unwrap()).unwrap();

    let transcript = built(&c, &[], &[]);

    assert_eq!(
        transcript
            .rows
            .iter()
            .map(|row| (row.key, row.day))
            .collect::<Vec<_>>(),
        [
            (item(2), Some(instant("2026-11-20T23:59:00Z"))),
            (
                RowKey::Task(Uuid::from_u128(10)),
                Some(instant("2026-11-21T00:02:00Z"))
            ),
            (item(3), None),
        ]
    );
}

#[test]
fn build_shows_an_exchange_reply_as_an_excerpt_until_opened_in_full() {
    let full = "A long answer that keeps going. ".repeat(40);
    let c = Conversation::from_snapshot(serde_json::from_value(json!({
        "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
        "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
        "items": [{"id": Uuid::from_u128(1), "seq": 1, "turn_id": Uuid::from_u128(10),
                   "kind": "status", "text": full,
                   "bot_reply": {"sender_bot_id": Uuid::from_u128(90), "sender_name": "Dev",
                                 "prompt": "Summarize", "status": "completed"}}],
        "turns": [],
        "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
    })).unwrap()).unwrap();
    let text = |transcript: Transcript| transcript.texts[0].1.text.clone();

    let excerpt = text(built(&c, &[], &[]));
    assert!(
        excerpt.len() < full.len(),
        "folded reply shows an excerpt: {excerpt:?}"
    );
    assert_eq!(text(built(&c, &[], &[1])), full);
}

#[test]
fn a_card_row_changes_state_when_its_card_settles() {
    let question = RowKey::Card(CardKey::Question(Uuid::from_u128(210)));
    let state = |status: &str| {
        let c =
            Conversation::from_snapshot(question_page(status, 1, "2026-11-01T00:00:00Z")).unwrap();
        built(&c, &[], &[])
            .rows
            .into_iter()
            .find(|row| row.key == question)
            .and_then(|row| row.state)
    };
    assert_eq!(
        state("pending"),
        Some(RowState::Card(CardState::Question(QuestionStatus::Pending)))
    );
    assert_eq!(
        state("answered"),
        Some(RowState::Card(CardState::Question(
            QuestionStatus::Answered
        )))
    );
}

#[test]
fn build_ticks_while_a_task_runs() {
    let c = mail_turn(false, "running", &[2], &[3]);
    assert!(built(&c, &[], &[]).ticking);
    let c = mail_turn(false, "completed", &[2], &[3]);
    assert!(!built(&c, &[], &[]).ticking);
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
    rows.insert(
        2,
        transcript_row(RowKey::Card(CardKey::Question(Uuid::from_u128(9)))),
    );
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
