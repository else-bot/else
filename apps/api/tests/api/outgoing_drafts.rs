use crate::common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::TestApp;
use common::recorder::{GATE, GATE_EVERY_ATTEMPT, Harness, harness, tool};
use else_api::app::outgoing::{Content, Delivery, send};
use else_api::connectors::{store, types::*};
use else_api::db::outgoing_drafts::{self, Claim, DraftError, MailSend, Payload};
use serde_json::{Value, json};
use uuid::Uuid;

/// A caller that may send through connected mail only.
const MAIL: else_api::app::outgoing::Channels = else_api::app::outgoing::Channels {
    mail: true,
    social: false,
};

#[derive(Default)]
struct FakeMail {
    commands: std::sync::Mutex<Vec<MailCommand>>,
    connection_lost: std::sync::atomic::AtomicBool,
    fail_at: std::sync::Mutex<Option<SendPhase>>,
    in_sent: std::sync::atomic::AtomicBool,
    search_fails: std::sync::atomic::AtomicBool,
}

impl FakeMail {
    fn sends(&self) -> Vec<Draft> {
        self.commands
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| match c {
                MailCommand::Send { draft } => Some(draft.clone()),
                _ => None,
            })
            .collect()
    }

    fn searches(&self) -> Vec<String> {
        self.commands
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| match c {
                MailCommand::FindSent { message_id } => Some(message_id.clone()),
                _ => None,
            })
            .collect()
    }

    fn fail_at(&self, phase: Option<SendPhase>) {
        *self.fail_at.lock().unwrap() = phase;
    }
}

#[async_trait::async_trait]
impl MailTransport for FakeMail {
    async fn verify(&self, _: &MailConfig, _: &Auth) -> Result<(), MailError> {
        Ok(())
    }
    async fn execute(
        &self,
        _: &MailConfig,
        _: &Auth,
        command: &MailCommand,
    ) -> Result<Value, MailError> {
        self.commands.lock().unwrap().push(command.clone());
        let flag = |f: &std::sync::atomic::AtomicBool| f.load(std::sync::atomic::Ordering::SeqCst);
        if let MailCommand::FindSent { .. } = command {
            if flag(&self.search_fails) {
                return Err(MailError::new(MailErrorCode::MailTimeout));
            }
            return Ok(json!({ "found": flag(&self.in_sent), "folder": "Sent" }));
        }
        if flag(&self.connection_lost) {
            return Err(MailError::new(MailErrorCode::MailTransportFailed));
        }
        if let Some(phase) = *self.fail_at.lock().unwrap() {
            return Err(MailError::new(MailErrorCode::MailTransportFailed)
                .cause("the mail server closed the connection")
                .at(phase));
        }
        Ok(json!({ "sent": true }))
    }
    async fn fetch(&self, _: &MailConfig, _: &Auth, _: &MessageRef) -> Result<Fetched, MailError> {
        Err(MailError::new(MailErrorCode::MessageNotFound))
    }
    async fn reader(&self, _: &MailConfig, _: &Auth) -> Result<Box<dyn MailReader>, MailError> {
        Err(MailError::new(MailErrorCode::MailTransportFailed))
    }
    async fn scan(&self, _: &MailConfig, _: &Auth, _: Option<&Cursor>) -> Result<Scan, MailError> {
        Ok(Scan {
            cursor: Cursor {
                uid_validity: 1,
                last_uid: 0,
            },
            arrivals: vec![],
            more: false,
        })
    }
    async fn wait(
        &self,
        _: &MailConfig,
        _: &Auth,
        _: &Cursor,
        stop: tokio_util::sync::CancellationToken,
    ) -> Result<(), MailError> {
        stop.cancelled().await;
        Ok(())
    }
}

async fn connect(app: &TestApp, bot: &str, address: &str, send: bool) -> Uuid {
    let config = MailConfig {
        provider: Provider::Imap,
        address: address.into(),
        username: address.into(),
        imap: Endpoint {
            host: "mail.example.com".into(),
            port: 993,
            start_tls: false,
        },
        smtp: Endpoint {
            host: "mail.example.com".into(),
            port: 465,
            start_tls: false,
        },
        archive_folder: "Archive".into(),
        trash_folder: "Trash".into(),
        drafts_folder: "Drafts".into(),
        sent_folder: "Sent".into(),
    };
    let account = store::create(
        &app.db.pool,
        &app.state.secret_key,
        &config,
        &Credential::Password {
            password: "private-app-password".into(),
        },
    )
    .await
    .unwrap();
    permit(app, account.id, bot, send).await;
    account.id
}

async fn permit(app: &TestApp, account: Uuid, bot: &str, send: bool) {
    let path = format!("/v1/connectors/email/accounts/{account}/bots/{bot}");
    let saved = if send {
        let all = json!({ "read": true, "write": true, "send": true, "wake": false });
        app.owner("PUT", &path, Some(all)).await
    } else {
        app.owner("DELETE", &path, None).await
    };
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
}

fn connected_reply(account: Uuid) -> Value {
    json!({ "account_id": account, "reply_to": { "folder": "INBOX", "uid_validity": 7, "uid": 42 },
            "to": ["ada@example.com"], "cc": ["bob@example.com"], "subject": "Re: Lunch",
            "body": "Hi Ada,\n\nFriday at noon works.\n\nMain" })
}

async fn send_with_draft(app: &TestApp, bot: &str, text: &str, draft: Value) -> common::Reply {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/turns"),
        Some(json!({ "request_id": Uuid::new_v4(), "text": text, "open_draft": draft })),
    )
    .await
}

async fn attempt(app: &TestApp, turn: Uuid) -> i32 {
    sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

/// Send an owner message that joins an unfinished reply, optionally with a draft open.
async fn joined(
    h: &Harness,
    bot: &str,
    into: Uuid,
    previous: &str,
    text: &str,
    draft: Option<Value>,
) -> (Uuid, String) {
    let app = &h.app;
    let active = status(app, into).await == "running";
    let sent = match draft {
        Some(draft) => send_with_draft(app, bot, text, draft).await,
        None => app.send(bot, Uuid::new_v4(), text, None).await,
    };
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(
        sent.body["turn"]["steered_into"],
        into.to_string(),
        "{}",
        sent.body
    );
    let turn = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    if active {
        h.release(into).await;
    }
    (turn, h.continued(into, previous).await)
}

fn card_of(cards: &[Value], id: Uuid) -> Value {
    cards
        .iter()
        .find(|c| c["id"] == id.to_string())
        .unwrap_or_else(|| panic!("no card {id} in {cards:?}"))
        .clone()
}

fn lunch() -> Value {
    json!({ "from": "main@bots.example", "to": ["ada@example.com"], "cc": ["bob@example.com"],
            "subject": "Lunch on Friday?", "body": "Hi Ada,\n\nLunch on Friday at noon?\n\nMain" })
}

fn ship_post() -> Value {
    json!({ "platform": "x", "body": "We shipped native drafts today." })
}

fn family_message() -> Value {
    json!({ "platform": "telegram", "account": "@main_bot", "destination": "Family group",
            "body": "Dinner at 8 on Friday?" })
}

async fn draft(app: &TestApp, token: &str, args: Value) -> Uuid {
    let name = if args.get("platform").is_some() {
        "draft_message"
    } else {
        "draft_email"
    };
    let drafted = tool(app, token, name, args).await;
    let result = &drafted.result;
    assert_eq!(result["status"], "pending", "{}", drafted.body);
    assert_eq!(result["parked"], true);
    result["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

async fn owner_action(
    app: &TestApp,
    bot: &str,
    draft: Uuid,
    action: &str,
    body: Value,
) -> common::Reply {
    let (method, path) = match action {
        "edit" => ("PUT", format!("/v1/bots/{bot}/outgoing-drafts/{draft}")),
        _ => (
            "POST",
            format!("/v1/bots/{bot}/outgoing-drafts/{draft}/{action}"),
        ),
    };
    app.owner(method, &path, Some(body)).await
}

async fn status(app: &TestApp, turn: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn shown(app: &TestApp, bot: &str, turn: Uuid) -> Vec<Value> {
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .expect("the turn is in the snapshot")["outgoing_drafts"]
        .as_array()
        .unwrap()
        .clone()
}

async fn journaled(app: &TestApp, bot: &str, draft: Uuid) -> Value {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT kind, data FROM bot_events WHERE bot_id = ?1 ORDER BY seq")
            .bind(bot.parse::<Uuid>().unwrap())
            .fetch_all(&app.db.pool)
            .await
            .unwrap();
    rows.iter()
        .filter(|(kind, _)| kind == "turn.updated")
        .filter_map(|(_, data)| serde_json::from_str::<Value>(data).ok())
        .filter_map(|data| {
            data["turn"]["outgoing_drafts"]
                .as_array()?
                .iter()
                .find(|d| d["id"] == draft.to_string())
                .cloned()
        })
        .next_back()
        .expect("the draft was journaled")
}

async fn questions(app: &TestApp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM owner_questions")
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

fn send_args(draft: Uuid) -> Value {
    json!({ "command": "python3 send_mail.py", "effect": "external", "approved_draft": draft })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_approve_button_sends_the_owners_edited_version_once() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let mut bad = lunch();
    bad["subject"] = json!("Hi\r\nBcc: everyone@example.com");
    let refused = tool(app, &token, "draft_email", bad).await;
    assert!(
        refused.error && refused.code() == "INVALID_DRAFT",
        "{}",
        refused.body
    );
    let id = draft(app, &token, lunch()).await;
    let parked = tool(app, &token, "memory_bots", json!({})).await;
    assert_eq!(parked.status, StatusCode::CONFLICT, "the attempt must end");

    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let card = shown(app, &main, turn).await[0].clone();
    assert_eq!(card["id"], id.to_string());
    assert_eq!(card["revision"], 1);
    assert_eq!(card["content"]["channel"], "email");
    assert_eq!(card["content"]["from"], "main@bots.example");
    assert_eq!(card["content"]["to"], json!(["ada@example.com"]));
    assert_eq!(card["content"]["cc"], json!(["bob@example.com"]));
    assert_eq!(card["content"]["bcc"], json!([]));
    assert_eq!(card["content"]["subject"], "Lunch on Friday?");
    assert_eq!(card["status"], "pending");
    assert_eq!(card["version"], 1);
    assert_eq!(card["can_approve"], true);
    assert_eq!(journaled(app, &main, id).await["status"], "pending");

    let edited = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "body": "Hi Ada, lunch Friday?" }),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);
    assert_eq!(edited.body["draft"]["version"], 2);
    let stale = owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);
    assert_eq!(stale.body["error"]["code"], "draft_changed");
    let bad_edit = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 2, "subject": "a\nb" }),
    )
    .await;
    assert_eq!(
        bad_edit.status,
        StatusCode::BAD_REQUEST,
        "{}",
        bad_edit.body
    );

    let approved = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 2, "subject": "Lunch Friday" }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let shown_draft = &approved.body["draft"];
    assert_eq!(shown_draft["status"], "approved", "approval is not a send");
    assert_eq!(shown_draft["version"], 3);
    assert_eq!(shown_draft["approved_version"], 3);
    assert_eq!(shown_draft["approval_source"], "button");
    assert_eq!(approved.body["requeued"], true);
    let replay = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 2, "subject": "Lunch Friday" }),
    )
    .await;
    assert_eq!(
        replay.status,
        StatusCode::OK,
        "a lost response retries its original edits: {}",
        replay.body
    );
    assert_eq!(replay.body["requeued"], false);
    let changed_replay = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 2, "subject": "Something else" }),
    )
    .await;
    assert_eq!(changed_replay.status, StatusCode::CONFLICT);
    let replay = owner_action(app, &main, id, "approve", json!({ "version": 3 })).await;
    assert_eq!(replay.status, StatusCode::OK, "{}", replay.body);
    assert_eq!(replay.body["requeued"], false);
    let late_edit = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 3, "body": "changed" }),
    )
    .await;
    assert_eq!(late_edit.body["error"]["code"], "draft_settled");

    let token = h.continued(turn, &token).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("[else.bot outgoing drafts]"), "{packet}");
    assert!(
        packet.contains("Subject: Lunch Friday\n\nHi Ada, lunch Friday?"),
        "{packet}"
    );
    assert!(
        packet.contains("The owner approved it on its card"),
        "{packet}"
    );

    let local = tool(
        app,
        &token,
        "computer_run",
        json!({ "command": "true", "effect": "local", "approved_draft": id }),
    )
    .await;
    assert!(
        local.error
            && local
                .result
                .as_str()
                .unwrap()
                .contains("only with effect external")
    );
    let claimed = tool(app, &token, "computer_run", send_args(id)).await;
    assert!(
        claimed.error && claimed.code() == "COMPUTER_UNAVAILABLE",
        "the claim passed the gate and reached the computer: {}",
        claimed.body
    );
    assert_eq!(questions(app).await, 0, "no second approval card");
    let again = tool(app, &token, "computer_run", send_args(id)).await;
    assert!(
        again.error && again.code() == "DRAFT_SETTLED",
        "one send per approval"
    );
    let sent = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "sent", "detail": "250 OK id=abc" }),
    )
    .await;
    assert_eq!(sent.result["status"], "sent", "{}", sent.body);
    let replayed = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "sent" }),
    )
    .await;
    assert_eq!(replayed.result["status"], "sent", "{}", replayed.body);
    let contradicted = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "failed" }),
    )
    .await;
    assert!(contradicted.error && contradicted.code() == "DRAFT_SETTLED");

    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "sent");
    assert_eq!(receipt["result_detail"], "250 OK id=abc");
    assert_eq!(receipt["delivery"]["via"], "tool");
    assert_eq!(receipt["delivery"]["tool"], "computer_run");
    assert!(receipt["settled_at"].is_string());
    let card = shown(app, &main, turn).await[0].clone();
    assert_eq!(card["status"], "sent");
    assert_eq!(card["can_approve"], false);

    let generic = tool(
        app,
        &token,
        "computer_run",
        json!({ "command": "python3 send_mail.py", "effect": "external" }),
    )
    .await;
    assert_eq!(generic.result["status"], "pending", "{}", generic.body);
    assert_eq!(
        questions(app).await,
        1,
        "other external calls still need their card"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn discarding_continues_the_turn_and_nothing_can_send_it() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, lunch()).await;
    let early = else_api::db::outgoing_drafts::claim(
        &app.db.pool,
        turn,
        id,
        &else_api::app::outgoing::Delivery::Host,
    )
    .await;
    assert!(
        matches!(
            early,
            Err(else_api::db::outgoing_drafts::DraftError::Settled(
                else_api::app::outgoing::Status::Pending
            ))
        ),
        "a pending draft cannot be sent: {early:?}"
    );
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;

    let discarded = owner_action(app, &main, id, "discard", json!({})).await;
    assert_eq!(discarded.status, StatusCode::OK, "{}", discarded.body);
    assert_eq!(discarded.body["draft"]["status"], "discarded");
    assert_eq!(discarded.body["requeued"], true);
    let replay = owner_action(app, &main, id, "discard", json!({})).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.body["requeued"], false);
    let approve = owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
    assert_eq!(
        approve.body["error"]["code"], "draft_settled",
        "{}",
        approve.body
    );

    let token = h.continued(turn, &token).await;
    let send = tool(app, &token, "computer_run", send_args(id)).await;
    assert!(send.error && send.code() == "DRAFT_SETTLED");
    let report = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "sent" }),
    )
    .await;
    assert!(
        report.error && report.code() == "OTHER_REQUEST",
        "a report cannot invent a send"
    );
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("The owner discarded it on its card"),
        "{packet}"
    );
    assert_eq!(journaled(app, &main, id).await["status"], "discarded");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_joined_message_carries_the_owners_edits_and_a_revision_replaces_the_card() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, lunch()).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;
    let turns_before: i64 = sqlx::query_scalar("SELECT count(*) FROM turns")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let stale = send_with_draft(
        app,
        &main,
        "Shorter please.",
        json!({ "id": id, "version": 2 }),
    )
    .await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);
    assert_eq!(stale.body["error"]["code"], "draft_changed");
    let turns_after: i64 = sqlx::query_scalar("SELECT count(*) FROM turns")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        turns_before, turns_after,
        "a stale draft refuses the whole message"
    );

    let follow_up = "Make it warmer and mention the new place.";
    let (second, token) = joined(
        &h,
        &main,
        first,
        &token,
        follow_up,
        Some(json!({ "id": id, "version": 1, "body": "Hi Ada, lunch Friday? My treat." })),
    )
    .await;
    assert_eq!(status(app, second).await, "queued");
    let old = shown(app, &main, first).await[0].clone();
    assert_eq!(old["status"], "pending");
    assert_eq!(
        old["version"], 2,
        "the owner's edits were saved with the message"
    );
    assert_eq!(old["content"]["body"], "Hi Ada, lunch Friday? My treat.");
    assert_eq!(old["in_reply"], true, "{old}");
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("in_context=\"true\""), "{packet}");
    assert!(
        packet.contains("Hi Ada, lunch Friday? My treat."),
        "{packet}"
    );
    assert!(packet.contains(follow_up), "{packet}");

    let mut revised = lunch();
    revised["body"] = json!("Hi Ada,\n\nLunch Friday at the new ramen place? My treat.\n\nMain");
    revised["replaces"] = json!(id);
    let drafted = tool(app, &token, "draft_email", revised).await.result;
    assert_eq!(drafted["status"], "pending", "{drafted}");
    assert_eq!(drafted["draft"]["revision"], 2);
    let new_id: Uuid = drafted["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(drafted["draft"]["lineage_id"], old["lineage_id"]);

    let cards = shown(app, &main, first).await;
    let old = card_of(&cards, id);
    assert_eq!(old["status"], "superseded");
    assert_eq!(old["superseded_by"], new_id.to_string());
    assert_eq!(card_of(&cards, new_id)["status"], "pending");
    assert!(shown(app, &main, second).await.is_empty());
    assert_eq!(journaled(app, &main, id).await["status"], "superseded");
    let obsolete = send_with_draft(app, &main, "Send it.", json!({ "id": id, "version": 2 })).await;
    assert_eq!(
        obsolete.body["error"]["code"], "draft_settled",
        "{}",
        obsolete.body
    );
    let obsolete = owner_action(app, &main, id, "edit", json!({ "version": 2, "body": "x" })).await;
    assert_eq!(obsolete.body["error"]["code"], "draft_settled");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clear_chat_approval_confirms_exactly_the_open_version() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, lunch()).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;

    let (second, token) = joined(
        &h,
        &main,
        first,
        &token,
        "Looks good, send it.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let early = tool(app, &token, "computer_run", send_args(id)).await;
    assert!(
        early.error && early.code() == "DRAFT_SETTLED",
        "chat words alone send nothing"
    );
    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    let replay = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(replay.result["status"], "approved", "{}", replay.body);

    assert!(
        shown(app, &main, second).await.is_empty(),
        "the card stays with the reply the message joined"
    );
    let card = shown(app, &main, first).await[0].clone();
    assert_eq!(card["status"], "approved");
    assert_eq!(card["approval_source"], "message");
    assert_eq!(card["approved_version"], 1);
    let item: String =
        sqlx::query_scalar("SELECT lower(hex(owner_item_id)) FROM turns WHERE id = ?1")
            .bind(second)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(
        card["approved_item_id"].as_str().unwrap().replace('-', ""),
        item
    );

    let started = tool(app, &token, "start_draft_send", json!({ "draft_id": id })).await;
    let started = started.result;
    assert_eq!(started["status"], "sending", "{started}");
    assert_eq!(started["draft"]["content"]["subject"], "Lunch on Friday?");
    let twice = tool(app, &token, "start_draft_send", json!({ "draft_id": id })).await;
    assert!(twice.error && twice.code() == "DRAFT_SETTLED");
    let uncertain = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "uncertain", "detail": "SMTP timeout after DATA" }),
    )
    .await;
    assert_eq!(
        uncertain.result["status"], "uncertain",
        "{}",
        uncertain.body
    );
    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "uncertain");
    assert_eq!(receipt["delivery"]["via"], "host");
    assert_eq!(questions(app).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_message_sent_with_the_unchanged_draft_can_approve_it() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, lunch()).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;

    let (_, plain) = joined(&h, &main, first, &token, "Send it.", None).await;
    let unbound = tool(
        app,
        &plain,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert!(
        unbound.error && unbound.code() == "NOT_IN_CONTEXT",
        "a message without the draft"
    );

    let (_, token) = joined(
        &h,
        &main,
        first,
        &plain,
        "Send it.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let edited = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "subject": "Lunch?" }),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);
    let changed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert!(
        changed.error && changed.code() == "NOT_IN_CONTEXT",
        "the owner changed it after sending"
    );
    let unknown = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": Uuid::new_v4() }),
    )
    .await;
    assert!(unknown.error && unknown.code() == "NOT_FOUND");
    assert_eq!(journaled(app, &main, id).await["status"], "pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_draft_sent_with_a_message_the_reply_has_not_taken_yet_cannot_be_approved() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, lunch()).await;
    let sent = send_with_draft(app, &main, "Send it.", json!({ "id": id, "version": 1 })).await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["steered_into"], first.to_string());
    assert_eq!(attempt(app, first).await, 1, "the attempt is still running");
    let early = else_api::db::outgoing_drafts::confirm(&app.db.pool, first, id, None).await;
    assert!(
        matches!(
            early,
            Err(else_api::db::outgoing_drafts::DraftError::NotInContext)
        ),
        "the reply has not read the message yet: {early:?}"
    );

    h.release(first).await;
    let token = h.continued(first, &token).await;
    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreported_send_is_uncertain_and_an_unused_approval_fails() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, lunch()).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
    let token = h.continued(turn, &token).await;
    tool(app, &token, "computer_run", send_args(id)).await;
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "uncertain", "{receipt}");
    assert!(
        receipt["result_detail"]
            .as_str()
            .unwrap()
            .contains("before the bot reported")
    );

    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let mut other = lunch();
    other["to"] = json!(["carol@example.com"]);
    let id = draft(app, &token, other).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "failed", "{receipt}");
    assert!(
        receipt["result_detail"]
            .as_str()
            .unwrap()
            .contains("Nothing was sent")
    );

    let sent = send_with_draft(
        app,
        &main,
        &format!("{GATE} Try sending it again."),
        json!({ "id": id, "version": 1 }),
    )
    .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let (retry, token) = h
        .started(sent.body["turn"]["id"].as_str().unwrap().parse().unwrap())
        .await;
    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    let failed = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "failed", "detail": "No SMTP credentials stored." }),
    )
    .await;
    assert_eq!(failed.result["status"], "failed", "{}", failed.body);
    let card = shown(app, &main, retry).await[0].clone();
    assert_eq!(card["status"], "failed");
    assert_eq!(card["result_detail"], "No SMTP credentials stored.");
    let discarded = owner_action(app, &main, id, "discard", json!({})).await;
    assert_eq!(
        discarded.body["draft"]["status"], "discarded",
        "{}",
        discarded.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_x_post_needs_no_destination_and_its_card_approval_sends_it_once() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let emailish = json!({ "platform": "x", "body": "Hi", "subject": "Launch" });
    let refused = tool(app, &token, "draft_message", emailish).await;
    assert!(
        refused.error && refused.code() == "INVALID_DRAFT",
        "a post has no subject"
    );
    let id = draft(app, &token, ship_post()).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let card = shown(app, &main, turn).await[0].clone();
    assert_eq!(
        card["content"],
        json!({ "channel": "message", "platform": "x", "account": null, "destination": null,
                "body": "We shipped native drafts today." }),
        "no addresses, Cc/Bcc, or subject"
    );
    assert_eq!(card["can_approve"], true);

    let subject = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 1, "subject": "Launch" }),
    )
    .await;
    assert_eq!(subject.status, StatusCode::BAD_REQUEST, "{}", subject.body);
    assert_eq!(subject.body["error"]["code"], "invalid_draft");
    assert_eq!(
        shown(app, &main, turn).await[0]["status"],
        "pending",
        "a refused approval approves nothing"
    );
    let edited = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "body": "Native drafts shipped today." }),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);
    let stale = owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
    assert_eq!(stale.body["error"]["code"], "draft_changed");
    let approved = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 2, "body": "Native drafts shipped today!" }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(approved.body["draft"]["approved_version"], 3);
    assert_eq!(
        approved.body["draft"]["content"]["body"],
        "Native drafts shipped today!"
    );
    let replay = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 2, "body": "Native drafts shipped today!" }),
    )
    .await;
    assert_eq!(replay.status, StatusCode::OK, "{}", replay.body);
    assert_eq!(replay.body["requeued"], false);

    let token = h.continued(turn, &token).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("channel=\"message\" platform=\"x\""),
        "{packet}"
    );
    assert!(packet.contains("\nNative drafts shipped today!\n</outgoing_draft>"));
    assert!(!packet.contains("Subject:"), "{packet}");
    assert!(packet.contains("send exactly this X post now"), "{packet}");

    let claimed = tool(app, &token, "computer_run", send_args(id)).await;
    assert!(
        claimed.error && claimed.code() == "COMPUTER_UNAVAILABLE",
        "{}",
        claimed.body
    );
    let again = tool(app, &token, "computer_run", send_args(id)).await;
    assert!(again.error && again.code() == "DRAFT_SETTLED");
    assert_eq!(questions(app).await, 0, "no second approval card");
    let sent = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "sent", "detail": "post id 1890" }),
    )
    .await
    .result;
    assert_eq!(sent["status"], "sent");
    assert_eq!(sent["medium"], "X post");
    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "sent");
    assert_eq!(receipt["result_detail"], "post id 1890");
    assert_eq!(receipt["delivery"]["via"], "tool");
    assert_eq!(receipt["content"]["platform"], "x");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_telegram_message_keeps_its_channel_and_owner_edits_across_revisions() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let mut nowhere = family_message();
    nowhere["destination"] = Value::Null;
    let refused = tool(app, &token, "draft_message", nowhere).await;
    assert!(
        refused.error && refused.code() == "INVALID_DRAFT",
        "a telegram message names its chat"
    );
    let id = draft(app, &token, family_message()).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;
    let card = shown(app, &main, first).await[0].clone();
    assert_eq!(card["content"]["channel"], "message");
    assert_eq!(card["content"]["platform"], "telegram");
    assert_eq!(card["content"]["destination"], "Family group");
    assert_eq!(card["content"]["account"], "@main_bot");
    for key in ["from", "to", "cc", "bcc", "subject"] {
        assert!(card["content"].get(key).is_none(), "{key} in {card}");
    }

    let with_subject = send_with_draft(
        app,
        &main,
        "Make it 9.",
        json!({ "id": id, "version": 1, "subject": "Dinner" }),
    )
    .await;
    assert_eq!(
        with_subject.status,
        StatusCode::BAD_REQUEST,
        "{}",
        with_subject.body
    );
    assert_eq!(with_subject.body["error"]["code"], "invalid_draft");
    assert_eq!(
        status(app, first).await,
        "waiting",
        "the message was refused"
    );

    let (_, token) = joined(
        &h,
        &main,
        first,
        &token,
        "Make it 9 and ask who brings dessert.",
        Some(json!({ "id": id, "version": 1, "body": "Dinner at 9 on Friday?" })),
    )
    .await;
    let old = shown(app, &main, first).await[0].clone();
    assert_eq!(old["version"], 2, "the owner's edits were saved");
    assert_eq!(old["content"]["body"], "Dinner at 9 on Friday?");
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("channel=\"message\" platform=\"telegram\""),
        "{packet}"
    );
    assert!(
        packet.contains("Destination: Family group\n\nDinner at 9 on Friday?"),
        "{packet}"
    );
    assert!(
        packet.contains("call draft_message with replaces"),
        "{packet}"
    );

    let mut as_email = lunch();
    as_email["replaces"] = json!(id);
    let switched = tool(app, &token, "draft_email", as_email).await;
    assert!(
        switched.error && switched.code() == "CHANNEL_CHANGED",
        "a message cannot become an email"
    );
    let mut as_post = ship_post();
    as_post["replaces"] = json!(id);
    let switched = tool(app, &token, "draft_message", as_post).await;
    assert!(
        switched.error && switched.code() == "CHANNEL_CHANGED",
        "a Telegram message cannot become an X post"
    );
    assert_eq!(shown(app, &main, first).await[0]["status"], "pending");

    let mut revised = family_message();
    revised["body"] = json!("Dinner at 9 on Friday? Who brings dessert?");
    revised["replaces"] = json!(id);
    let revised = tool(app, &token, "draft_message", revised).await.result;
    assert_eq!(revised["status"], "pending", "{revised}");
    assert_eq!(revised["draft"]["revision"], 2);
    assert_eq!(revised["draft"]["lineage_id"], old["lineage_id"]);
    assert_eq!(revised["draft"]["content"]["platform"], "telegram");
    let new_id: Uuid = revised["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let cards = shown(app, &main, first).await;
    assert_eq!(card_of(&cards, id)["status"], "superseded");
    assert_eq!(card_of(&cards, new_id)["status"], "pending");
    let obsolete = send_with_draft(app, &main, "Send it.", json!({ "id": id, "version": 2 })).await;
    assert_eq!(obsolete.body["error"]["code"], "draft_settled");

    let moved = sqlx::query(
        "UPDATE outgoing_drafts SET content = json_set(content, '$.destination', 'Work group')
         WHERE id = ?1",
    )
    .bind(new_id)
    .execute(&app.db.pool)
    .await;
    assert!(moved.is_err(), "a revision's destination is fixed");
    let to_email = sqlx::query(
        "UPDATE outgoing_drafts SET content = json_set(content, '$.channel', 'email')
         WHERE id = ?1",
    )
    .bind(new_id)
    .execute(&app.db.pool)
    .await;
    assert!(to_email.is_err(), "a revision's channel is fixed");

    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;
    let (_, token) = joined(
        &h,
        &main,
        first,
        &token,
        "Perfect, send it.",
        Some(json!({ "id": new_id, "version": 1 })),
    )
    .await;
    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": new_id }),
    )
    .await
    .result;
    assert_eq!(confirmed["status"], "approved", "{confirmed}");
    assert!(
        confirmed["next"]
            .as_str()
            .unwrap()
            .contains("Telegram message")
    );
    let approved = card_of(&shown(app, &main, first).await, new_id);
    assert_eq!(approved["approval_source"], "message");
    assert_eq!(approved["approved_version"], 1);
    let started = tool(
        app,
        &token,
        "start_draft_send",
        json!({ "draft_id": new_id }),
    )
    .await
    .result;
    assert_eq!(started["status"], "sending", "{started}");
    assert_eq!(
        started["draft"]["content"]["body"],
        "Dinner at 9 on Friday? Who brings dessert?"
    );
    let uncertain = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": new_id, "status": "uncertain", "detail": "Bot API timed out" }),
    )
    .await
    .result;
    assert_eq!(uncertain["status"], "uncertain");
    assert_eq!(uncertain["medium"], "Telegram message");
    let receipt = journaled(app, &main, new_id).await;
    assert_eq!(receipt["status"], "uncertain");
    assert_eq!(receipt["delivery"]["via"], "host");
    assert_eq!(receipt["content"]["destination"], "Family group");
    assert_eq!(questions(app).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_approval_binds_the_open_message_version_only() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, family_message()).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;

    let (_, token) = joined(
        &h,
        &main,
        first,
        &token,
        "Send it.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let edited = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "body": "Dinner at 7?" }),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);
    let changed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert!(
        changed.error && changed.code() == "NOT_IN_CONTEXT",
        "the owner changed it after sending"
    );
    let send = tool(app, &token, "computer_run", send_args(id)).await;
    assert!(send.error && send.code() == "DRAFT_SETTLED");
    assert_eq!(journaled(app, &main, id).await["status"], "pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connected_email_sends_the_owners_approved_edit_once_through_its_account() {
    let mail = Arc::new(FakeMail::default());
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let unpermitted = connect(app, &main, "other@owner.example", false).await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;

    let direct = tool(
        app,
        &token,
        "mail_action",
        json!({ "account_id": account, "action_id": Uuid::new_v4(),
                "command": { "operation": "send", "draft": {
                    "to": ["ada@example.com"], "subject": "Hi", "body": "Unapproved" } } }),
    )
    .await;
    assert!(direct.error);
    assert_eq!(direct.result, "mail_send_needs_approved_draft");
    let reply = tool(
        app,
        &token,
        "mail_action",
        json!({ "account_id": account, "action_id": Uuid::new_v4(),
                "command": { "operation": "reply", "body": "Unapproved", "send": true,
                             "message": { "folder": "INBOX", "uid_validity": 7, "uid": 42 } } }),
    )
    .await;
    assert!(reply.error);
    assert_eq!(reply.result, "mail_send_needs_approved_draft");
    assert!(
        mail.commands.lock().unwrap().is_empty(),
        "nothing reached the server"
    );
    let forbidden = tool(app, &token, "draft_email", connected_reply(unpermitted)).await;
    assert!(
        forbidden.error && forbidden.code() == "MAIL_FORBIDDEN",
        "{}",
        forbidden.body
    );
    let mut spoofed = connected_reply(account);
    spoofed["from"] = json!("ceo@owner.example");
    let refused = tool(app, &token, "draft_email", spoofed).await;
    assert!(
        refused.error && refused.code() == "INVALID_DRAFT",
        "{}",
        refused.body
    );

    let paused = connect(app, &main, "paused@owner.example", true).await;
    let paused_response = app
        .owner(
            "PATCH",
            &format!("/v1/connectors/email/accounts/{paused}"),
            Some(json!({"active":false})),
        )
        .await;
    assert_eq!(paused_response.status, StatusCode::OK);
    let refused = tool(app, &token, "draft_email", connected_reply(paused)).await;
    assert!(
        refused.error && refused.code() == "MAIL_FORBIDDEN",
        "{}",
        refused.body
    );

    let folder = "Projects/\"urgent\"/INBOX";
    let mut args = connected_reply(account);
    args["reply_to"]["folder"] = json!(folder);
    let id = draft(app, &token, args).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let card = shown(app, &main, turn).await[0].clone();
    assert_eq!(
        card["content"]["from"], "main@owner.example",
        "the account's own address"
    );
    assert_eq!(card["content"]["account_id"], account.to_string());
    assert_eq!(card["content"]["reply_to"]["uid"], 42);
    assert!(!card.to_string().contains("private-app-password"));

    let approved = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 1, "subject": "Re: Lunch Friday", "body": "Friday at noon. Main" }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let token = h.continued(turn, &token).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains(&format!("account_id=\"{account}\"")),
        "{packet}"
    );
    assert!(packet.contains("send it now with send_draft"), "{packet}");
    assert!(
        !packet.contains(" reply_to=\""),
        "the folder must not break tag attributes: {packet}"
    );
    let reference = packet
        .lines()
        .find_map(|line| line.strip_prefix("Reply to message: "))
        .unwrap();
    let reference: Value = serde_json::from_str(reference).unwrap();
    assert_eq!(reference["folder"], folder);

    for (name, args) in [
        ("computer_run", send_args(id)),
        ("start_draft_send", json!({ "draft_id": id })),
    ] {
        let other = tool(app, &token, name, args).await;
        assert!(
            other.error && other.code() == "CONNECTED_ACCOUNT",
            "{name}: {}",
            other.body
        );
    }
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(sent.result["status"], "sent", "{}", sent.body);
    let again = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(again.result["status"], "sent", "a repeat reads the receipt");
    let contradicted = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "failed" }),
    )
    .await;
    assert!(contradicted.error && contradicted.code() == "DRAFT_SETTLED");

    let sends = mail.sends();
    assert_eq!(sends.len(), 1, "one transport send");
    assert_eq!(sends[0].to, ["ada@example.com"]);
    assert_eq!(sends[0].cc, ["bob@example.com"]);
    assert_eq!(sends[0].subject, "Re: Lunch Friday");
    assert_eq!(sends[0].body, "Friday at noon. Main");
    assert_eq!(sends[0].reply_to.as_ref().unwrap().folder, folder);
    assert_eq!(
        sends[0].reply_to.as_ref().map(|m| (m.uid_validity, m.uid)),
        Some((7, 42))
    );
    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "sent");
    assert_eq!(receipt["delivery"]["via"], "mail");
    assert_eq!(receipt["delivery"]["account_id"], account.to_string());
    let (status, operation, action_turn, action_id): (String, String, Uuid, Uuid) = sqlx::query_as(
        "SELECT status, operation, turn_id, action_id FROM connector_actions
             WHERE account_id = ?1",
    )
    .bind(account)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), operation.as_str(), action_turn),
        ("succeeded", "send", turn)
    );
    assert_eq!(
        receipt["delivery"]["action_id"],
        action_id.to_string(),
        "the card names the mail action receipt"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn uncertain_revoked_and_revised_connected_emails_never_send_twice() {
    let mail = Arc::new(FakeMail::default());
    mail.connection_lost
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, connected_reply(account)).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;
    owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
    let token = h.continued(first, &token).await;
    let lost = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(lost.result["status"], "uncertain", "{}", lost.body);
    let again = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(again.result["status"], "uncertain");
    assert_eq!(mail.sends().len(), 1, "an uncertain send is never repeated");
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;
    assert_eq!(journaled(app, &main, id).await["status"], "uncertain");

    mail.connection_lost
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let sent = send_with_draft(
        app,
        &main,
        &format!("{GATE} I checked Sent, it is not there. Send it again."),
        json!({ "id": id, "version": 1 }),
    )
    .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let (second, token) = h
        .started(sent.body["turn"]["id"].as_str().unwrap().parse().unwrap())
        .await;
    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    permit(app, account, &main, false).await;
    let revoked = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    let revoked = revoked.result;
    assert_eq!(
        revoked["status"], "uncertain",
        "the earlier send stays uncertain: {revoked}"
    );
    assert!(
        revoked["detail"]
            .as_str()
            .unwrap()
            .contains("was not searched: mail_forbidden. Nothing was sent again"),
        "{revoked}"
    );
    assert_eq!(mail.sends().len(), 1, "a revoked account sends nothing");
    h.release(second).await;
    app.wait_turn(second, &["completed"]).await;

    permit(app, account, &main, true).await;
    let sent = send_with_draft(
        app,
        &main,
        &format!("{GATE} Make it shorter."),
        json!({ "id": id, "version": 1 }),
    )
    .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let (_, token) = h
        .started(sent.body["turn"]["id"].as_str().unwrap().parse().unwrap())
        .await;
    let mut unbound = connected_reply(account);
    unbound["account_id"] = Value::Null;
    unbound["reply_to"] = Value::Null;
    unbound["from"] = json!("main@owner.example");
    unbound["replaces"] = json!(id);
    let changed = tool(app, &token, "draft_email", unbound).await;
    assert!(
        changed.error && changed.code() == "CHANNEL_CHANGED",
        "{}",
        changed.body
    );
    let mut revision = connected_reply(account);
    revision["body"] = json!("Friday works.");
    revision["replaces"] = json!(id);
    let revised = tool(app, &token, "draft_email", revision).await;
    assert!(revised.error);
    assert_eq!(
        revised.result, "DRAFT_SETTLED: the draft is uncertain",
        "an email that may have gone out is never revised into a second send"
    );
    assert_eq!(mail.sends().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connected_send_settles_from_its_mail_receipt_even_after_the_request_ends() {
    use else_api::db::outgoing_drafts::{self, Claim, Claimed, Payload};
    let mail = Arc::new(FakeMail::default());
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let bot: Uuid = main.parse().unwrap();
    let account = connect(app, &main, "main@owner.example", true).await;
    let generation = store::account(&app.db.pool, account)
        .await
        .unwrap()
        .unwrap()
        .generation;
    let mut outcomes = Vec::new();
    for dispatched in [false, true] {
        let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
        let mut args = connected_reply(account);
        args["body"] = json!(format!("Dispatched: {dispatched}"));
        let id = draft(app, &token, args).await;
        h.release(turn).await;
        app.wait_turn(turn, &["waiting"]).await;
        owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
        h.continued(turn, &token).await;
        let Claim::Fresh(Claimed {
            action_id,
            payload: Payload::Mail(email),
            ..
        }) = outgoing_drafts::claim_connected(&app.db.pool, turn, id, MAIL)
            .await
            .unwrap()
        else {
            panic!("an approved connected email is claimed for its mail send");
        };
        if dispatched {
            let started = store::begin_action(
                &app.db.pool,
                account,
                generation,
                bot,
                turn,
                action_id,
                &email.send_command(Vec::new(), None),
            )
            .await
            .unwrap();
            assert!(matches!(
                started,
                else_api::connector_core::actions::Start::Dispatch
            ));
        }
        h.release(turn).await;
        app.wait_turn(turn, &["completed"]).await;
        let ended = journaled(app, &main, id).await;
        outcomes.push((id, action_id, ended["status"].as_str().unwrap().to_owned()));
        assert!(ended["result_detail"].is_string(), "{ended}");
    }
    assert_eq!(outcomes[0].2, "failed", "never recorded, so never sent");
    assert_eq!(outcomes[1].2, "uncertain", "dispatched without a receipt");
    let (id, action_id, _) = outcomes[1];
    assert_eq!(
        store::ACTIONS
            .finish(
                &app.db.pool,
                account,
                action_id,
                else_api::connector_core::actions::Outcome::Succeeded,
                json!({}),
            )
            .await
            .unwrap()["status"],
        "succeeded"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM outgoing_drafts WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "sent", "the late receipt settles it");
    assert_eq!(
        journaled(app, &main, id).await["status"],
        "sent",
        "the late receipt reaches the live conversation without a reload"
    );
    assert!(
        mail.sends().is_empty(),
        "the test drove the action receipts itself"
    );
}

/// A task the main bot delegated to another bot while serving an owner
/// message, running: the bot's ID, the root turn, the delegation, and the
/// task's turn.
async fn delegated_task(h: &Harness) -> (String, Uuid, Uuid, Uuid) {
    let app = &h.app;
    let main = app.main_bot().await;
    let leaf = app.create_bot("Telegram").await;
    let warm = h.send(&leaf, "hello").await;
    app.wait_turn(warm, &["completed"]).await;
    let (root, parent) = h.running(&main, GATE).await;
    let delegated = tool(
        app,
        &parent,
        "delegate",
        json!({ "target_bot_id": leaf, "task": GATE_EVERY_ATTEMPT }),
    )
    .await;
    let task: Uuid = delegated.result["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("delegated: {}", delegated.body))
        .parse()
        .unwrap();
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    h.release(root).await;
    (leaf, root, task, child)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_approval_of_a_delegated_draft_answers_the_delegating_request() {
    chat_approval_of_a_delegated_draft(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_approval_before_the_task_parks_still_answers_the_delegating_request() {
    chat_approval_of_a_delegated_draft(false).await;
}

async fn chat_approval_of_a_delegated_draft(parked: bool) {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, root, task, child) = delegated_task(&h).await;
    let (_, token) = h.started(child).await;
    let id = draft(app, &token, lunch()).await;
    if parked {
        h.release(child).await;
        app.wait_turn(child, &["waiting"]).await;
    }

    // Before it parks, the message waits for the task's attempt to end,
    // which then continues at once instead of parking.
    let (reply, token) = joined(
        &h,
        &leaf,
        child,
        &token,
        "Looks good, send it.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let takeover: Option<Uuid> =
        sqlx::query_scalar("SELECT takeover_turn_id FROM delegations WHERE id = ?1")
            .bind(task)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(takeover, None, "the task keeps its own turn");
    assert_eq!(attempt(app, child).await, 2);

    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    h.release(child).await;
    app.wait_turn(child, &["completed"]).await;
    app.wait_turn(reply, &["completed"]).await;
    let outcome: String = sqlx::query_scalar("SELECT status FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(outcome, "completed");
    assert_ne!(
        status(app, root).await,
        "waiting",
        "the delegating request continues"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_continued_reply_ends_the_message_then_the_reply_closes_its_card() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, family_message()).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;

    let (reply, _) = joined(
        &h,
        &main,
        first,
        &token,
        "Offer me a choice of channels instead.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let card = shown(app, &main, first).await[0].clone();
    assert_eq!(card["status"], "pending", "the reply may still confirm it");
    assert_eq!(card["in_reply"], true, "{card}");

    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    app.wait_turn(reply, &["interrupted"]).await;
    app.wait_turn(first, &["waiting"]).await;
    let card = shown(app, &main, first).await[0].clone();
    assert_eq!(
        card["status"], "pending",
        "Stop ends the attempt, not its card"
    );
    assert_eq!(card["in_reply"], false, "{card}");

    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    app.wait_turn(first, &["interrupted"]).await;
    let card = journaled(app, &main, id).await;
    assert_eq!(
        card["status"], "discarded",
        "once the reply and its message have both ended, the card closes: {card}"
    );
    assert_eq!(card["can_approve"], false, "{card}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_that_moves_on_keeps_its_card_for_the_owner() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, family_message()).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;
    let (done, _) = joined(
        &h,
        &main,
        first,
        &token,
        "Make it a poll instead.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;
    assert_eq!(status(app, done).await, "queued", "it ends with its reply");
    let card = shown(app, &main, first).await[0].clone();
    assert_eq!(card["status"], "pending", "{card}");
    assert_eq!(card["can_approve"], true, "the reply still waits on it");

    let discarded = owner_action(app, &main, id, "discard", json!({})).await;
    assert_eq!(discarded.status, StatusCode::OK, "{}", discarded.body);
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;
    app.wait_turn(done, &["completed"]).await;
}

async fn granted(app: &TestApp, bot: &str) -> Value {
    app.owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body["draft_grants"]
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn approving_always_approves_later_drafts_of_that_kind_until_stopped() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let reply = draft(
        app,
        &token,
        json!({ "platform": "x", "account": "@Proliquid_xyz", "body": "Agreed.",
                "destination": "https://x.com/someone/status/1" }),
    )
    .await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    assert_eq!(shown(app, &main, turn).await[0]["kind"], Value::Null);
    let refused = owner_action(
        app,
        &main,
        reply,
        "approve",
        json!({ "version": 1, "always": true }),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], "no_kind");
    assert_eq!(
        shown(app, &main, turn).await[0]["status"],
        "pending",
        "an X reply cannot be approved always, and the refusal approves nothing"
    );
    let discarded = owner_action(app, &main, reply, "discard", json!({})).await;
    assert_eq!(discarded.body["requeued"], true, "{}", discarded.body);

    let token = h.continued(turn, &token).await;
    let post = |body: &str| json!({ "platform": "x", "account": "@Proliquid_xyz", "body": body });
    let first = draft(app, &token, post("A whale flipped long.")).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let card = shown(app, &main, turn).await[1].clone();
    assert_eq!(
        card["kind"],
        json!({ "platform": "x", "account_id": null, "account": "@proliquid_xyz",
                "destination": null })
    );
    assert_eq!(granted(app, &main).await, json!([]));
    let approved = owner_action(
        app,
        &main,
        first,
        "approve",
        json!({ "version": 1, "always": true }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(approved.body["draft"]["status"], "approved");
    assert_eq!(approved.body["draft"]["approval_source"], "button");
    assert_eq!(
        granted(app, &main).await,
        json!([{ "platform": "x", "account_id": null, "account": "@proliquid_xyz",
                 "destination": null }])
    );
    let replay = owner_action(
        app,
        &main,
        first,
        "approve",
        json!({ "version": 1, "always": true }),
    )
    .await;
    assert_eq!(replay.status, StatusCode::OK, "{}", replay.body);
    assert_eq!(replay.body["requeued"], false);

    let token = h.continued(turn, &token).await;
    let drafted = tool(
        app,
        &token,
        "draft_message",
        json!({ "platform": "x", "account": "@PROLIQUID_XYZ", "body": "A whale flipped short." }),
    )
    .await;
    let result = &drafted.result;
    assert_eq!(result["status"], "approved", "{}", drafted.body);
    assert!(result.get("parked").is_none(), "nothing waits on the owner");
    assert!(
        result["next"]
            .as_str()
            .unwrap()
            .contains("approves X post like this always"),
        "{result}"
    );
    let second: Uuid = result["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let card = journaled(app, &main, second).await;
    assert_eq!(card["approval_source"], "standing");
    assert_eq!(card["can_approve"], false);
    let claimed = tool(
        app,
        &token,
        "start_draft_send",
        json!({ "draft_id": second }),
    )
    .await;
    assert_eq!(claimed.result["status"], "sending", "{}", claimed.body);
    let sent = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": second, "status": "sent", "detail": "post id 7" }),
    )
    .await;
    assert_eq!(sent.result["status"], "sent", "{}", sent.body);
    let third = tool(app, &token, "draft_message", post("A third whale.")).await;
    assert_eq!(
        third.result["status"], "approved",
        "an approved-always draft parks nothing, so the request can send more: {}",
        third.body
    );
    let reply = tool(
        app,
        &token,
        "draft_message",
        json!({ "platform": "x", "account": "@proliquid_xyz", "reply_to": "1890",
                "body": "Thanks!" }),
    )
    .await;
    assert_eq!(
        reply.result["status"], "pending",
        "an X reply named only by reply_to is not a post: {}",
        reply.body
    );
    let reply: Uuid = reply.result["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let discarded = owner_action(app, &main, reply, "discard", json!({})).await;
    assert_eq!(discarded.status, StatusCode::OK, "{}", discarded.body);

    let stop = format!("/v1/bots/{main}/outgoing-drafts/{second}/always");
    let stopped = app.owner("DELETE", &stop, None).await;
    assert_eq!(stopped.status, StatusCode::OK, "{}", stopped.body);
    assert_eq!(granted(app, &main).await, json!([]));
    assert_eq!(
        stopped.body["draft"]["status"], "sent",
        "a sent post stays sent"
    );
    let again = app.owner("DELETE", &stop, None).await;
    assert_eq!(
        again.status,
        StatusCode::OK,
        "stopping twice changes nothing"
    );

    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    let (next, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    draft(app, &token, post("Another whale.")).await;
    h.release(next).await;
    app.wait_turn(next, &["waiting"]).await;
    assert_eq!(
        shown(app, &main, next).await[0]["status"],
        "pending",
        "after stopping, the kind waits for approval again"
    );
}

async fn root_source(app: &TestApp, root: Uuid, kind: &str) {
    sqlx::query(
        "UPDATE items SET kind = ?2 WHERE id = (SELECT owner_item_id FROM turns WHERE id = ?1)",
    )
    .bind(root)
    .bind(kind)
    .execute(&app.db.pool)
    .await
    .unwrap();
}

/// Makes `root` a scheduled run of a saved routine. A 'routine' item alone
/// is how a webhook wake starts.
async fn scheduled_run(app: &TestApp, root: Uuid) {
    root_source(app, root, "routine").await;
    let routine = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO routines (id, bot_id, conversation_id, name, prompt, cron, timezone,
             created_turn_id, created_name, request_hash)
         SELECT ?1, bot_id, conversation_id, 'School blog', 'Forward new posts.', '0 8 * * 1-5',
             'UTC', id, 'School blog', zeroblob(32) FROM turns WHERE id = ?2",
    )
    .bind(routine)
    .bind(root)
    .execute(&app.db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO routine_runs (id, routine_id, due_at, name, turn_id)
         VALUES (randomblob(16), ?1, '2030-01-01T08:00:00Z', 'School blog', ?2)",
    )
    .bind(routine)
    .bind(root)
    .execute(&app.db.pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_a_routine_run_delegated_drafts_for_the_owners_approval() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, root, _, child) = delegated_task(&h).await;
    let (_, token) = h.started(child).await;
    scheduled_run(app, root).await;

    let id = draft(app, &token, ship_post()).await;
    h.release(child).await;
    app.wait_turn(child, &["waiting"]).await;
    let card = shown(app, &leaf, child).await[0].clone();
    assert_eq!(
        card["status"], "pending",
        "the owner still approves the post"
    );
    assert_eq!(card["id"], id.to_string(), "{card}");

    let approved = owner_action(app, &leaf, id, "approve", json!({ "version": 1 })).await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    h.continued(child, &token).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("send exactly this X post now"), "{packet}");
    h.release(child).await;
    app.wait_turn(child, &["completed"]).await;
    let delegation: String =
        sqlx::query_scalar("SELECT status FROM delegations WHERE child_turn_id = ?1")
            .bind(child)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(delegation, "completed", "the routine run gets the result");
}

/// Drafts `family_message` from a task delegated by a scheduled routine run
/// or a webhook wake, clicks Approve always on its card, and returns the
/// next draft's result in the same task.
async fn delegated_draft_after_approving_always(scheduled: bool) -> Value {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, root, _, child) = delegated_task(&h).await;
    let (_, token) = h.started(child).await;
    if scheduled {
        scheduled_run(app, root).await;
    } else {
        root_source(app, root, "routine").await;
    }
    let first = draft(app, &token, family_message()).await;
    h.release(child).await;
    app.wait_turn(child, &["waiting"]).await;
    let approved = owner_action(
        app,
        &leaf,
        first,
        "approve",
        json!({ "version": 1, "always": true }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let token = h.continued(child, &token).await;
    let mut next = family_message();
    next["body"] = json!("Pick-up moved to 16:30.");
    let drafted = tool(app, &token, "draft_message", next).await;
    drafted.result
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_a_routine_run_delegated_sends_with_its_bots_approve_always() {
    let result = delegated_draft_after_approving_always(true).await;
    assert_eq!(result["status"], "approved", "{result}");
    assert_eq!(
        result["draft"]["content"]["body"], "Pick-up moved to 16:30.",
        "it sends exactly what the task proposed: {result}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_a_webhook_wake_delegated_still_asks_for_each_message() {
    let result = delegated_draft_after_approving_always(false).await;
    assert_eq!(result["status"], "pending", "{result}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_whatsapp_draft_is_approved_always_for_its_chat_only() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let to = |chat: &str, body: &str| {
        json!({ "platform": "whatsapp", "account": "+33612345678", "destination": chat,
                "body": body })
    };
    let family = "120363041234567890@g.us";
    let unsigned = draft(
        app,
        &token,
        json!({ "platform": "whatsapp", "destination": family, "body": "Bonjour" }),
    )
    .await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    assert_eq!(shown(app, &main, turn).await[0]["kind"], Value::Null);
    let refused = owner_action(
        app,
        &main,
        unsigned,
        "approve",
        json!({ "version": 1, "always": true }),
    )
    .await;
    assert_eq!(
        refused.body["error"]["code"], "no_kind",
        "without its sending account, consent could outlive a switch of account: {}",
        refused.body
    );
    let discarded = owner_action(app, &main, unsigned, "discard", json!({})).await;
    assert_eq!(discarded.status, StatusCode::OK, "{}", discarded.body);
    let token = h.continued(turn, &token).await;
    let first = draft(app, &token, to(family, "Nouvel article sur le blog.")).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let kind = json!({ "platform": "whatsapp", "account_id": null, "account": "+33612345678",
                       "destination": family });
    assert_eq!(
        shown(app, &main, turn).await[1]["kind"],
        kind,
        "a WhatsApp card offers Approve always for its one chat"
    );
    let approved = owner_action(
        app,
        &main,
        first,
        "approve",
        json!({ "version": 1, "always": true }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(granted(app, &main).await, json!([kind]));

    let token = h.continued(turn, &token).await;
    let same = tool(
        app,
        &token,
        "draft_message",
        to(family, "Sortie scolaire vendredi."),
    )
    .await;
    assert_eq!(same.result["status"], "approved", "{}", same.body);
    let other = tool(
        app,
        &token,
        "draft_message",
        to("33698765432@s.whatsapp.net", "Bonjour"),
    )
    .await;
    assert_eq!(
        other.result["status"], "pending",
        "another chat still waits for the owner: {}",
        other.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_reply_saying_always_approves_a_delegated_chat_always() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, _, _, child) = delegated_task(&h).await;
    let (_, token) = h.started(child).await;
    let school = "120363431565310389@g.us";
    let to = |chat: &str, body: &str| {
        json!({ "platform": "whatsapp", "account": "+33612345678", "destination": chat,
                "body": body })
    };
    let first = draft(app, &token, to(school, "Rue aux enfants vendredi.")).await;
    h.release(child).await;
    app.wait_turn(child, &["waiting"]).await;

    let reply = child;
    let (_, token) = joined(
        &h,
        &leaf,
        child,
        &token,
        "Always approve posting messages",
        Some(json!({ "id": first, "version": 1 })),
    )
    .await;
    let unquoted = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": first, "always": "approve everything always" }),
    )
    .await;
    assert!(
        unquoted.error && unquoted.code() == "NOT_QUOTED",
        "only the owner's own words can approve always: {}",
        unquoted.body
    );
    assert_eq!(granted(app, &leaf).await, json!([]));
    assert_eq!(shown(app, &leaf, child).await[0]["status"], "pending");

    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": first, "always": "Always approve posting messages" }),
    )
    .await;
    let result = confirmed.result;
    assert_eq!(result["status"], "approved", "{}", confirmed.body);
    let kind = json!({ "platform": "whatsapp", "account_id": null, "account": "+33612345678",
                       "destination": school });
    assert_eq!(result["always"], kind, "{result}");
    assert_eq!(granted(app, &leaf).await, json!([kind]));
    let card = shown(app, &leaf, reply).await[0].clone();
    assert_eq!(card["approval_source"], "message");
    let replay = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": first, "always": "Always approve posting messages" }),
    )
    .await;
    assert_eq!(replay.result["status"], "approved", "{}", replay.body);

    let same = tool(
        app,
        &token,
        "draft_message",
        to(school, "Invitation de la CAF."),
    )
    .await;
    assert_eq!(
        same.result["status"], "approved",
        "the next post to that chat needs no card: {}",
        same.body
    );
    let same: Uuid = same.result["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let other = tool(
        app,
        &token,
        "draft_message",
        to("33698765432@s.whatsapp.net", "Bonjour"),
    )
    .await;
    assert_eq!(
        other.result["status"], "pending",
        "another chat still waits for the owner: {}",
        other.body
    );
    let other: Uuid = other.result["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    h.release(reply).await;
    app.wait_turn(reply, &["waiting"]).await;
    let discarded = owner_action(app, &leaf, other, "discard", json!({})).await;
    assert_eq!(discarded.status, StatusCode::OK, "{}", discarded.body);
    let token = h.continued(reply, &token).await;

    let stop = format!("/v1/bots/{leaf}/outgoing-drafts/{same}/always");
    let stopped = app.owner("DELETE", &stop, None).await;
    assert_eq!(stopped.status, StatusCode::OK, "{}", stopped.body);
    assert_eq!(granted(app, &leaf).await, json!([]));
    let after = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": first, "always": "Always approve posting messages" }),
    )
    .await;
    assert!(
        after.error && after.code() == "NOT_ALWAYS",
        "a retry never brings back what the owner stopped: {}",
        after.body
    );
    let claimed = tool(
        app,
        &token,
        "start_draft_send",
        json!({ "draft_id": first }),
    )
    .await;
    assert_eq!(claimed.result["status"], "sending", "{}", claimed.body);
    let failed = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": first, "status": "failed", "detail": "offline" }),
    )
    .await;
    assert_eq!(failed.result["status"], "failed", "{}", failed.body);
    let retried = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": first, "always": "Always approve posting messages" }),
    )
    .await;
    assert!(
        retried.error && retried.code() == "NOT_ALWAYS",
        "nor does a retry after its send failed: {}",
        retried.body
    );
    assert_eq!(granted(app, &leaf).await, json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_rooted_in_neither_the_owner_nor_a_routine_cannot_draft() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (_, root, _, child) = delegated_task(&h).await;
    let (_, token) = h.started(child).await;
    root_source(app, root, "status").await;

    let refused = tool(app, &token, "draft_message", ship_post()).await;
    assert!(
        refused.error && refused.code() == "NOT_ALLOWED",
        "{}",
        refused.body
    );
}

async fn sidebar_status(app: &TestApp, bot: &str) -> Value {
    let list = app.owner("GET", "/v1/bots", None).await;
    let mut status = list.body["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == bot)
        .unwrap()["sidebar_status"]
        .clone();
    if let Some(fields) = status.as_object_mut() {
        assert!(fields.remove("since").is_some(), "{status}");
    }
    status
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_draft_marks_its_bot_as_needing_approval_until_approved() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, _, _, child) = delegated_task(&h).await;
    let (_, token) = h.started(child).await;
    let id = draft(app, &token, family_message()).await;
    h.release(child).await;
    app.wait_turn(child, &["waiting"]).await;
    assert_eq!(
        sidebar_status(app, &leaf).await,
        json!({ "kind": "needs_approval", "preview": "Telegram message to Family group" }),
        "the card in the bot's chat waits on the owner"
    );

    let approved = owner_action(app, &leaf, id, "approve", json!({ "version": 1 })).await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_ne!(
        sidebar_status(app, &leaf).await["kind"],
        "needs_approval",
        "an approved draft no longer waits on the owner"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_question_marks_its_bot_as_waiting_for_the_owner() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, _, _, child) = delegated_task(&h).await;
    let (_, token) = h.started(child).await;
    let asked = tool(
        app,
        &token,
        "ask_owner",
        json!({ "question": "Which account?", "choices": ["Main", "Alt"] }),
    )
    .await;
    assert_eq!(asked.result["status"], "pending", "{}", asked.body);
    h.release(child).await;
    app.wait_turn(child, &["waiting"]).await;
    assert_eq!(
        sidebar_status(app, &leaf).await,
        json!({ "kind": "waiting_for_answer", "preview": "Which account?" }),
        "the card in the bot's chat waits on the owner"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn approving_a_task_always_approves_its_later_messages_to_anyone() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let dm = |to: &str, task: &str| {
        json!({ "platform": "x", "account": "@Proliquid_xyz", "destination": to,
                "task": task, "body": format!("Hey {to}, want VIP access?") })
    };
    let unowned = tool(
        app,
        &token,
        "draft_message",
        json!({ "platform": "x", "destination": "@ada", "task": "Initial lead DMs",
                "body": "Hi" }),
    )
    .await;
    assert!(
        unowned.error
            && unowned
                .result
                .as_str()
                .unwrap()
                .contains("a task needs the account it sends from"),
        "{}",
        unowned.body
    );
    let first = draft(app, &token, dm("@ada", "Initial  lead DMs")).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let kind = json!({ "platform": "x", "account_id": null, "account": "@proliquid_xyz",
                       "destination": null, "task": "initial lead dms" });
    assert_eq!(
        shown(app, &main, turn).await[0]["kind"],
        kind,
        "a DM filed under a task can be approved always"
    );
    let approved = owner_action(
        app,
        &main,
        first,
        "approve",
        json!({ "version": 1, "always": true }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(granted(app, &main).await, json!([kind]));

    let token = h.continued(turn, &token).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("[else.bot always-approved message tasks]")
            && packet.contains("\"task\":\"initial lead dms\""),
        "the next attempt learns the saved task to reuse: {packet}"
    );
    let next = tool(app, &token, "draft_message", dm("@bob", "initial lead dms")).await;
    assert_eq!(
        next.result["status"], "approved",
        "the same task from the same account reaches a new lead without a card: {}",
        next.body
    );
    let other = tool(app, &token, "draft_message", dm("@cy", "Follow-ups")).await;
    assert_eq!(
        other.result["status"], "pending",
        "another task still waits for the owner: {}",
        other.body
    );
}

/// A bot's project directory with `files` written into it.
fn project(root: &std::path::Path, bot: &str, files: &[(&str, &[u8])]) {
    let dir = else_api::providers::host::project_dir(root, bot.parse().unwrap()).unwrap();
    for (name, data) in files {
        std::fs::write(dir.join(name), data).unwrap();
    }
}

fn png(shade: u8) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(2, 2, image::Rgb([shade, 0, 0]))
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

const PDF: &[u8] = b"%PDF-1.7\n1 0 obj << >> endobj\ntrailer << >>\n%%EOF\n";

fn files_of(card: &Value) -> Vec<Value> {
    card["content"]["attachments"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connected_email_sends_its_approved_pdf_from_the_account() {
    let root = tempfile::tempdir().unwrap();
    let mail = Arc::new(FakeMail::default());
    let h = harness(TestApp::builder().mail(mail.clone()).projects(root.path())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    project(root.path(), &main, &[("report.pdf", PDF)]);
    let account = connect(app, &main, "main@owner.example", true).await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;

    let mut args = connected_reply(account);
    args["attachments"] = json!([{ "path": "~/work/missing.pdf" }]);
    let refused = tool(app, &token, "draft_email", args.clone()).await;
    assert!(
        refused.error && refused.code() == "FILE_UNAVAILABLE",
        "a missing file is refused when drafting"
    );
    args["attachments"] = json!([{ "path": "report.pdf" }]);
    let id = draft(app, &token, args).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let card = shown(app, &main, turn).await[0].clone();
    let files = files_of(&card);
    assert_eq!(files.len(), 1, "{card}");
    assert_eq!(files[0]["name"], "report.pdf");
    assert_eq!(files[0]["media_type"], "application/pdf");
    assert_eq!(files[0]["size"], PDF.len());
    assert_eq!(files[0]["sha256"].as_str().unwrap().len(), 64);
    let file_id = files[0]["id"].as_str().unwrap().to_owned();
    let bytes = app
        .owner(
            "GET",
            &format!("/v1/bots/{main}/attachments/{file_id}"),
            None,
        )
        .await;
    assert_eq!(bytes.status, StatusCode::OK, "the card can fetch the file");

    let unseen = owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
    assert_eq!(unseen.status, StatusCode::CONFLICT, "{}", unseen.body);
    assert_eq!(unseen.body["error"]["code"], "attachments_unseen");
    let approved = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 1, "attachments": [file_id] }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let token = h.continued(turn, &token).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("Attachments: report.pdf (application/pdf, 1 KB)"),
        "{packet}"
    );
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(sent.result["status"], "sent", "{}", sent.body);
    let sends = mail.sends();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].attachments.len(), 1);
    let attached = &sends[0].attachments[0];
    assert_eq!(
        (attached.name.as_str(), attached.media_type.as_str()),
        ("report.pdf", "application/pdf")
    );
    use base64::Engine;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&attached.data_base64)
            .unwrap(),
        PDF
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_x_post_takes_pictures_the_owner_may_remove_and_stages_them_for_its_send() {
    let root = tempfile::tempdir().unwrap();
    let h = harness(TestApp::builder().projects(root.path())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, second) = (png(10), png(200));
    project(
        root.path(),
        &main,
        &[("a.png", &first), ("b.png", &second), ("report.pdf", PDF)],
    );
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;

    let mut post = ship_post();
    post["attachments"] = json!([{ "path": "report.pdf" }]);
    let refused = tool(app, &token, "draft_message", post.clone()).await;
    assert!(refused.error, "{}", refused.body);
    let refused = refused.result.as_str().unwrap();
    assert!(
        refused.contains("an X post takes only PNG, JPEG, GIF, or WebP images"),
        "{refused}"
    );
    post["attachments"] = json!([{ "path": "a.png" }, { "path": "~/work/b.png" }]);
    let id = draft(app, &token, post).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let files = files_of(&shown(app, &main, turn).await[0]);
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| f["media_type"] == "image/png"));
    let keep = files[1]["id"].as_str().unwrap().to_owned();
    let dir = else_api::providers::host::project_dir(root.path(), main.parse().unwrap()).unwrap();
    let staged = dir.join(".attachments").join(&keep).join("b.png");
    assert!(
        !staged.exists(),
        "nothing is staged before the owner approves"
    );
    for (change, why) in [
        (
            "json_insert(content, '$.attachments[#]', json_extract(content, '$.attachments[0]'))",
            "repeat",
        ),
        (
            "json_set(content, '$.attachments', json_array(json_extract(content, '$.attachments[1]'), \
          json_extract(content, '$.attachments[0]')))",
            "reorder",
        ),
    ] {
        let changed = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE outgoing_drafts SET content = {change} WHERE id = ?1"
        )))
        .bind(id)
        .execute(&app.db.pool)
        .await;
        assert!(changed.is_err(), "the owner may not {why} attachments");
    }

    let added = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "attachments": [Uuid::new_v4()] }),
    )
    .await;
    assert_eq!(added.status, StatusCode::BAD_REQUEST, "{}", added.body);
    let removed = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "attachments": [keep] }),
    )
    .await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);
    let files = files_of(&removed.body["draft"]);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["id"], keep);
    let changed = sqlx::query(
        "UPDATE outgoing_drafts SET content = json_set(content, '$.attachments[0].name', 'x.png')
         WHERE id = ?1",
    )
    .bind(id)
    .execute(&app.db.pool)
    .await;
    assert!(changed.is_err(), "a stored file never changes");

    let approved = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 2, "attachments": [keep] }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let token = h.continued(turn, &token).await;
    std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
    std::fs::write(&staged, b"tampered").unwrap();
    let started = tool(app, &token, "start_draft_send", json!({ "draft_id": id })).await;
    let result = started.result;
    assert_eq!(result["status"], "sending", "{}", started.body);
    assert_eq!(
        result["draft"]["attachment_paths"],
        json!([format!("~/work/.attachments/{keep}/b.png")])
    );
    assert_eq!(
        std::fs::read(&staged).unwrap(),
        second,
        "the claim restores the approved bytes"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_attachment_fails_the_send_and_nothing_leaves() {
    let root = tempfile::tempdir().unwrap();
    let mail = Arc::new(FakeMail::default());
    let h = harness(TestApp::builder().mail(mail.clone()).projects(root.path())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    project(root.path(), &main, &[("report.pdf", PDF)]);
    let account = connect(app, &main, "main@owner.example", true).await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let mut args = connected_reply(account);
    args["attachments"] = json!([{ "path": "report.pdf" }]);
    let id = draft(app, &token, args).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let file_id = files_of(&shown(app, &main, turn).await[0])[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let approved = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 1, "attachments": [file_id] }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let token = h.continued(turn, &token).await;
    sqlx::query("UPDATE attachments SET data = zeroblob(size) WHERE id = ?1")
        .bind(file_id.parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert!(sent.error && sent.code() == "FILE_CHANGED", "{}", sent.body);
    assert!(mail.commands.lock().unwrap().is_empty(), "nothing was sent");
    let status: String = sqlx::query_scalar("SELECT status FROM outgoing_drafts WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_approval_of_a_draft_with_files_needs_the_files_the_owner_saw() {
    let root = tempfile::tempdir().unwrap();
    let mail = Arc::new(FakeMail::default());
    let h = harness(TestApp::builder().mail(mail.clone()).projects(root.path())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    project(root.path(), &main, &[("report.pdf", PDF)]);
    let account = connect(app, &main, "main@owner.example", true).await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let mut args = connected_reply(account);
    args["attachments"] = json!([{ "path": "report.pdf" }]);
    let id = draft(app, &token, args).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let file_id = files_of(&shown(app, &main, turn).await[0])[0]["id"].clone();

    let unseen = send_with_draft(app, &main, "Send it.", json!({ "id": id, "version": 1 })).await;
    assert_eq!(unseen.status, StatusCode::CONFLICT, "{}", unseen.body);
    assert_eq!(unseen.body["error"]["code"], "attachments_unseen");
    let (_, token) = joined(
        &h,
        &main,
        turn,
        &token,
        "Send it.",
        Some(json!({ "id": id, "version": 1, "attachments": [file_id] })),
    )
    .await;
    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(sent.result["status"], "sent", "{}", sent.body);
    assert_eq!(mail.sends()[0].attachments.len(), 1);
}

/// Starts a request whose owner message approves the open draft `id`, and
/// confirms that approval.
async fn approve_again(h: &Harness, bot: &str, id: Uuid) -> (Uuid, String) {
    let app = &h.app;
    let sent = send_with_draft(
        app,
        bot,
        &format!("{GATE} Send it again."),
        json!({ "id": id, "version": 1 }),
    )
    .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let (turn, token) = h
        .started(sent.body["turn"]["id"].as_str().unwrap().parse().unwrap())
        .await;
    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    (turn, token)
}

/// A connected reply, approved on its card and claimed by a running request.
async fn approved_connected_reply(h: &Harness, bot: &str, account: Uuid) -> (Uuid, Uuid, String) {
    let app = &h.app;
    let (turn, token) = h.running(bot, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, connected_reply(account)).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    owner_action(app, bot, id, "approve", json!({ "version": 1 })).await;
    (id, turn, h.continued(turn, &token).await)
}

async fn action_statuses(app: &TestApp, account: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT status FROM connector_actions WHERE account_id = ?1 ORDER BY created_at, rowid",
    )
    .bind(account)
    .fetch_all(&app.db.pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_send_that_stops_before_the_message_is_written_fails_with_its_reason() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Envelope));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, first, token) = approved_connected_reply(&h, &main, account).await;

    let refused = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(refused["status"], "failed", "{refused}");
    assert_eq!(
        refused["detail"],
        "Nothing was sent: the mail server closed the connection while naming the sender and \
         recipients.",
    );
    assert_eq!(refused["sends"][0]["phase"], "envelope", "{refused}");
    assert_eq!(action_statuses(app, account).await, ["failed"]);
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;

    mail.fail_at(None);
    let (_, token) = approve_again(&h, &main, id).await;
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(sent["status"], "sent", "{sent}");
    assert!(
        mail.searches().is_empty(),
        "nothing was sent, so Sent is not searched"
    );
    let sends = mail.sends();
    assert_eq!(sends.len(), 2);
    let message_id = format!("draft-{}@owner.example", id.simple());
    assert_eq!(sends[0].message_id.as_deref(), Some(message_id.as_str()));
    assert_eq!(
        sends[1].message_id, sends[0].message_id,
        "a retry keeps its Message-ID"
    );
    assert_eq!(sent["sends"].as_array().unwrap().len(), 2, "{sent}");
    assert_eq!(action_statuses(app, account).await, ["failed", "succeeded"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uncertain_send_found_in_sent_is_never_sent_again() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Reply));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, first, token) = approved_connected_reply(&h, &main, account).await;

    let lost = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(lost["status"], "uncertain", "{lost}");
    assert!(
        lost["detail"].as_str().unwrap().starts_with(
            "It may have been sent: the mail server closed the connection while waiting for the \
             server's reply after the whole message was sent."
        ),
        "{lost}"
    );
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;

    mail.fail_at(None);
    mail.search_fails
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (second, token) = approve_again(&h, &main, id).await;
    let unsearched = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(unsearched["status"], "uncertain", "{unsearched}");
    assert!(
        unsearched["detail"]
            .as_str()
            .unwrap()
            .starts_with("Its Sent folder was not searched: the mail server timed out."),
        "{unsearched}"
    );
    assert_eq!(mail.sends().len(), 1, "a failed search sends nothing");
    h.release(second).await;
    app.wait_turn(second, &["completed"]).await;

    mail.search_fails
        .store(false, std::sync::atomic::Ordering::SeqCst);
    mail.in_sent
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (_, token) = approve_again(&h, &main, id).await;
    let found = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(found["status"], "sent", "{found}");
    assert!(
        found["detail"]
            .as_str()
            .unwrap()
            .contains("already holds it"),
        "{found}"
    );
    let message_id = format!("draft-{}@owner.example", id.simple());
    assert_eq!(mail.searches(), [message_id.clone(), message_id]);
    assert_eq!(mail.sends().len(), 1, "found in Sent, so never sent again");
    assert_eq!(action_statuses(app, account).await, ["succeeded"]);
    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "sent");
    assert_eq!(
        receipt["delivery"]["action_id"], found["sends"][0]["action_id"],
        "the card names the send that went out"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uncertain_send_missing_from_sent_waits_for_a_new_approval() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Reply));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, first, token) = approved_connected_reply(&h, &main, account).await;
    let lost = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(lost["status"], "uncertain", "{lost}");
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;

    mail.fail_at(None);
    let (second, token) = approve_again(&h, &main, id).await;
    let absent = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(absent["status"], "uncertain", "{absent}");
    assert!(
        absent["detail"]
            .as_str()
            .unwrap()
            .starts_with("Its Sent folder does not hold it"),
        "{absent}"
    );
    let again = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(
        again["status"], "uncertain",
        "the same approval never sends it"
    );
    let reconfirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert!(reconfirmed.error);
    assert_eq!(
        reconfirmed.result, "DRAFT_SETTLED: the draft is uncertain",
        "the owner message that led to the search cannot approve it again"
    );
    let earlier: Uuid = absent["sends"][0]["action_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let stale = outgoing_drafts::record_sent_check(
        &app.db.pool,
        id,
        Uuid::new_v4(),
        &[earlier],
        Ok(outgoing_drafts::SentCheck::Found),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(stale, DraftError::Settled(_)),
        "a search for another claim is refused: {stale:?}"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM outgoing_drafts WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "uncertain", "and changes nothing");
    assert_eq!(action_statuses(app, account).await, ["uncertain"]);
    assert_eq!(mail.sends().len(), 1);
    assert_eq!(mail.searches().len(), 1);
    h.release(second).await;
    app.wait_turn(second, &["completed"]).await;

    let (_, token) = approve_again(&h, &main, id).await;
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(sent["status"], "sent", "{sent}");
    assert_eq!(
        mail.searches().len(),
        1,
        "the owner approved after the search"
    );
    let sends = mail.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(
        sends[1].message_id, sends[0].message_id,
        "a duplicate is recognisable"
    );
    let attempts = sent["sends"].as_array().unwrap();
    assert_eq!(attempts.len(), 2, "{sent}");
    assert_eq!(
        (
            &attempts[0]["status"],
            &attempts[0]["phase"],
            &attempts[0]["sent_check"]
        ),
        (&json!("uncertain"), &json!("reply"), &json!("absent"))
    );
    assert_eq!(attempts[1]["status"], "succeeded");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsent_retry_keeps_the_earlier_send_uncertain() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Reply));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, first, token) = approved_connected_reply(&h, &main, account).await;
    let lost = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(lost["status"], "uncertain", "{lost}");
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;

    let (second, _) = approve_again(&h, &main, id).await;
    h.release(second).await;
    app.wait_turn(second, &["completed"]).await;
    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "uncertain", "{receipt}");
    assert_eq!(
        receipt["delivery"]["action_id"], lost["sends"][0]["action_id"],
        "the card names the send that may have gone out"
    );

    let sent = send_with_draft(
        app,
        &main,
        &format!("{GATE} Make it shorter."),
        json!({ "id": id, "version": 1 }),
    )
    .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let (_, token) = h
        .started(sent.body["turn"]["id"].as_str().unwrap().parse().unwrap())
        .await;
    let mut revision = connected_reply(account);
    revision["replaces"] = json!(id);
    let revised = tool(app, &token, "draft_email", revision).await;
    assert!(revised.error);
    assert_eq!(revised.result, "DRAFT_SETTLED: the draft is uncertain");
    assert_eq!(mail.sends().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uncertain_send_without_a_message_id_is_never_sent_again() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Reply));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, first, token) = approved_connected_reply(&h, &main, account).await;
    let lost = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(lost["status"], "uncertain", "{lost}");
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;
    sqlx::query("UPDATE draft_mail_sends SET message_id = NULL")
        .execute(&app.db.pool)
        .await
        .unwrap();

    mail.fail_at(None);
    for _ in 0..2 {
        let (turn, token) = approve_again(&h, &main, id).await;
        let blocked = tool(app, &token, "send_draft", json!({ "draft_id": id }))
            .await
            .result;
        assert_eq!(blocked["status"], "uncertain", "{blocked}");
        assert!(
            blocked["detail"].as_str().unwrap().starts_with(
                "Its Sent folder was not searched: an earlier send carried no Message-ID"
            ),
            "{blocked}"
        );
        h.release(turn).await;
        app.wait_turn(turn, &["completed"]).await;
    }
    assert!(mail.searches().is_empty());
    assert_eq!(mail.sends().len(), 1, "no approval sends it again");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_file_on_a_retry_keeps_an_earlier_send_uncertain() {
    let root = tempfile::tempdir().unwrap();
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Reply));
    let h = harness(TestApp::builder().mail(mail.clone()).projects(root.path())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    project(root.path(), &main, &[("report.pdf", PDF)]);
    let account = connect(app, &main, "main@owner.example", true).await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let mut args = connected_reply(account);
    args["attachments"] = json!([{ "path": "report.pdf" }]);
    let id = draft(app, &token, args).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;
    let file_id = files_of(&shown(app, &main, first).await[0])[0]["id"].clone();
    let seen = json!({ "id": id, "version": 1, "attachments": [file_id] });
    let mut approval = seen.clone();
    approval.as_object_mut().unwrap().remove("id");
    owner_action(app, &main, id, "approve", approval).await;
    let token = h.continued(first, &token).await;
    let lost = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(lost["status"], "uncertain", "{lost}");
    let earlier = lost["sends"][0]["action_id"].clone();
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;

    let again = send_with_draft(app, &main, &format!("{GATE} Send it again."), seen).await;
    assert_eq!(again.status, StatusCode::ACCEPTED, "{}", again.body);
    let (_, token) = h
        .started(again.body["turn"]["id"].as_str().unwrap().parse().unwrap())
        .await;
    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    sqlx::query("UPDATE attachments SET data = zeroblob(size) WHERE id = ?1")
        .bind(file_id.as_str().unwrap().parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
    let commands = mail.commands.lock().unwrap().len();
    let retried = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(retried.result["status"], "uncertain", "{}", retried.body);
    assert_eq!(
        retried.result["detail"],
        "the attachment report.pdf changed or is missing since the owner approved it; nothing \
         was sent An earlier send of it may have gone out."
    );
    assert_eq!(
        mail.commands.lock().unwrap().len(),
        commands,
        "nothing was sent"
    );
    let delivery: String = sqlx::query_scalar("SELECT delivery FROM outgoing_drafts WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&delivery).unwrap()["action_id"],
        earlier,
        "the draft names the send that may have gone out"
    );
}

/// A mail connector too busy to queue the send.
struct Busy;

#[async_trait::async_trait]
impl send::Connected for Busy {
    async fn send_mail(&self, _: Uuid, _: Uuid, _: MailCommand) -> Result<(), MailError> {
        Err(MailError::new(MailErrorCode::MailAccountBusy))
    }

    async fn find_sent(&self, _: Uuid, _: String) -> Result<bool, MailError> {
        unreachable!("no earlier send to search for")
    }

    async fn send_social(
        &self,
        _: Uuid,
        _: Uuid,
        _: else_api::app::outgoing::Platform,
        _: else_api::socials::types::SendMessage,
    ) -> Result<(), else_api::socials::types::Error> {
        unreachable!("these drafts are emails")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_mail_send_fails_and_a_queued_copy_never_goes_out() {
    let mail = Arc::new(FakeMail::default());
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let bot: Uuid = main.parse().unwrap();
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, turn, _) = approved_connected_reply(&h, &main, account).await;

    let refused = send::send_connected(&app.db.pool, turn, id, MAIL, &Busy)
        .await
        .unwrap();
    assert_eq!(refused.status.as_str(), "failed");
    let Some(Delivery::Mail { action_id, .. }) = refused.delivery.as_deref().cloned() else {
        panic!("a connected email: {refused:?}");
    };
    let Content::Email(email) = &refused.content.0 else {
        panic!("an email: {refused:?}");
    };
    let queued = app
        .state
        .connectors
        .execute(
            account,
            bot,
            turn,
            Some(action_id),
            email.send_command(Vec::new(), Some(email.message_id(id))),
        )
        .await;
    assert!(queued.is_err(), "{queued:?}");
    assert!(mail.sends().is_empty(), "the queued copy never went out");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_send_its_stopped_request_failed_is_never_admitted_after_the_request_resumes() {
    let mail = Arc::new(FakeMail::default());
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let bot: Uuid = main.parse().unwrap();
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, turn, _) = approved_connected_reply(&h, &main, account).await;
    let Claim::Fresh(claimed) = outgoing_drafts::claim_connected(&app.db.pool, turn, id, MAIL)
        .await
        .unwrap()
    else {
        panic!("the first call claims the email");
    };
    for status in ["cancelling", "running"] {
        sqlx::query("UPDATE turns SET status = ?2 WHERE id = ?1")
            .bind(turn)
            .bind(status)
            .execute(&app.db.pool)
            .await
            .unwrap();
    }
    let status: String = sqlx::query_scalar("SELECT status FROM outgoing_drafts WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "failed", "the stop failed the unsent email");
    let Payload::Mail(email) = claimed.payload else {
        panic!("an email");
    };
    let queued = app
        .state
        .connectors
        .execute(
            account,
            bot,
            turn,
            Some(claimed.action_id),
            email.send_command(Vec::new(), Some(email.message_id(id))),
        )
        .await;
    assert!(queued.is_err(), "{queued:?}");
    assert!(mail.sends().is_empty(), "the queued copy never went out");
}

/// The request's first call claims the email, and a second call resumes the
/// claim and stops before reading the outcome.
async fn stalled_retry(app: &TestApp, turn: Uuid, id: Uuid) -> Uuid {
    let first = outgoing_drafts::claim_connected(&app.db.pool, turn, id, MAIL)
        .await
        .unwrap();
    assert!(matches!(first, Claim::Fresh(_)), "{first:?}");
    match outgoing_drafts::claim_connected(&app.db.pool, turn, id, MAIL)
        .await
        .unwrap()
    {
        Claim::Resumed(claimed) => claimed.action_id,
        other => panic!("the retry resumes the claim: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_mail_retry_returns_the_receipt_its_claim_settled() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Envelope));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, turn, token) = approved_connected_reply(&h, &main, account).await;
    let claim = stalled_retry(app, turn, id).await;
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(sent["status"], "failed", "{sent}");

    let next = outgoing_drafts::next_mail_send(&app.db.pool, id, claim, "draft@owner.example")
        .await
        .unwrap();
    let MailSend::Settled(draft) = next else {
        panic!("the claim's receipt: {next:?}");
    };
    assert_eq!(draft.status.as_str(), "failed");
    assert_eq!(
        json!(draft.delivery.unwrap().0)["action_id"],
        sent["sends"][0]["action_id"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_retry_never_reads_or_changes_a_later_claim() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Envelope));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, first, token) = approved_connected_reply(&h, &main, account).await;
    let claim = stalled_retry(app, first, id).await;
    let failed = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(failed["status"], "failed", "{failed}");
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;
    mail.fail_at(None);
    let (_, token) = approve_again(&h, &main, id).await;
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(sent["status"], "sent", "{sent}");

    let next = outgoing_drafts::next_mail_send(&app.db.pool, id, claim, "draft@owner.example")
        .await
        .unwrap_err();
    assert!(matches!(next, DraftError::Settled(_)), "{next:?}");
    let after =
        outgoing_drafts::after_mail_send(&app.db.pool, id, claim, Some("MAIL_ACCOUNT_BUSY"))
            .await
            .unwrap_err();
    assert!(matches!(after, DraftError::Settled(_)), "{after:?}");
    let status: String = sqlx::query_scalar("SELECT status FROM outgoing_drafts WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "sent");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_after_the_earlier_send_succeeded_sends_nothing() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Reply));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, first, token) = approved_connected_reply(&h, &main, account).await;
    let lost = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(lost["status"], "uncertain", "{lost}");
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;

    mail.fail_at(None);
    let (_, token) = approve_again(&h, &main, id).await;
    sqlx::query("UPDATE connector_actions SET status = 'succeeded' WHERE account_id = ?1")
        .bind(account)
        .execute(&app.db.pool)
        .await
        .unwrap();
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(sent["status"], "sent", "{sent}");
    assert_eq!(
        sent["detail"],
        "An earlier send of it went out. It was not sent again."
    );
    assert_eq!(mail.sends().len(), 1);
    assert!(mail.searches().is_empty());
}
