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
    pub reactions: Vec<(Uuid, crate::model::ReactionKind)>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl TaskReply {
    pub(in crate::app) fn elapsed(&self, now: DateTime<Utc>) -> Option<String> {
        let start = self.started_at?;
        let end = if self.state.terminal() {
            self.finished_at?
        } else {
            now
        };
        Some(format_elapsed((end - start).num_seconds().max(0)))
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
    let mut replies: HashMap<_, _> = conversation
        .turns_in_order()
        .into_iter()
        .filter(|turn| conversation.reaction_in(turn.id).is_none())
        .filter_map(|turn| {
            let state = match turn.status.as_str() {
                "queued" => ReplyState::Queued,
                "preparing" | "sending" | "running" => ReplyState::Working,
                "waiting" if !turn.mcp_waits.is_empty() => ReplyState::Working,
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
                    reactions: Vec::new(),
                    started_at: turn.started_at,
                    finished_at: turn.finished_at,
                },
            ))
        })
        .collect();
    let mut seen_turns = HashSet::new();
    for item in conversation.items() {
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
                if reply.latest_kept {
                    reply.kept.push(previous)
                } else {
                    reply.history.push(previous)
                }
            }
            reply.latest_kept = reaction.is_some() || has_file;
            reply.anchor = Some(item.id);
        } else if item.bot_created.is_some() || item.rules_update.is_some() {
            notice_turns.insert(item.turn_id);
        } else if reply.latest.is_none() && item.routine_update.is_none() {
            reply.anchor = Some(item.id);
        }
    }
    replies.retain(|turn, reply| {
        let notice_only = reply.latest.is_none() && notice_turns.contains(turn);
        !notice_only
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

#[cfg(test)]
mod tests {
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
        for (field, value) in [
            (
                "bot_task",
                json!({"sender_bot_id": id(20), "sender_name": "Researcher", "prompt": "Find a source"}),
            ),
            (
                "bot_reply",
                json!({"sender_bot_id": id(20), "sender_name": "Researcher", "prompt": "Find a source", "status": "completed"}),
            ),
        ] {
            let mut handoff = item(1, 10, "status");
            handoff[field] = value;
            let c = conversation(
                vec![handoff, item(2, 10, "assistant")],
                vec![turn(10, "completed")],
            );
            assert!(task_replies(&c).contains_key(&id(10)), "{field} stays rich");
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
    fn a_turn_parked_on_an_mcp_job_is_working_not_waiting_for_a_bot() {
        let mut parked = turn(11, "waiting");
        parked["mcp_waits"] = json!([{ "id": id(30), "mcp_id": "t3-code" }]);
        let c = conversation(
            vec![item(1, 11, "assistant")],
            vec![parked, turn(12, "waiting")],
        );
        let replies = task_replies(&c);
        assert_eq!(replies[&id(11)].state, ReplyState::Working);
        assert_eq!(replies[&id(12)].state, ReplyState::Waiting);
        let label = |n| crate::app::message_row::turn_label(c.turn(id(n)).unwrap()).0;
        assert_eq!(label(11), "Working");
        assert_eq!(label(12), "Waiting for another bot…");
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
        update["text"] =
            json!("\n## **Checking** [backup](https://example.invalid) ✓\nMore detail.");
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
}
