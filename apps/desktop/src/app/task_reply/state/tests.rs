use super::*;
use crate::model::{Frame, Snapshot};
use serde_json::{Value, json};

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn turn(number: u128, status: &str) -> Value {
    json!({
        "id": id(number), "request_id": id(number + 100), "turn_no": number,
        "provider_id": "synthetic", "status": status, "error": null,
        "effect_uncertain": false, "created_at": "2026-11-05T10:00:00Z",
        "started_at": "2026-11-05T10:01:00Z", "finished_at": "2026-11-05T10:08:50Z"
    })
}

fn item(number: u128, turn: u128, kind: &str) -> Value {
    json!({"id": id(number), "seq": number, "turn_id": id(turn), "kind": kind, "text": format!("Message {number}")})
}

fn conversation(items: Vec<Value>, turns: Vec<Value>) -> Conversation {
    let snapshot: Snapshot = serde_json::from_value(json!({
        "event_cursor": "test:0", "items": items, "turns": turns,
        "activity": {"phase": "idle", "queued_count": 0}
    }))
    .unwrap();
    Conversation::from_snapshot(snapshot).unwrap()
}

#[test]
fn a_finished_handoff_turn_off_the_page_leaves_no_task_row() {
    for status in ["completed", "interrupted"] {
        let mut off_page = turn(11, status);
        off_page["computer_handoffs"] = json!([{"id": id(300), "reason": "X Chat is blank.",
            "status": "pending", "created_at": "2026-11-05T10:05:00Z"}]);
        let c = conversation(
            vec![item(1, 10, "user"), item(2, 10, "assistant")],
            vec![turn(10, "completed"), off_page],
        );
        assert!(
            !task_replies(&c).contains_key(&id(11)),
            "a {status} handoff turn without loaded items must not add a task row"
        );
        assert_eq!(c.open_handoff().map(|h| h.id), Some(id(300)));
        assert!(crate::app::conversation::request_rows(&c).is_empty());
    }
}

#[test]
fn a_single_completed_reply_is_a_chat_bubble() {
    let mut answer = item(2, 10, "assistant");
    answer["text"] = json!("Got it. I'll wait for you to kick things off.");
    let c = conversation(
        vec![item(1, 10, "user"), answer],
        vec![turn(10, "completed")],
    );
    assert!(
        task_replies(&c).is_empty(),
        "a conversational reply must not become a completed task card"
    );
    assert_eq!(c.latest_reply().unwrap().id, id(2));
}

#[test]
fn a_chat_reply_settles_to_the_same_bubble_after_streaming_or_reload() {
    let items = vec![item(1, 10, "user"), item(2, 10, "assistant")];
    let mut live = conversation(items.clone(), vec![turn(10, "running")]);
    assert_eq!(task_replies(&live)[&id(10)].state, ReplyState::Working);
    live.apply(&Frame {
        id: Some("test:1".into()),
        event: "turn.updated".into(),
        data: json!({"turn": turn(10, "completed")}).to_string(),
    })
    .unwrap();
    let reloaded = conversation(items, vec![turn(10, "completed")]);
    assert!(task_replies(&live).is_empty());
    assert_eq!(task_replies(&live), task_replies(&reloaded));
    assert_eq!(live.latest_reply(), reloaded.latest_reply());
}

#[test]
fn reacting_to_a_chat_reply_does_not_turn_it_into_a_task() {
    let mut feedback = item(3, 11, "user");
    feedback["reaction"] = json!({"item_id": id(2), "kind": "thumbs_up"});
    let c = conversation(
        vec![item(1, 10, "user"), item(2, 10, "assistant"), feedback],
        vec![turn(10, "completed"), turn(11, "queued")],
    );
    assert!(task_replies(&c).is_empty());
    assert!(c.reaction_for(id(2)).is_some());
}

#[test]
fn a_task_split_across_history_pages_keeps_its_card() {
    let mut c = conversation(vec![item(3, 10, "assistant")], vec![turn(10, "completed")]);
    c.older_before_seq = Some(3);
    assert!(task_replies(&c).contains_key(&id(10)));
    c.merge_older(
        serde_json::from_value(json!({
            "event_cursor": "test:0",
            "items": [item(1, 10, "user"), item(2, 10, "assistant")],
            "turns": [turn(10, "completed")],
            "activity": {"phase": "idle", "queued_count": 0}
        }))
        .unwrap(),
    );
    assert_eq!(task_replies(&c)[&id(10)].history, [id(2)]);

    let mut chat = conversation(
        vec![item(4, 11, "user"), item(5, 11, "assistant")],
        vec![turn(11, "completed")],
    );
    chat.older_before_seq = Some(4);
    assert!(task_replies(&chat).is_empty());

    let mut interleaved = conversation(
        vec![
            item(2, 11, "user"),
            item(3, 10, "assistant"),
            item(4, 11, "assistant"),
        ],
        vec![turn(10, "completed"), turn(11, "completed")],
    );
    interleaved.older_before_seq = Some(2);
    let replies = task_replies(&interleaved);
    assert!(replies.contains_key(&id(10)));
    assert!(!replies.contains_key(&id(11)));
}

#[test]
fn completed_tasks_keep_their_card_without_prose_progress_updates() {
    for (field, value) in [
        (
            "routine",
            json!({"routine_id": id(20), "name": "Daily digest", "due_at": "2026-11-05T10:00:00Z"}),
        ),
        (
            "questions",
            json!([{"id": id(21), "kind": "external", "title": "Send it?", "choices": [], "status": "answered"}]),
        ),
        (
            "secret_requests",
            json!([{"id": id(22), "name": "Access", "reason": "Read branches", "env": ["TOKEN"], "status": "provided"}]),
        ),
    ] {
        let mut task = turn(10, "completed");
        task[field] = value;
        let c = conversation(
            vec![item(1, 10, "user"), item(2, 10, "assistant")],
            vec![task],
        );
        assert!(task_replies(&c).contains_key(&id(10)), "{field} stays rich");
    }
    let mut handoff = item(1, 10, "status");
    handoff["bot_reply"] = json!({"sender_bot_id": id(20), "sender_name": "Researcher",
        "prompt": "Find a source", "status": "completed"});
    let c = conversation(
        vec![handoff, item(2, 10, "assistant")],
        vec![turn(10, "completed")],
    );
    assert!(
        task_replies(&c).contains_key(&id(10)),
        "bot_reply stays rich"
    );
}

#[test]
fn a_task_from_another_bot_leaves_its_reply_to_the_exchange_row() {
    let mut receipt = item(1, 10, "status");
    receipt["bot_task"] = json!({"sender_bot_id": id(20), "sender_name": "Proliquid",
        "prompt": "List your routines"});
    for status in ["running", "completed", "failed"] {
        let c = conversation(
            vec![
                receipt.clone(),
                item(2, 10, "assistant"),
                item(3, 11, "user"),
            ],
            vec![turn(10, status), turn(11, "queued")],
        );
        let replies = task_replies(&c);
        assert!(
            !replies.contains_key(&id(10)),
            "a {status} task shows no card"
        );
        assert!(
            replies.contains_key(&id(11)),
            "the owner's own turn keeps its card"
        );
        assert_eq!(c.delegated_turns(), [id(10)].into());
        let answers = c.task_answers(c.item(id(1)).unwrap());
        assert_eq!(answers.iter().map(|a| a.id).collect::<Vec<_>>(), [id(2)]);
    }
}

#[test]
fn routine_updates_fold_into_history_and_the_last_reply_is_the_answer() {
    let c = conversation(
        vec![
            item(1, 10, "user"),
            item(2, 10, "assistant"),
            item(3, 10, "assistant"),
            item(4, 10, "assistant"),
        ],
        vec![turn(10, "completed")],
    );
    let replies = task_replies(&c);
    let reply = &replies[&id(10)];
    assert_eq!(reply.latest, Some(id(4)));
    assert_eq!(reply.anchor, Some(id(4)));
    assert_eq!(reply.history, [id(2), id(3)]);
    assert_eq!(reply.state, ReplyState::Complete);
    assert_eq!(c.items().count(), 4);
}

#[test]
fn a_bot_creation_notice_stands_alone_without_an_empty_task_card() {
    let mut notice = item(1, 10, "status");
    notice["bot_created"] = json!({
        "bot_id": id(20), "name": "Auto", "creator_bot_id": id(21),
        "creator_name": "Tech Manager", "purpose": "Own the routines."
    });
    let c = conversation(vec![notice], vec![turn(10, "completed")]);
    assert!(task_replies(&c).is_empty());
    let c = conversation(vec![item(1, 10, "status")], vec![turn(10, "completed")]);
    assert!(
        task_replies(&c).contains_key(&id(10)),
        "plain notices keep theirs"
    );
}

#[test]
fn a_routine_edit_does_not_move_the_task_capsule_from_its_receipt() {
    let mut c = conversation(vec![item(1, 10, "status")], vec![turn(10, "running")]);
    let before = task_replies(&c);
    let mut notice = item(2, 10, "status");
    notice["routine_update"] = json!({ "routine_id": id(20), "name": "Daily digest" });
    c.apply(&Frame {
        id: Some("test:1".into()),
        event: "item.upsert".into(),
        data: json!({ "item": notice.clone() }).to_string(),
    })
    .unwrap();
    assert_eq!(task_replies(&c), before);
    assert_eq!(before[&id(10)].anchor, Some(id(1)));
    let reloaded = conversation(
        vec![item(1, 10, "status"), notice],
        vec![turn(10, "running")],
    );
    assert_eq!(task_replies(&reloaded), before);
}

#[test]
fn streaming_and_reconnect_choose_the_same_capsule_and_reply() {
    let mut c = conversation(vec![item(1, 10, "user")], vec![turn(10, "running")]);
    let empty = task_replies(&c).remove(&id(10)).unwrap();
    assert_eq!(empty.anchor, Some(id(1)));
    assert_eq!(empty.latest, None);
    for (seq, event, data) in [
        (1, "item.upsert", json!({"item": item(2, 10, "assistant")})),
        (2, "item.upsert", json!({"item": item(3, 10, "assistant")})),
        (3, "turn.updated", json!({"turn": turn(10, "completed")})),
    ] {
        c.apply(&Frame {
            id: Some(format!("test:{seq}")),
            event: event.into(),
            data: data.to_string(),
        })
        .unwrap();
    }
    let reloaded = conversation(
        vec![
            item(1, 10, "user"),
            item(2, 10, "assistant"),
            item(3, 10, "assistant"),
        ],
        vec![turn(10, "completed")],
    );
    assert_eq!(task_replies(&c), task_replies(&reloaded));
}

#[test]
fn interleaved_tasks_and_handoffs_keep_their_own_replies() {
    let c = conversation(
        vec![
            item(1, 10, "user"),
            item(2, 10, "assistant"),
            item(3, 11, "assistant"),
            item(4, 10, "status"),
            item(5, 10, "assistant"),
        ],
        vec![turn(10, "completed"), turn(11, "waiting")],
    );
    let replies = task_replies(&c);
    assert_eq!(replies[&id(10)].history, [id(2)]);
    assert_eq!(replies[&id(10)].latest, Some(id(5)));
    assert_eq!(replies[&id(11)].latest, Some(id(3)));
    assert_eq!(replies[&id(11)].state, ReplyState::Waiting);
    assert_eq!(c.item(id(4)).unwrap().kind, "status");
}

#[test]
fn a_turn_parked_on_a_job_is_working_not_waiting_for_a_bot() {
    let mut mcp = turn(11, "waiting");
    mcp["mcp_waits"] = json!([{ "id": id(30), "mcp_id": "t3-code" }]);
    let mut computer = turn(13, "waiting");
    computer["computer_job_waits"] = json!([{ "id": id(31), "job_id": id(32) }]);
    let c = conversation(
        vec![item(1, 11, "assistant"), item(2, 13, "assistant")],
        vec![mcp, turn(12, "waiting"), computer],
    );
    let replies = task_replies(&c);
    assert_eq!(replies[&id(11)].state, ReplyState::Working);
    assert_eq!(replies[&id(13)].state, ReplyState::Working);
    assert_eq!(replies[&id(12)].state, ReplyState::Waiting);
    let label = |n| crate::app::message_row::turn_label(c.turn(id(n)).unwrap()).0;
    assert_eq!(label(11), "Working");
    assert_eq!(label(13), "Working");
    assert_eq!(label(12), "Waiting for another bot…");
}

#[test]
fn a_turn_waiting_on_bots_counts_each_bot_and_yields_to_the_owner() {
    let task = |n: u128, bot: u128, name: &str| {
        json!({ "id": id(n), "attempt": 0, "target_bot_id": id(bot), "target_name": name,
                "task": "Fix it", "status": "running", "created_at": "2026-11-05T10:02:00Z" })
    };
    let mut parked = turn(11, "waiting");
    parked["delegations"] = json!([task(40, 50, "Dev"), task(41, 51, "QA"), task(42, 52, "Dev")]);
    let mut asking = parked.clone();
    asking["id"] = json!(id(12));
    asking["request_id"] = json!(id(112));
    asking["questions"] = json!([{ "id": id(60), "kind": "choice", "title": "Which repo?",
        "choices": [], "status": "pending" }]);
    let c = conversation(
        vec![item(1, 11, "assistant"), item(2, 12, "assistant")],
        vec![parked, asking],
    );
    let label = |n| crate::app::message_row::turn_label(c.turn(id(n)).unwrap()).0;
    assert_eq!(
        label(11),
        "Waiting for 3 bots…",
        "two bots named Dev are two bots"
    );
    assert!(c.turn(id(11)).unwrap().waits_on_bots());
    assert_eq!(label(12), "Waiting for your answer…");
    assert!(
        !c.turn(id(12)).unwrap().waits_on_bots(),
        "an owner question outranks the carried tasks"
    );
}

#[test]
fn feedback_keeps_an_earlier_update_visible() {
    let mut feedback = item(4, 11, "user");
    feedback["reaction"] = json!({"item_id": id(2), "kind": "thumbs_up"});
    let c = conversation(
        vec![
            item(1, 10, "user"),
            item(2, 10, "assistant"),
            item(3, 10, "assistant"),
            feedback,
        ],
        vec![turn(10, "completed"), turn(11, "queued")],
    );
    let replies = task_replies(&c);
    assert_eq!(replies[&id(10)].kept, [id(2)]);
    assert!(replies[&id(10)].history.is_empty());
}

#[test]
fn reaction_feedback_uses_chat_bubbles_instead_of_task_capsules() {
    for status in ["queued", "running", "completed", "failed"] {
        for kind in ["thumbs_up", "thumbs_down"] {
            let mut feedback = item(4, 11, "user");
            feedback["reaction"] = json!({"item_id": id(2), "kind": kind});
            let c = conversation(
                vec![
                    item(1, 10, "user"),
                    item(2, 10, "assistant"),
                    item(3, 10, "assistant"),
                    feedback,
                    item(5, 11, "assistant"),
                ],
                vec![turn(10, "completed"), turn(11, status)],
            );
            let replies = task_replies(&c);
            assert!(
                replies.contains_key(&id(10)),
                "the original task stays rich"
            );
            assert!(
                !replies.contains_key(&id(11)),
                "{kind} feedback in {status} must not become a task capsule"
            );
        }
    }
}

#[test]
fn feedback_stays_plain_when_its_owner_item_is_on_an_older_page() {
    let mut feedback = turn(11, "completed");
    feedback["reaction"] = json!({"item_id": id(2), "kind": "thumbs_up"});
    let mut ack = item(4, 11, "assistant");
    ack["text"] = json!("Thanks, noted.");
    let c = conversation(vec![ack], vec![feedback]);
    assert!(task_replies(&c).is_empty());
    assert!(c.is_feedback_acknowledgement(c.latest_reply().unwrap()));
}

#[test]
fn reacting_to_a_file_changes_the_task_layout_even_when_already_kept() {
    let mut file = item(2, 10, "assistant");
    file["parts"] = json!([{"type": "file", "file": {
        "id": id(50), "name": "report.txt", "media_type": "text/plain", "size": 12
    }}]);
    let mut c = conversation(vec![item(1, 10, "user"), file], vec![turn(10, "completed")]);
    let before = task_replies(&c);
    assert!(before[&id(10)].latest_kept);
    let mut feedback = item(3, 11, "user");
    feedback["reaction"] = json!({"item_id": id(2), "kind": "thumbs_up"});
    c.apply(&Frame {
        id: Some("test:1".into()),
        event: "item.upsert".into(),
        data: json!({"item": feedback}).to_string(),
    })
    .unwrap();
    assert_ne!(task_replies(&c)[&id(10)], before[&id(10)]);
}

#[test]
fn duration_uses_execution_start_and_freezes_at_the_server_finish() {
    let c = conversation(vec![], vec![turn(10, "completed")]);
    let reply = task_replies(&c).remove(&id(10)).unwrap();
    assert_eq!(
        reply
            .elapsed("2027-01-01T00:00:00Z".parse().unwrap())
            .as_deref(),
        Some("7m 50s")
    );
    let mut legacy = reply.clone();
    legacy.started_at = None;
    assert_eq!(legacy.elapsed(Utc::now()), None);
    legacy.started_at = reply.started_at;
    legacy.finished_at = None;
    assert_eq!(legacy.elapsed(Utc::now()), None);
}

#[test]
fn a_resumed_task_shows_no_duration_once_finished() {
    let now = "2027-01-01T00:00:00Z".parse().unwrap();
    let mut first = turn(10, "completed");
    first["attempt"] = json!(1);
    let c = conversation(vec![], vec![first]);
    let reply = task_replies(&c).remove(&id(10)).unwrap();
    assert_eq!(reply.elapsed(now).as_deref(), Some("7m 50s"));
    let mut resumed = turn(10, "completed");
    resumed["attempt"] = json!(2);
    let c = conversation(vec![], vec![resumed.clone()]);
    assert_eq!(task_replies(&c)[&id(10)].elapsed(now), None);
    resumed["status"] = json!("running");
    let c = conversation(vec![], vec![resumed]);
    assert!(
        task_replies(&c)[&id(10)].elapsed(now).is_some(),
        "a live attempt keeps its running clock"
    );
}

#[test]
fn the_fold_counts_only_what_it_hides_and_says_more_beside_the_rail() {
    let parked = parked_turn("completed", true);
    let (tasks, _) = placed(&parked);
    let beside = &tasks[&id(10)];
    assert_eq!(beside.fold_label(1), "1 more update");
    assert_eq!(beside.fold_label(3), "3 more updates");
    let c = conversation(
        vec![
            item(1, 10, "user"),
            item(2, 10, "assistant"),
            item(3, 10, "assistant"),
        ],
        vec![turn(10, "completed")],
    );
    let alone = &task_replies(&c)[&id(10)];
    assert_eq!(alone.fold_label(1), "1 update");
    assert_eq!(alone.fold_label(2), "2 updates");
}

#[test]
fn stopped_and_failed_tasks_never_claim_completion() {
    for (status, state) in [
        ("interrupted", ReplyState::Stopped),
        ("failed", ReplyState::Failed),
        ("uncertain", ReplyState::Uncertain),
        ("waiting", ReplyState::Waiting),
        ("cancelling", ReplyState::Stopping),
    ] {
        let c = conversation(vec![item(1, 10, "assistant")], vec![turn(10, status)]);
        let reply = task_replies(&c).remove(&id(10)).unwrap();
        assert_eq!(reply.state, state);
        assert_eq!(reply.latest, Some(id(1)));
        assert_ne!(reply.state, ReplyState::Complete);
    }
    let unknown = conversation(
        vec![item(1, 10, "assistant")],
        vec![turn(10, "future_status")],
    );
    assert!(task_replies(&unknown).is_empty());
}

#[test]
fn elapsed_labels_handle_short_tasks_and_hours() {
    assert_eq!(format_elapsed(0), "0s");
    assert_eq!(format_elapsed(59), "59s");
    assert_eq!(format_elapsed(60), "1m 00s");
    assert_eq!(format_elapsed(3600), "1h 00m");
}

#[test]
fn the_static_welcome_is_not_reported_as_completed_work() {
    let mut greeting = turn(1, "completed");
    greeting["started_at"] = Value::Null;
    let c = conversation(vec![item(1, 1, "assistant")], vec![greeting]);
    assert!(task_replies(&c).is_empty());
    assert_eq!(c.latest_reply().unwrap().text, "Message 1");
}

#[test]
fn an_earlier_file_remains_visible_beside_the_final_answer() {
    let mut file = item(2, 10, "assistant");
    file["parts"] = json!([{"type":"file", "file": {
        "id":id(1000), "name":"report.json", "media_type":"application/json", "size":42
    }}]);
    let c = conversation(
        vec![item(1, 10, "user"), file, item(3, 10, "assistant")],
        vec![turn(10, "completed")],
    );
    let reply = task_replies(&c).remove(&id(10)).unwrap();
    assert_eq!(reply.kept, [id(2)]);
    assert_eq!(reply.latest, Some(id(3)));
    assert!(reply.history.is_empty());
}

#[test]
fn queued_cancellation_and_legacy_turns_do_not_invent_work_time() {
    for status in ["queued", "interrupted", "completed"] {
        let mut record = turn(10, status);
        record["started_at"] = Value::Null;
        let c = conversation(vec![item(1, 10, "user")], vec![record]);
        assert_eq!(task_replies(&c)[&id(10)].elapsed(Utc::now()), None);
    }
}

#[test]
fn progress_title_uses_plain_markdown_text() {
    let mut update = item(2, 10, "assistant");
    update["text"] = json!("\n## **Checking** [backup](https://example.invalid) ✓\nMore detail.");
    let c = conversation(vec![item(1, 10, "user"), update], vec![turn(10, "running")]);
    assert_eq!(
        task_replies(&c)[&id(10)].summary.as_deref(),
        Some("Checking backup ✓")
    );
}

#[test]
fn a_file_in_the_current_live_update_is_kept_visible() {
    let mut file = item(2, 10, "assistant");
    file["parts"] = json!([{"type":"file", "file": {
        "id":id(1000), "name":"report.json", "media_type":"application/json", "size":42
    }}]);
    let c = conversation(vec![item(1, 10, "user"), file], vec![turn(10, "running")]);
    assert!(task_replies(&c)[&id(10)].latest_kept);
}

/// #387: a reply that ended an attempt was read as the answer while the
/// turn waited on a delegated task; the continuation must not fold it away.
#[test]
fn a_reply_that_ended_an_attempt_stays_visible_after_the_turn_resumes() {
    let reply = |number, attempt| {
        let mut reply = item(number, 10, "assistant");
        reply["attempt"] = json!(attempt);
        reply
    };
    let mut result = item(4, 10, "status");
    result["bot_reply"] = json!({"sender_bot_id": id(50), "sender_name": "Bento",
        "prompt": "Clean up the routine", "status": "completed"});
    let attempt = |status, attempt| {
        let mut turn = turn(10, status);
        turn["attempt"] = json!(attempt);
        turn
    };
    let items = vec![item(1, 10, "user"), reply(2, 1), reply(3, 1), result];

    let requeued = conversation(items.clone(), vec![attempt("queued", 1)]);
    assert!(
        task_replies(&requeued)[&id(10)].latest_kept,
        "the read reply stays on screen while the result waits for a slot"
    );

    let resumed = conversation(items.clone(), vec![attempt("running", 2)]);
    let working = &task_replies(&resumed)[&id(10)];
    assert_eq!(working.latest, Some(id(3)));
    assert!(
        working.latest_kept,
        "the read reply stays on screen while the bot continues"
    );
    assert_eq!(
        working.history,
        [id(2)],
        "progress inside one attempt still folds"
    );

    let mut done = items;
    done.push(reply(5, 2));
    let c = conversation(done, vec![attempt("completed", 2)]);
    let finished = &task_replies(&c)[&id(10)];
    assert_eq!(finished.latest, Some(id(5)));
    assert_eq!(finished.kept, [id(3)]);
    assert_eq!(finished.history, [id(2)]);
}

/// Shaped like the production turn whose draft the owner discarded: the
/// "ready for your review" reply ended attempt 1, and the discard resumed
/// the turn. The card must end on the discard, not on the stale claim.
#[test]
fn a_reply_from_before_a_park_reads_before_the_reply_that_ended_the_turn() {
    let reply = |number, attempt| {
        let mut reply = item(number, 10, "assistant");
        reply["attempt"] = json!(attempt);
        reply
    };
    let mut finished = turn(10, "completed");
    finished["attempt"] = json!(2);
    let c = conversation(
        vec![
            item(98, 10, "user"),
            reply(99, 1),
            reply(100, 1),
            reply(101, 2),
        ],
        vec![finished],
    );
    let task = &task_replies(&c)[&id(10)];
    assert_eq!(task.latest, Some(id(101)), "the discard reply is current");

    assert_eq!(
        task.earlier(&c, &task.history, false),
        [Earlier::Reply(id(100))],
        "the reply from before the park shows above the current one"
    );
    assert_eq!(
        task.earlier(&c, &task.history, true),
        [Earlier::Reply(id(99)), Earlier::Reply(id(100))],
        "expanded progress joins it in the order it was written"
    );
}

/// #393, shaped like production turn 42: every resumed attempt restated
/// the work and re-attached the same card, once under another name.
#[test]
fn a_turn_that_parks_repeatedly_keeps_one_copy_of_a_restated_reply() {
    let reply = |number: u128, attempt: Option<i64>, file: Option<&str>| {
        let mut reply = item(number, 10, "assistant");
        reply["attempt"] = json!(attempt);
        if let Some(name) = file {
            reply["parts"] = json!([{"type": "file", "file": {
                "id": id(1000 + number), "name": name,
                "media_type": "image/png", "size": 572_211
            }}]);
        }
        reply
    };
    let card = Some("recap-final.png");
    for attempts in [false, true] {
        let at = |attempt| attempts.then_some(attempt);
        let items = vec![
            item(1, 10, "user"),
            reply(2, at(1), None),
            reply(3, at(1), Some("recap.png")),
            reply(4, at(1), card),
            reply(5, at(2), card),
            reply(6, at(2), card),
            reply(7, at(3), card),
            reply(8, at(3), card),
            reply(9, at(4), None),
        ];
        let mut finished = turn(10, "completed");
        finished["attempt"] = json!(4);
        let task = &task_replies(&conversation(items, vec![finished]))[&id(10)];
        assert_eq!(task.latest, Some(id(9)), "attempts={attempts}");
        if attempts {
            assert_eq!(
                task.kept,
                [id(4), id(6), id(8)],
                "each attempt's ending text"
            );
            assert_eq!(task.history, [id(2), id(3), id(5), id(7)]);
            assert_eq!(
                task.repeated_files,
                [id(1004), id(1006)].into(),
                "one copy of the card: the last one"
            );
        } else {
            assert_eq!(task.kept, [id(8)], "one copy of the card");
            assert_eq!(task.history, [id(2), id(3), id(4), id(5), id(6), id(7)]);
            assert!(task.repeated_files.is_empty());
        }
    }
}

/// A re-attached file is not a repeated text: attempt 1's "filed" news stays
/// in view after attempt 2 re-sends its chart with other text, and the chart
/// shows once, on the later reply.
#[test]
fn an_ending_reply_keeps_its_text_when_a_later_reply_resends_its_file() {
    let reply = |number: u128, attempt: i64, file: bool| {
        let mut reply = item(number, 10, "assistant");
        reply["attempt"] = json!(attempt);
        if file {
            reply["parts"] = json!([{"type": "file", "file": {
                "id": id(1000 + number), "name": "chart.png",
                "media_type": "image/png", "size": 4_096
            }}]);
        }
        reply
    };
    let mut finished = turn(10, "completed");
    finished["attempt"] = json!(3);
    let items = vec![
        item(1, 10, "user"),
        reply(2, 1, true),
        reply(3, 2, true),
        reply(4, 3, false),
    ];
    let task = &task_replies(&conversation(items, vec![finished]))[&id(10)];
    assert_eq!(task.kept, [id(2), id(3)]);
    assert_eq!(task.repeated_files, [id(1002)].into());
    assert!(task.history.is_empty());
}

/// A resumed attempt reports only what changed, so with three attempts the
/// reply that ended attempt 1 is still news the owner has not read twice.
#[test]
fn every_earlier_attempts_ending_reply_stays_in_view() {
    let reply = |number: u128, attempt: i64| {
        let mut reply = item(number, 10, "assistant");
        reply["attempt"] = json!(attempt);
        reply
    };
    let mut finished = turn(10, "completed");
    finished["attempt"] = json!(3);
    let items = vec![
        item(1, 10, "user"),
        reply(2, 1),
        reply(3, 1),
        reply(4, 2),
        reply(5, 2),
        reply(6, 3),
    ];
    let task = &task_replies(&conversation(items, vec![finished]))[&id(10)];
    assert_eq!(task.latest, Some(id(6)));
    assert_eq!(task.kept, [id(3), id(5)], "each attempt's last word");
    assert_eq!(task.history, [id(2), id(4)], "progress still folds");
}

#[test]
fn a_distinct_earlier_file_stays_beside_a_later_one() {
    let file = |number: u128, size: u64| {
        let mut reply = item(number, 10, "assistant");
        reply["parts"] = json!([{"type": "file", "file": {
            "id": id(1000 + number), "name": "chart.png", "media_type": "image/png", "size": size
        }}]);
        reply
    };
    let c = conversation(
        vec![
            item(1, 10, "user"),
            file(2, 10),
            file(3, 20),
            item(4, 10, "assistant"),
        ],
        vec![turn(10, "completed")],
    );
    assert_eq!(task_replies(&c)[&id(10)].kept, [id(2), id(3)]);
}

#[test]
fn a_text_file_of_the_same_size_but_another_name_stays_kept() {
    let file = |number: u128, name: &str| {
        let mut reply = item(number, 10, "assistant");
        reply["parts"] = json!([{"type": "file", "file": {
            "id": id(1000 + number), "name": name, "media_type": "application/json", "size": 11
        }}]);
        reply
    };
    let c = conversation(
        vec![
            item(1, 10, "user"),
            file(2, "before.json"),
            file(3, "after.json"),
            item(4, 10, "assistant"),
        ],
        vec![turn(10, "completed")],
    );
    assert_eq!(task_replies(&c)[&id(10)].kept, [id(2), id(3)]);
}

/// The turn from the owner's screenshots: "ready for your review" ended
/// attempt 1 on a draft and a question, the owner settled both at 10:05,
/// and attempt 2 replied at 10:06.
fn parked_turn(status: &str, settled: bool) -> Conversation {
    let reply = |number: u128, attempt, at: &str| {
        let mut reply = item(number, 10, "assistant");
        reply["attempt"] = json!(attempt);
        reply["created_at"] = json!(at);
        reply
    };
    let mut items = vec![
        item(1, 10, "user"),
        reply(2, 1, "2026-11-05T10:02:00Z"),
        reply(3, 1, "2026-11-05T10:03:00Z"),
    ];
    if settled {
        items.push(reply(4, 2, "2026-11-05T10:06:00Z"));
    }
    let at = settled.then_some("2026-11-05T10:05:00Z");
    let mut parked = turn(10, status);
    parked["attempt"] = json!(if settled { 2 } else { 1 });
    parked["outgoing_drafts"] = json!([{"id": id(310), "lineage_id": id(311),
        "revision": 1, "content": {"channel": "email", "from": "main@example.invalid",
        "to": ["edward@example.invalid"], "cc": [], "bcc": [], "subject": "Re: help",
        "body": "Hello"}, "version": 1, "status": if settled { "discarded" } else { "pending" },
        "can_approve": !settled, "result_detail": null,
        "updated_at": "2026-11-05T10:03:00Z", "settled_at": at}]);
    parked["questions"] = json!([{"id": id(210), "kind": "choice",
        "title": "Which proposal should I schedule?",
        "choices": [{"id": "a", "label": "A"}, {"id": "b", "label": "B"}],
        "status": if settled { "answered" } else { "pending" },
        "answer": settled.then_some("b"), "settled_at": at}]);
    conversation(items, vec![parked])
}

type Rows = Vec<(RowKey, Option<Uuid>)>;

fn placed(c: &Conversation) -> (HashMap<Uuid, TaskReply>, Rows) {
    let mut tasks = task_replies(c);
    let rows = place_cards(c, &mut tasks, crate::app::conversation::turn_card_rows(c));
    (tasks, rows)
}

#[test]
fn a_card_settled_in_a_resumed_turn_reads_between_the_earlier_and_current_reply() {
    for status in ["completed", "running"] {
        let c = parked_turn(status, true);
        let (tasks, rows) = placed(&c);
        let task = &tasks[&id(10)];
        assert!(
            rows.is_empty(),
            "{status}: the cards render inside the task, not as rows: {rows:?}"
        );
        assert_eq!(task.latest, Some(id(4)), "{status}");
        assert_eq!(
            task.earlier(&c, &task.history, false),
            [
                Earlier::Reply(id(3)),
                Earlier::Card(RowKey::Question(id(210))),
                Earlier::Card(RowKey::Draft(id(310))),
            ],
            "{status}: each card follows the reply that asked for it, once"
        );
        assert_eq!(
            task.earlier(&c, &task.history, true),
            [
                Earlier::Reply(id(2)),
                Earlier::Reply(id(3)),
                Earlier::Card(RowKey::Question(id(210))),
                Earlier::Card(RowKey::Draft(id(310))),
            ],
            "{status}: expanded progress keeps the cards after the reply they followed"
        );
    }
}

/// Two parks: a question answered after attempt 1, a draft discarded
/// after attempt 2. The reply each card followed stays visible beside it.
#[test]
fn every_card_in_a_task_keeps_the_reply_it_followed_in_view() {
    let reply = |number: u128, attempt, at: &str| {
        let mut reply = item(number, 10, "assistant");
        reply["attempt"] = json!(attempt);
        reply["created_at"] = json!(at);
        reply
    };
    let mut done = turn(10, "completed");
    done["attempt"] = json!(3);
    done["questions"] = json!([{"id": id(210), "kind": "choice",
        "title": "Which proposal should I schedule?",
        "choices": [{"id": "a", "label": "A"}, {"id": "b", "label": "B"}],
        "status": "answered", "answer": "b", "settled_at": "2026-11-05T10:03:00Z"}]);
    done["outgoing_drafts"] = json!([{"id": id(310), "lineage_id": id(311),
        "revision": 1, "content": {"channel": "email", "from": "main@example.invalid",
        "to": ["edward@example.invalid"], "cc": [], "bcc": [], "subject": "Re: help",
        "body": "Hello"}, "version": 1, "status": "discarded", "can_approve": false,
        "result_detail": null, "updated_at": "2026-11-05T10:05:00Z",
        "settled_at": "2026-11-05T10:05:00Z"}]);
    let c = conversation(
        vec![
            item(1, 10, "user"),
            reply(2, 1, "2026-11-05T10:02:00Z"),
            reply(3, 2, "2026-11-05T10:04:00Z"),
            reply(4, 3, "2026-11-05T10:06:00Z"),
        ],
        vec![done],
    );
    let (tasks, rows) = placed(&c);
    let task = &tasks[&id(10)];
    assert!(
        rows.is_empty(),
        "both cards render inside the task: {rows:?}"
    );
    assert_eq!(
        task.earlier(&c, &task.history, false),
        [
            Earlier::Reply(id(2)),
            Earlier::Card(RowKey::Question(id(210))),
            Earlier::Reply(id(3)),
            Earlier::Card(RowKey::Draft(id(310))),
        ],
        "each card follows the reply that asked for it, even collapsed"
    );
    assert!(
        task.history.is_empty(),
        "a reply on view is not counted as hidden"
    );
}

#[test]
fn a_pending_card_keeps_its_row_below_the_reply_asking_for_it() {
    let c = parked_turn("waiting", false);
    let (tasks, rows) = placed(&c);
    let task = &tasks[&id(10)];
    assert_eq!(task.anchor, Some(id(3)));
    assert!(task.cards.is_empty());
    assert_eq!(
        rows,
        [
            (RowKey::Question(id(210)), Some(id(3))),
            (RowKey::Draft(id(310)), Some(id(3))),
        ]
    );
}

#[test]
fn a_card_settled_after_a_single_attempts_last_reply_keeps_its_row() {
    let c = {
        let mut reply = item(3, 10, "assistant");
        reply["created_at"] = json!("2026-11-05T10:03:00Z");
        let mut single = turn(10, "completed");
        single["outgoing_drafts"] = json!([{"id": id(310), "lineage_id": id(311),
            "revision": 1, "content": {"channel": "email", "from": "main@example.invalid",
            "to": ["edward@example.invalid"], "cc": [], "bcc": [], "subject": "Re: help",
            "body": "Hello"}, "version": 1, "status": "sent", "can_approve": false,
            "result_detail": null, "updated_at": "2026-11-05T10:05:00Z",
            "settled_at": "2026-11-05T10:05:00Z"}]);
        conversation(vec![item(1, 10, "user"), reply], vec![single])
    };
    let (tasks, rows) = placed(&c);
    let task = &tasks[&id(10)];
    assert!(task.cards.is_empty());
    assert!(task.earlier(&c, &task.history, true).is_empty());
    assert_eq!(rows, [(RowKey::Draft(id(310)), Some(id(3)))]);
}
