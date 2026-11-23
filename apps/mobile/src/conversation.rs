//! What the phone shows of a bot's conversation: the transcript rows, the
//! asks waiting on the owner, and whether the bot is replying. The
//! conversation itself, its pages and the live events applied on top, is
//! the shared `else_client::model::Conversation` the desktop uses too.

use else_client::model::Conversation;
use else_wire::bots::Phase;
use else_wire::email::TurnMail;
use else_wire::items::{Attachment, BotReplyStatus, Item, ItemKind, MessagePart, ReactionKind};
use else_wire::notices::{OwnerNotice, OwnerNoticeStatus};
use else_wire::outgoing::{DraftStatus, OutgoingDraft};
use else_wire::questions::{Question, QuestionStatus};
use else_wire::secrets::{SecretRequest, SecretRequestStatus};
use else_wire::skills::{SkillInstall, SkillInstallStatus, SkillScope};
use else_wire::socials::{SetupStatus, SocialSetup};
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
        /// The owner's latest reaction to it.
        reaction: Option<ReactionKind>,
    },
    /// A receipt or notice, centered and quiet.
    Note { id: Uuid, text: String },
    /// Work handed between bots: a task another bot sent this one, or the
    /// answer to a task this one sent.
    Exchange(Box<Exchange>),
    /// Email that woke the bot, before what it did with it.
    Mail { turn: Uuid, mails: Vec<TurnMail> },
}

/// One side of a task between bots, as a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchange {
    pub id: Uuid,
    /// The other bot: who sent the task, or who answered it.
    pub peer: Uuid,
    pub peer_name: String,
    /// The task's short name, when the sender gave one.
    pub title: Option<String>,
    pub prompt: String,
    pub kind: ExchangeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExchangeKind {
    /// A task another bot sent this one.
    Received,
    /// The answer to a task this bot sent.
    Answered {
        status: BotReplyStatus,
        answer: String,
        /// Why a failed task failed.
        reason: Option<String>,
    },
}

/// Something the bot waits on the owner to settle, shown as a card.
#[derive(Debug, Clone, PartialEq)]
pub enum Ask {
    Question(Box<Question>),
    Draft(Box<OutgoingDraft>),
    Secret(Box<SecretRequest>),
    /// The bot is blocked or needs a decision, in its own words.
    Notice(Box<OwnerNotice>),
    /// The bot wants a skill installed.
    Skill(Box<SkillInstall>),
    /// The bot wants a Telegram, Discord or WhatsApp connection.
    Social(Box<SocialSetup>),
}

impl Ask {
    pub fn key(&self) -> String {
        match self {
            Self::Question(question) => format!("q-{}", question.id),
            Self::Draft(draft) => format!("d-{}", draft.id),
            Self::Secret(request) => format!("s-{}", request.id),
            Self::Notice(notice) => format!("n-{}", notice.id),
            Self::Skill(install) => format!("k-{}", install.id),
            Self::Social(setup) => format!("c-{}", setup.id),
        }
    }
}

/// What the owner's message says under it: its turn's status line, as the
/// desktop words it (else_client::turn_status), and a retry when the server
/// can run it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub label: String,
    /// The line reports a failure.
    pub failed: bool,
    pub retry: Option<Retry>,
}

/// Running a failed turn again: the attempt that failed is the request's
/// stable ID, so a repeat queues one new attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    pub turn: Uuid,
    pub attempt: i64,
}

impl Row {
    pub fn key(&self) -> String {
        match self {
            Self::Owner { id, .. } | Self::Bot { id, .. } | Self::Note { id, .. } => id.to_string(),
            Self::Exchange(exchange) => exchange.id.to_string(),
            Self::Mail { turn, .. } => format!("mail-{turn}"),
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
        let first_of_turn = previous != Some(item.turn_id);
        previous = Some(item.turn_id);
        let turn = conversation.turn(item.turn_id);
        // A turn new mail woke opens with that mail.
        if first_of_turn
            && let Some(turn) = turn.filter(|turn| !turn.mail.is_empty())
            && !rows
                .iter()
                .any(|row| matches!(row, Row::Mail { turn: t, .. } if *t == turn.id))
        {
            rows.push(Row::Mail {
                turn: turn.id,
                mails: turn.mail.clone(),
            });
        }
        match item.kind {
            // A reaction shows as its mark on the message it is about.
            ItemKind::User if item.reaction.is_some() => {}
            ItemKind::User => {
                if let Some(task) = &item.bot_task {
                    rows.push(Row::Exchange(Box::new(Exchange {
                        id: item.id,
                        peer: task.sender_bot_id,
                        peer_name: task.sender_name.clone(),
                        title: task.title.clone(),
                        prompt: task.prompt.clone(),
                        kind: ExchangeKind::Received,
                    })));
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
                // The bot's "Thanks, noted" to a reaction says nothing new.
                if item.text.trim().is_empty() || conversation.is_feedback_acknowledgement(item) {
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
                    reaction: conversation.reaction_for(item.id),
                });
            }
            ItemKind::Status | ItemKind::Routine => {
                // A task another bot sent arrives as a status item.
                if let Some(task) = &item.bot_task {
                    rows.push(Row::Exchange(Box::new(Exchange {
                        id: item.id,
                        peer: task.sender_bot_id,
                        peer_name: task.sender_name.clone(),
                        title: task.title.clone(),
                        prompt: task.prompt.clone(),
                        kind: ExchangeKind::Received,
                    })));
                } else if let Some(reply) = &item.bot_reply {
                    rows.push(Row::Exchange(Box::new(Exchange {
                        id: item.id,
                        peer: reply.sender_bot_id,
                        peer_name: reply.sender_name.clone(),
                        title: reply.title.clone(),
                        prompt: reply.prompt.clone(),
                        kind: ExchangeKind::Answered {
                            status: reply.status.clone(),
                            answer: item.text.clone(),
                            reason: reply.reason.clone(),
                        },
                    })));
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
    for install in &turn.skill_installs {
        let text = match install.status {
            SkillInstallStatus::Installed => format!(
                "Installed {} for {} · {}",
                install.name,
                if install.scope == Some(SkillScope::Bot) {
                    "this bot"
                } else {
                    "all bots"
                },
                if install.key_stored {
                    "key saved"
                } else if install.env.is_empty() {
                    "no key needed"
                } else {
                    "key not set"
                }
            ),
            SkillInstallStatus::Declined => format!("You declined {}", install.name),
            SkillInstallStatus::Cancelled => format!("{} was not installed", install.name),
            SkillInstallStatus::Pending | SkillInstallStatus::Unknown => continue,
        };
        rows.push(Row::Note {
            id: install.id,
            text,
        });
    }
    for setup in &turn.social_setups {
        let text = match setup.status {
            SetupStatus::Connected => format!("{} connected", setup.provider.label()),
            SetupStatus::Cancelled => {
                format!("{} connection request cancelled", setup.provider.label())
            }
            SetupStatus::Pending | SetupStatus::Unknown => continue,
        };
        rows.push(Row::Note { id: setup.id, text });
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

/// What waits on the owner, oldest turn first, then what the tasks this
/// bot sent ask of the owner, as their answers carry it.
pub fn asks(conversation: &Conversation) -> Vec<Ask> {
    let mut asks = Vec::new();
    let mut from_tasks: Vec<Ask> = conversation
        .items()
        .filter_map(|item| item.bot_reply.as_ref())
        .flat_map(|reply| &reply.asks)
        .filter(|ask| ask.status == OwnerNoticeStatus::Pending)
        .map(|ask| Ask::Notice(Box::new(OwnerNotice::from(ask))))
        .collect();
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
        asks.extend(
            turn.skill_installs
                .iter()
                .filter(|i| i.status == SkillInstallStatus::Pending)
                .cloned()
                .map(|i| Ask::Skill(Box::new(i))),
        );
        asks.extend(
            turn.social_setups
                .iter()
                .filter(|s| s.is_pending())
                .cloned()
                .map(|s| Ask::Social(Box::new(s))),
        );
    }
    // A notice also on this bot's own turns shows once.
    from_tasks.retain(|ask| !asks.iter().any(|a| a.key() == ask.key()));
    asks.append(&mut from_tasks);
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
    let (label, failed) = else_client::turn_status::label(turn);
    let retry = turn
        .attempt
        .filter(|_| turn.status == TurnStatus::Failed && turn.retryable)
        .map(|attempt| Retry {
            turn: turn.id,
            attempt,
        });
    (!label.is_empty() || retry.is_some()).then_some(Outcome {
        label,
        failed,
        retry,
    })
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
    fn a_pending_skill_install_is_an_ask_until_answered() {
        let mut conversation = conversation();
        let install = |status: &str, scope: serde_json::Value| {
            json!({ "event": "turn.updated", "data": { "turn": {
                "id": TURN, "request_id": REQUEST, "turn_no": 1, "provider_id": "codex",
                "status": "completed", "error": null, "effect_uncertain": false,
                "skill_installs": [{
                    "id": "00000000-0000-0000-0000-00000000000e", "skill_id": "brand",
                    "name": "Brand guidelines", "description": "House style.",
                    "reason": "On brand.", "files": [], "env": ["BRAND_API_KEY"],
                    "source": { "url": "u", "repo": "anthropics/skills",
                                "path": "skills/brand", "commit": "abcdef1" },
                    "secret": "brand", "status": status, "scope": scope,
                    "key_stored": false, "created_at": "2026-11-23T10:00:00Z",
                    "settled_at": null,
                }],
            }}})
        };
        apply(&mut conversation, 5, install("pending", json!(null)));
        assert!(
            matches!(&asks(&conversation)[..], [Ask::Skill(i)] if i.name == "Brand guidelines")
        );
        apply(&mut conversation, 6, install("installed", json!("bot")));
        assert_eq!(asks(&conversation), vec![]);
        assert_eq!(
            rows(&conversation).last(),
            Some(&Row::Note {
                id: "00000000-0000-0000-0000-00000000000e".parse().unwrap(),
                text: "Installed Brand guidelines for this bot · key not set".into(),
            })
        );
    }

    #[test]
    fn a_reaction_shows_on_the_message_it_is_about() {
        let mut conversation = conversation();
        let bot_item = "00000000-0000-0000-0000-000000000002";
        apply(
            &mut conversation,
            5,
            json!({ "event": "item.upsert", "data": { "item": {
                "id": "00000000-0000-0000-0000-000000000010", "seq": 2, "turn_id": TURN,
                "request_id": REQUEST, "kind": "user", "text": "👍 Feedback on …",
                "provider_id": null,
                "reaction": { "item_id": bot_item, "kind": "thumbs_up" },
            }}}),
        );
        let rows = rows(&conversation);
        assert_eq!(rows.len(), 1, "the reaction itself is no row");
        assert!(matches!(
            &rows[0],
            Row::Bot {
                reaction: Some(ReactionKind::ThumbsUp),
                ..
            }
        ));
    }

    #[test]
    fn a_task_answer_is_a_card_and_its_open_ask_waits_here_too() {
        let mut conversation = conversation();
        apply(
            &mut conversation,
            5,
            json!({ "event": "item.upsert", "data": { "item": {
                "id": "00000000-0000-0000-0000-000000000011", "seq": 2, "turn_id": TURN,
                "request_id": null, "kind": "status", "text": "Three spaces found.",
                "provider_id": null,
                "bot_reply": {
                    "sender_bot_id": "00000000-0000-0000-0000-00000000000d",
                    "sender_name": "Scout", "prompt": "Find coworking spaces.",
                    "status": "completed", "title": "Coworking",
                    "asks": [{
                        "id": "00000000-0000-0000-0000-00000000000c",
                        "bot_id": "00000000-0000-0000-0000-00000000000d",
                        "bot_name": "Scout", "turn_id": TURN, "kind": "decision",
                        "summary": "Pick one of the three.", "status": "pending",
                        "created_at": "2026-11-23T10:00:00Z",
                    }],
                },
            }}}),
        );
        let row = rows(&conversation).pop().unwrap();
        let Row::Exchange(exchange) = row else {
            panic!("an exchange row")
        };
        assert_eq!(exchange.peer_name, "Scout");
        assert!(matches!(
            &exchange.kind,
            ExchangeKind::Answered { answer, .. } if answer == "Three spaces found."
        ));
        assert!(
            matches!(&asks(&conversation)[..], [Ask::Notice(n)] if n.summary == "Pick one of the three.")
        );
    }

    #[test]
    fn mail_that_woke_the_bot_comes_before_its_reply_once() {
        let mut conversation = conversation();
        apply(
            &mut conversation,
            5,
            json!({ "event": "turn.updated", "data": { "turn": {
                "id": TURN, "request_id": REQUEST, "turn_no": 1, "provider_id": "codex",
                "status": "completed", "error": null, "effect_uncertain": false,
                "mail": [{
                    "account_id": "00000000-0000-0000-0000-0000000000a1",
                    "message": { "folder": "INBOX", "uid_validity": 1, "uid": 7 },
                    "subject": "Invoice 42", "sender": "Ledger <ap@example.com>",
                }],
            }}}),
        );
        let rows = rows(&conversation);
        assert!(
            matches!(&rows[0], Row::Mail { mails, .. } if mails[0].subject.as_deref() == Some("Invoice 42"))
        );
        assert!(matches!(&rows[1], Row::Bot { .. }));
        assert_eq!(rows.len(), 2);
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
                outcome: Some(Outcome {
                    label: "Failed. Codex is signed out.".into(),
                    failed: true,
                    retry: Some(Retry {
                        turn: TURN.parse().unwrap(),
                        attempt: 2,
                    }),
                }),
            })
        );
    }
}
