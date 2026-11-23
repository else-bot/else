//! What the phone shows of a bot's conversation: the transcript rows, the
//! asks waiting on the owner, and whether the bot is replying. The
//! conversation itself, its pages and the live events applied on top, is
//! the shared `else_client::model::Conversation` the desktop uses too.

use else_client::model::Conversation;
use else_wire::bots::Phase;
use else_wire::items::{Attachment, Item, ItemKind, MessagePart};
use else_wire::notices::{OwnerNotice, OwnerNoticeStatus};
use else_wire::outgoing::{DraftStatus, OutgoingDraft};
use else_wire::questions::{Question, QuestionStatus};
use else_wire::secrets::{SecretRequest, SecretRequestStatus};
use else_wire::turns::{Turn, TurnStatus};
use uuid::Uuid;

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
    /// The bot is blocked or needs a decision, in its own words.
    Notice(Box<OwnerNotice>),
}

impl Ask {
    pub fn key(&self) -> String {
        match self {
            Self::Question(question) => format!("q-{}", question.id),
            Self::Draft(draft) => format!("d-{}", draft.id),
            Self::Secret(request) => format!("s-{}", request.id),
            Self::Notice(notice) => format!("n-{}", notice.id),
        }
    }
}

/// A reply that did not complete, as the owner's message shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The owner stopped it.
    Stopped,
    Failed {
        reason: String,
        /// Set when the server can run the message again.
        retry: Option<Retry>,
    },
    /// The server cannot tell whether it ran.
    Unknown,
}

/// Running a failed turn again: the attempt that failed is the request's
/// stable ID, so a repeat queues one new attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    pub turn: Uuid,
    pub attempt: i64,
}

impl Outcome {
    pub fn text(&self) -> &str {
        match self {
            Self::Stopped => "Stopped",
            Self::Failed { reason, .. } => reason,
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

/// Whether the bot is replying or has replies queued.
pub fn busy(conversation: &Conversation) -> bool {
    conversation
        .activity
        .as_ref()
        .is_some_and(|activity| !matches!(activity.phase, Phase::Idle | Phase::Unknown))
}

/// The request of the turn the bot works on now, to stop it.
pub fn active_request(conversation: &Conversation) -> Option<Uuid> {
    let turn = conversation.activity.as_ref()?.active_turn_id?;
    conversation.turn(turn).map(|turn| turn.request_id)
}

/// The transcript, oldest first.
pub fn rows(conversation: &Conversation) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut previous: Option<Uuid> = None;
    for item in conversation.items() {
        if previous.is_some_and(|turn| turn != item.turn_id) {
            receipts(conversation, previous, &mut rows);
        }
        previous = Some(item.turn_id);
        let turn = conversation.turn(item.turn_id);
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
                    && conversation
                        .activity
                        .as_ref()
                        .is_some_and(|a| a.active_turn_id == Some(item.turn_id));
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
    receipts(conversation, previous, &mut rows);
    rows
}

/// The settled asks of `turn`, after its messages.
fn receipts(conversation: &Conversation, turn: Option<Uuid>, rows: &mut Vec<Row>) {
    let Some(turn) = turn.and_then(|turn| conversation.turn(turn)) else {
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
    for notice in &turn.owner_notices {
        let label = match notice.status {
            OwnerNoticeStatus::Replied => "You replied",
            OwnerNoticeStatus::Dismissed => "Handled",
            // Pending is a card; a superseded one gave way to another card.
            OwnerNoticeStatus::Pending
            | OwnerNoticeStatus::Superseded
            | OwnerNoticeStatus::Other(_) => continue,
        };
        rows.push(Row::Note {
            id: notice.id,
            text: format!("{label} · {}", notice.summary),
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

/// What waits on the owner, oldest turn first.
pub fn asks(conversation: &Conversation) -> Vec<Ask> {
    let mut asks = Vec::new();
    for turn in conversation.turns_in_order() {
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
        asks.extend(
            turn.owner_notices
                .iter()
                .filter(|n| n.status == OwnerNoticeStatus::Pending)
                .cloned()
                .map(|n| Ask::Notice(Box::new(n))),
        );
    }
    asks
}

/// Whether the newest item is the owner's and the bot has not started
/// writing: shown as a typing indicator.
pub fn thinking(conversation: &Conversation) -> bool {
    if !busy(conversation) {
        return false;
    }
    conversation
        .items()
        .next_back()
        .is_none_or(|item| item.kind != ItemKind::Assistant)
}

fn outcome(turn: &Turn) -> Option<Outcome> {
    match turn.status {
        TurnStatus::Failed => Some(Outcome::Failed {
            reason: turn
                .error
                .as_ref()
                .and_then(|error| error.message.clone())
                .unwrap_or_else(|| "The reply failed".into()),
            retry: turn
                .attempt
                .filter(|_| turn.retryable)
                .map(|attempt| Retry {
                    turn: turn.id,
                    attempt,
                }),
        }),
        TurnStatus::Uncertain => Some(Outcome::Unknown),
        TurnStatus::Interrupted => Some(Outcome::Stopped),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use else_client::model::Frame;
    use else_wire::conversation::Snapshot;
    use serde_json::json;

    const TURN: &str = "00000000-0000-0000-0000-000000000001";
    const REQUEST: &str = "00000000-0000-0000-0000-000000000003";

    fn conversation() -> Conversation {
        let snapshot: Snapshot = serde_json::from_value(json!({
            "selected_provider_id": null,
            "event_cursor": "srv:bot:4",
            "items": [{
                "id": "00000000-0000-0000-0000-000000000002", "seq": 1, "turn_id": TURN,
                "request_id": null, "kind": "assistant", "text": "Hel", "provider_id": "codex",
            }],
            "turns": [{
                "id": TURN, "request_id": REQUEST, "turn_no": 1, "provider_id": "codex",
                "status": "running", "error": null, "effect_uncertain": false,
            }],
            "activity": { "phase": "working", "active_turn_id": TURN, "queued_count": 0 },
            "older_before_seq": null,
        }))
        .unwrap();
        Conversation::from_snapshot(snapshot).unwrap()
    }

    fn apply(conversation: &mut Conversation, seq: i64, message: serde_json::Value) {
        let frame = Frame::decode(Some(format!("srv:bot:{seq}")), &message);
        conversation.apply(&frame).unwrap();
    }

    #[test]
    fn a_pending_question_is_an_ask_until_answered() {
        let mut conversation = conversation();
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
        apply(&mut conversation, 5, question("pending"));
        assert!(matches!(&asks(&conversation)[..], [Ask::Question(q)] if q.title == "Which plan?"));
        let mut answered = question("answered");
        answered["data"]["turn"]["questions"][0]["answer"] = json!("a");
        apply(&mut conversation, 6, answered);
        assert_eq!(asks(&conversation), vec![]);
        assert_eq!(
            rows(&conversation).last(),
            Some(&Row::Note {
                id: "00000000-0000-0000-0000-00000000000a".parse().unwrap(),
                text: "You answered “Which plan?”: A".into(),
            })
        );
    }

    #[test]
    fn a_pending_secret_request_is_an_ask_until_settled() {
        let mut conversation = conversation();
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
        apply(&mut conversation, 5, request("pending"));
        assert!(matches!(&asks(&conversation)[..], [Ask::Secret(r)] if r.name == "smtp"));
        apply(&mut conversation, 6, request("cancelled"));
        assert_eq!(asks(&conversation), vec![]);
        assert_eq!(
            rows(&conversation).last(),
            Some(&Row::Note {
                id: "00000000-0000-0000-0000-00000000000b".parse().unwrap(),
                text: "smtp was not saved".into(),
            })
        );
    }

    #[test]
    fn a_pending_owner_notice_is_an_ask_until_handled() {
        let mut conversation = conversation();
        let notice = |status: &str| {
            json!({ "event": "turn.updated", "data": { "turn": {
                "id": TURN, "request_id": REQUEST, "turn_no": 1, "provider_id": "codex",
                "status": "completed", "error": null, "effect_uncertain": false,
                "owner_notices": [{
                    "id": "00000000-0000-0000-0000-00000000000c",
                    "bot_id": "00000000-0000-0000-0000-00000000000d", "turn_id": TURN,
                    "kind": "blocked", "summary": "The DNS zone is locked.",
                    "tried": "dig", "status": status,
                    "created_at": "2026-11-23T10:00:00Z", "settled_at": null,
                }],
            }}})
        };
        apply(&mut conversation, 5, notice("pending"));
        assert!(
            matches!(&asks(&conversation)[..], [Ask::Notice(n)] if n.summary == "The DNS zone is locked.")
        );
        apply(&mut conversation, 6, notice("dismissed"));
        assert_eq!(asks(&conversation), vec![]);
        assert_eq!(
            rows(&conversation).last(),
            Some(&Row::Note {
                id: "00000000-0000-0000-0000-00000000000c".parse().unwrap(),
                text: "Handled · The DNS zone is locked.".into(),
            })
        );
        apply(&mut conversation, 7, notice("superseded"));
        assert_eq!(asks(&conversation), vec![]);
    }

    #[test]
    fn a_failed_turn_marks_the_owner_message() {
        let mut conversation = conversation();
        apply(
            &mut conversation,
            5,
            json!({ "event": "item.upsert", "data": { "item": {
                "id": "00000000-0000-0000-0000-000000000009", "seq": 2, "turn_id": TURN,
                "request_id": REQUEST, "kind": "user", "text": "hi", "provider_id": null,
            }}}),
        );
        apply(
            &mut conversation,
            6,
            json!({ "event": "turn.updated", "data": { "turn": {
                "id": TURN, "request_id": REQUEST, "turn_no": 1, "provider_id": "codex",
                "status": "failed", "attempt": 2, "retryable": true,
                "error": { "code": "provider_failed", "message": "Codex is signed out." },
                "effect_uncertain": false,
            }}}),
        );
        assert_eq!(
            rows(&conversation).last(),
            Some(&Row::Owner {
                id: "00000000-0000-0000-0000-000000000009".parse().unwrap(),
                text: "hi".into(),
                files: vec![],
                outcome: Some(Outcome::Failed {
                    reason: "Codex is signed out.".into(),
                    retry: Some(Retry {
                        turn: TURN.parse().unwrap(),
                        attempt: 2,
                    }),
                }),
            })
        );
    }
}
