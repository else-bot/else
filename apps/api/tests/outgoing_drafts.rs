mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::{TestApp, TestDb};
use else_api::connectors::{store, types::*};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::synthetic::Synthetic;
use serde_json::{Value, json};
use uuid::Uuid;

const SLOW: &str = "[synthetic:slow] email Ada about lunch on Friday, and keep it short.";

struct Harness {
    app: TestApp,
    synthetic: Arc<Synthetic>,
}

async fn harness() -> Harness {
    harness_with(None).await
}

async fn harness_with(mail: Option<Arc<FakeMail>>) -> Harness {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let synthetic = Arc::new(Synthetic::new(Duration::from_millis(5)));
    let registry = else_api::providers::Registry::new(vec![synthetic.clone()]);
    let app = match mail {
        Some(mail) => TestApp::with_mail(db, 4, registry, mail),
        None => TestApp::with_registry(db, 4, registry),
    };
    Harness { app, synthetic }
}

#[derive(Default)]
struct FakeMail {
    commands: std::sync::Mutex<Vec<MailCommand>>,
    connection_lost: std::sync::atomic::AtomicBool,
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
}

#[async_trait::async_trait]
impl MailTransport for FakeMail {
    async fn execute(
        &self,
        _: &MailConfig,
        _: &Auth,
        command: &MailCommand,
    ) -> Result<Value, MailError> {
        self.commands.lock().unwrap().push(command.clone());
        if self
            .connection_lost
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(MailError::new("connection_lost"));
        }
        Ok(json!({ "sent": true }))
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
        app.state.secret_key.as_ref().unwrap(),
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

async fn grant(app: &TestApp, turn_id: Uuid) -> String {
    let (bot, conversation): (Uuid, Uuid) =
        sqlx::query_as("SELECT bot_id, conversation_id FROM turns WHERE id = ?1")
            .bind(turn_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    app.scheduler
        .grants()
        .issue(
            bot,
            conversation,
            turn_id,
            vec![Tool::ReadMemory, Tool::AskOwner, Tool::Computer, Tool::Mail],
            DEFAULT_TTL,
        )
        .await
}

async fn send(app: &TestApp, bot: &str, text: &str, draft: Option<Value>) -> common::Reply {
    let mut body =
        json!({ "request_id": Uuid::new_v4(), "text": text, "provider_id": "synthetic" });
    if let Some(draft) = draft {
        body["open_draft"] = draft;
    }
    app.owner("POST", &format!("/v1/bots/{bot}/turns"), Some(body))
        .await
}

async fn running(app: &TestApp, bot: &str, text: &str, draft: Option<Value>) -> (Uuid, String) {
    let sent = send(app, bot, text, draft).await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let turn: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    wait(app, turn, "running").await;
    (turn, grant(app, turn).await)
}

async fn tool(app: &TestApp, token: &str, name: &str, args: Value) -> common::Reply {
    app.call(
        "POST",
        "/tools/v1/mcp",
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                     "params": { "name": name, "arguments": args } })),
        Some(token),
    )
    .await
}

fn tool_text(reply: &common::Reply) -> Value {
    let text = reply.body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    serde_json::from_str(text).unwrap_or_else(|_| json!(text))
}

fn refusal(reply: &common::Reply) -> String {
    assert_eq!(reply.body["result"]["isError"], true, "{}", reply.body);
    tool_text(reply).as_str().unwrap().to_owned()
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
    let result = tool_text(&drafted);
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

async fn wait(app: &TestApp, turn: Uuid, status: &str) {
    app.wait_turn_for(&turn.to_string(), &[status], Duration::from_secs(30))
        .await;
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
    let Harness { app, synthetic } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main, SLOW, None).await;
    let mut bad = lunch();
    bad["subject"] = json!("Hi\r\nBcc: everyone@example.com");
    assert!(refusal(&tool(&app, &token, "draft_email", bad).await).starts_with("INVALID_DRAFT"));
    let id = draft(&app, &token, lunch()).await;
    let parked = tool(&app, &token, "memory_bots", json!({})).await;
    assert_eq!(parked.status, StatusCode::CONFLICT, "the attempt must end");

    wait(&app, turn, "waiting").await;
    let card = shown(&app, &main, turn).await[0].clone();
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
    assert_eq!(journaled(&app, &main, id).await["status"], "pending");

    let edited = owner_action(
        &app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "body": "Hi Ada, lunch Friday?" }),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);
    assert_eq!(edited.body["draft"]["version"], 2);
    let stale = owner_action(&app, &main, id, "approve", json!({ "version": 1 })).await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);
    assert_eq!(stale.body["error"]["code"], "draft_changed");
    let bad_edit = owner_action(
        &app,
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
        &app,
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
        &app,
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
        &app,
        &main,
        id,
        "approve",
        json!({ "version": 2, "subject": "Something else" }),
    )
    .await;
    assert_eq!(changed_replay.status, StatusCode::CONFLICT);
    let replay = owner_action(&app, &main, id, "approve", json!({ "version": 3 })).await;
    assert_eq!(replay.status, StatusCode::OK, "{}", replay.body);
    assert_eq!(replay.body["requeued"], false);
    let late_edit = owner_action(
        &app,
        &main,
        id,
        "edit",
        json!({ "version": 3, "body": "changed" }),
    )
    .await;
    assert_eq!(late_edit.body["error"]["code"], "draft_settled");

    wait(&app, turn, "running").await;
    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(packet.contains("[else.bot outgoing drafts]"), "{packet}");
    assert!(
        packet.contains("Subject: Lunch Friday\n\nHi Ada, lunch Friday?"),
        "{packet}"
    );
    assert!(
        packet.contains("The owner approved it on its card"),
        "{packet}"
    );

    let token = grant(&app, turn).await;
    let local = tool(
        &app,
        &token,
        "computer_run",
        json!({ "command": "true", "effect": "local", "approved_draft": id }),
    )
    .await;
    assert!(refusal(&local).contains("only with effect external"));
    let claimed = tool(&app, &token, "computer_run", send_args(id)).await;
    assert!(
        refusal(&claimed).starts_with("COMPUTER_UNAVAILABLE"),
        "the claim passed the gate and reached the computer: {}",
        claimed.body
    );
    assert_eq!(questions(&app).await, 0, "no second approval card");
    let again = tool(&app, &token, "computer_run", send_args(id)).await;
    assert!(
        refusal(&again).starts_with("DRAFT_SETTLED"),
        "one send per approval"
    );
    let generic = tool(
        &app,
        &token,
        "computer_run",
        json!({ "command": "python3 send_mail.py", "effect": "external" }),
    )
    .await;
    assert_eq!(tool_text(&generic)["status"], "pending", "{}", generic.body);
    assert_eq!(
        questions(&app).await,
        1,
        "other external calls still need their card"
    );

    let token = grant(&app, turn).await;
    let sent = tool(
        &app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "sent", "detail": "250 OK id=abc" }),
    )
    .await;
    assert_eq!(tool_text(&sent)["status"], "sent", "{}", sent.body);
    let replayed = tool(
        &app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "sent" }),
    )
    .await;
    assert_eq!(tool_text(&replayed)["status"], "sent", "{}", replayed.body);
    let contradicted = tool(
        &app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "failed" }),
    )
    .await;
    assert!(refusal(&contradicted).starts_with("DRAFT_SETTLED"));

    let receipt = journaled(&app, &main, id).await;
    assert_eq!(receipt["status"], "sent");
    assert_eq!(receipt["result_detail"], "250 OK id=abc");
    assert_eq!(receipt["delivery"]["via"], "tool");
    assert_eq!(receipt["delivery"]["tool"], "computer_run");
    assert!(receipt["settled_at"].is_string());
    let card = shown(&app, &main, turn).await[0].clone();
    assert_eq!(card["status"], "sent");
    assert_eq!(card["can_approve"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn discarding_continues_the_turn_and_nothing_can_send_it() {
    let Harness { app, synthetic } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main, SLOW, None).await;
    let id = draft(&app, &token, lunch()).await;
    let early = tool(
        &app,
        &grant(&app, turn).await,
        "start_draft_send",
        json!({ "draft_id": id }),
    )
    .await;
    assert!(
        refusal(&early).starts_with("DRAFT_SETTLED"),
        "a pending draft cannot be sent"
    );
    wait(&app, turn, "waiting").await;

    let discarded = owner_action(&app, &main, id, "discard", json!({})).await;
    assert_eq!(discarded.status, StatusCode::OK, "{}", discarded.body);
    assert_eq!(discarded.body["draft"]["status"], "discarded");
    assert_eq!(discarded.body["requeued"], true);
    let replay = owner_action(&app, &main, id, "discard", json!({})).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.body["requeued"], false);
    let approve = owner_action(&app, &main, id, "approve", json!({ "version": 1 })).await;
    assert_eq!(
        approve.body["error"]["code"], "draft_settled",
        "{}",
        approve.body
    );

    wait(&app, turn, "running").await;
    let token = grant(&app, turn).await;
    let send = tool(&app, &token, "computer_run", send_args(id)).await;
    assert!(refusal(&send).starts_with("DRAFT_SETTLED"));
    let report = tool(
        &app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "sent" }),
    )
    .await;
    assert!(
        refusal(&report).starts_with("OTHER_REQUEST"),
        "a report cannot invent a send"
    );
    wait(&app, turn, "completed").await;
    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("The owner discarded it on its card"),
        "{packet}"
    );
    assert_eq!(journaled(&app, &main, id).await["status"], "discarded");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follow_up_carries_the_owners_edits_and_a_revision_replaces_the_card() {
    let Harness { app, synthetic } = harness().await;
    let main = app.main_bot().await;
    let (first, token) = running(&app, &main, SLOW, None).await;
    let id = draft(&app, &token, lunch()).await;
    wait(&app, first, "waiting").await;
    let turns_before: i64 = sqlx::query_scalar("SELECT count(*) FROM turns")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let stale = send(
        &app,
        &main,
        "Shorter please.",
        Some(json!({ "id": id, "version": 2 })),
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

    let follow_up = "[synthetic:slow] Make it warmer and mention the new place.";
    let (second, token) = running(
        &app,
        &main,
        follow_up,
        Some(json!({ "id": id, "version": 1, "body": "Hi Ada, lunch Friday? My treat." })),
    )
    .await;
    assert_eq!(status(&app, first).await, "interrupted");
    let old = shown(&app, &main, first).await[0].clone();
    assert_eq!(old["status"], "pending");
    assert_eq!(
        old["version"], 2,
        "the owner's edits were saved with the message"
    );
    assert_eq!(old["content"]["body"], "Hi Ada, lunch Friday? My treat.");
    assert_eq!(old["can_approve"], false);
    let button = owner_action(&app, &main, id, "approve", json!({ "version": 2 })).await;
    assert_eq!(
        button.body["error"]["code"], "draft_inactive",
        "{}",
        button.body
    );
    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(packet.contains("in_context=\"true\""), "{packet}");
    assert!(
        packet.contains("Hi Ada, lunch Friday? My treat."),
        "{packet}"
    );
    assert!(packet.contains(follow_up), "{packet}");

    let mut revised = lunch();
    revised["body"] = json!("Hi Ada,\n\nLunch Friday at the new ramen place? My treat.\n\nMain");
    revised["replaces"] = json!(id);
    let drafted = tool_text(&tool(&app, &token, "draft_email", revised).await);
    assert_eq!(drafted["status"], "pending", "{drafted}");
    assert_eq!(drafted["draft"]["revision"], 2);
    let new_id: Uuid = drafted["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(drafted["draft"]["lineage_id"], old["lineage_id"]);

    let old = shown(&app, &main, first).await[0].clone();
    assert_eq!(old["status"], "superseded");
    assert_eq!(old["superseded_by"], new_id.to_string());
    let card = shown(&app, &main, second).await[0].clone();
    assert_eq!(card["id"], new_id.to_string());
    assert_eq!(card["status"], "pending");
    assert_eq!(journaled(&app, &main, id).await["status"], "superseded");
    let obsolete = send(
        &app,
        &main,
        "Send it.",
        Some(json!({ "id": id, "version": 2 })),
    )
    .await;
    assert_eq!(
        obsolete.body["error"]["code"], "draft_settled",
        "{}",
        obsolete.body
    );
    let obsolete = owner_action(
        &app,
        &main,
        id,
        "edit",
        json!({ "version": 2, "body": "x" }),
    )
    .await;
    assert_eq!(obsolete.body["error"]["code"], "draft_settled");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clear_chat_approval_confirms_exactly_the_open_version() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let (first, token) = running(&app, &main, SLOW, None).await;
    let id = draft(&app, &token, lunch()).await;
    wait(&app, first, "waiting").await;

    let (second, token) = running(
        &app,
        &main,
        "[synthetic:slow] Looks good, send it.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let early = tool(&app, &token, "computer_run", send_args(id)).await;
    assert!(
        refusal(&early).starts_with("DRAFT_SETTLED"),
        "chat words alone send nothing"
    );
    let confirmed = tool(
        &app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(
        tool_text(&confirmed)["status"],
        "approved",
        "{}",
        confirmed.body
    );
    let replay = tool(
        &app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(tool_text(&replay)["status"], "approved", "{}", replay.body);

    assert!(
        shown(&app, &main, first).await.is_empty(),
        "the card moved under the follow-up"
    );
    let card = shown(&app, &main, second).await[0].clone();
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

    let started = tool(&app, &token, "start_draft_send", json!({ "draft_id": id })).await;
    let started = tool_text(&started);
    assert_eq!(started["status"], "sending", "{started}");
    assert_eq!(started["draft"]["content"]["subject"], "Lunch on Friday?");
    let twice = tool(&app, &token, "start_draft_send", json!({ "draft_id": id })).await;
    assert!(refusal(&twice).starts_with("DRAFT_SETTLED"));
    let uncertain = tool(
        &app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "uncertain", "detail": "SMTP timeout after DATA" }),
    )
    .await;
    assert_eq!(
        tool_text(&uncertain)["status"],
        "uncertain",
        "{}",
        uncertain.body
    );
    let receipt = journaled(&app, &main, id).await;
    assert_eq!(receipt["status"], "uncertain");
    assert_eq!(receipt["delivery"]["via"], "host");
    assert_eq!(questions(&app).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_message_sent_with_the_unchanged_draft_can_approve_it() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let (first, token) = running(&app, &main, SLOW, None).await;
    let id = draft(&app, &token, lunch()).await;
    wait(&app, first, "waiting").await;

    let (_, plain) = running(&app, &main, "[synthetic:slow] Send it.", None).await;
    let unbound = tool(
        &app,
        &plain,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert!(
        refusal(&unbound).starts_with("NOT_IN_CONTEXT"),
        "a message without the draft"
    );

    let (_, token) = running(
        &app,
        &main,
        "[synthetic:slow] Send it.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let edited = owner_action(
        &app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "subject": "Lunch?" }),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);
    let changed = tool(
        &app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert!(
        refusal(&changed).starts_with("NOT_IN_CONTEXT"),
        "the owner changed it after sending"
    );
    let unknown = tool(
        &app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": Uuid::new_v4() }),
    )
    .await;
    assert!(refusal(&unknown).starts_with("NOT_FOUND"));
    assert_eq!(journaled(&app, &main, id).await["status"], "pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreported_send_is_uncertain_and_an_unused_approval_fails() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main, SLOW, None).await;
    let id = draft(&app, &token, lunch()).await;
    wait(&app, turn, "waiting").await;
    owner_action(&app, &main, id, "approve", json!({ "version": 1 })).await;
    wait(&app, turn, "running").await;
    let token = grant(&app, turn).await;
    tool(&app, &token, "computer_run", send_args(id)).await;
    wait(&app, turn, "completed").await;
    let receipt = journaled(&app, &main, id).await;
    assert_eq!(receipt["status"], "uncertain", "{receipt}");
    assert!(
        receipt["result_detail"]
            .as_str()
            .unwrap()
            .contains("before the bot reported")
    );

    let (turn, token) = running(&app, &main, SLOW, None).await;
    let mut other = lunch();
    other["to"] = json!(["carol@example.com"]);
    let id = draft(&app, &token, other).await;
    wait(&app, turn, "waiting").await;
    owner_action(&app, &main, id, "approve", json!({ "version": 1 })).await;
    wait(&app, turn, "completed").await;
    let receipt = journaled(&app, &main, id).await;
    assert_eq!(receipt["status"], "failed", "{receipt}");
    assert!(
        receipt["result_detail"]
            .as_str()
            .unwrap()
            .contains("Nothing was sent")
    );

    let (retry, token) = running(
        &app,
        &main,
        "[synthetic:slow] Try sending it again.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let confirmed = tool(
        &app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(
        tool_text(&confirmed)["status"],
        "approved",
        "{}",
        confirmed.body
    );
    let failed = tool(
        &app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "failed", "detail": "No SMTP credentials stored." }),
    )
    .await;
    assert_eq!(tool_text(&failed)["status"], "failed", "{}", failed.body);
    let card = shown(&app, &main, retry).await[0].clone();
    assert_eq!(card["status"], "failed");
    assert_eq!(card["result_detail"], "No SMTP credentials stored.");
    let discarded = owner_action(&app, &main, id, "discard", json!({})).await;
    assert_eq!(
        discarded.body["draft"]["status"], "discarded",
        "{}",
        discarded.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_x_post_needs_no_destination_and_its_card_approval_sends_it_once() {
    let Harness { app, synthetic } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main, SLOW, None).await;
    let emailish = json!({ "platform": "x", "body": "Hi", "subject": "Launch" });
    assert!(
        refusal(&tool(&app, &token, "draft_message", emailish).await).starts_with("INVALID_DRAFT"),
        "a post has no subject"
    );
    let id = draft(&app, &token, ship_post()).await;
    wait(&app, turn, "waiting").await;
    let card = shown(&app, &main, turn).await[0].clone();
    assert_eq!(
        card["content"],
        json!({ "channel": "message", "platform": "x", "account": null, "destination": null,
                "body": "We shipped native drafts today." }),
        "no addresses, Cc/Bcc, or subject"
    );
    assert_eq!(card["can_approve"], true);

    let subject = owner_action(
        &app,
        &main,
        id,
        "approve",
        json!({ "version": 1, "subject": "Launch" }),
    )
    .await;
    assert_eq!(subject.status, StatusCode::BAD_REQUEST, "{}", subject.body);
    assert_eq!(subject.body["error"]["code"], "invalid_draft");
    assert_eq!(
        shown(&app, &main, turn).await[0]["status"],
        "pending",
        "a refused approval approves nothing"
    );
    let edited = owner_action(
        &app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "body": "Native drafts shipped today." }),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);
    let stale = owner_action(&app, &main, id, "approve", json!({ "version": 1 })).await;
    assert_eq!(stale.body["error"]["code"], "draft_changed");
    let approved = owner_action(
        &app,
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
        &app,
        &main,
        id,
        "approve",
        json!({ "version": 2, "body": "Native drafts shipped today!" }),
    )
    .await;
    assert_eq!(replay.status, StatusCode::OK, "{}", replay.body);
    assert_eq!(replay.body["requeued"], false);

    wait(&app, turn, "running").await;
    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("channel=\"message\" platform=\"x\""),
        "{packet}"
    );
    assert!(packet.contains("\nNative drafts shipped today!\n</outgoing_draft>"));
    assert!(!packet.contains("Subject:"), "{packet}");
    assert!(packet.contains("send exactly this X post now"), "{packet}");

    let token = grant(&app, turn).await;
    let claimed = tool(&app, &token, "computer_run", send_args(id)).await;
    assert!(
        refusal(&claimed).starts_with("COMPUTER_UNAVAILABLE"),
        "{}",
        claimed.body
    );
    let again = tool(&app, &token, "computer_run", send_args(id)).await;
    assert!(refusal(&again).starts_with("DRAFT_SETTLED"));
    assert_eq!(questions(&app).await, 0, "no second approval card");
    let token = grant(&app, turn).await;
    let sent = tool_text(
        &tool(
            &app,
            &token,
            "report_draft_send",
            json!({ "draft_id": id, "status": "sent", "detail": "post id 1890" }),
        )
        .await,
    );
    assert_eq!(sent["status"], "sent");
    assert_eq!(sent["medium"], "X post");
    let receipt = journaled(&app, &main, id).await;
    assert_eq!(receipt["status"], "sent");
    assert_eq!(receipt["result_detail"], "post id 1890");
    assert_eq!(receipt["delivery"]["via"], "tool");
    assert_eq!(receipt["content"]["platform"], "x");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_telegram_message_keeps_its_channel_and_owner_edits_across_revisions() {
    let Harness { app, synthetic } = harness().await;
    let main = app.main_bot().await;
    let (first, token) = running(&app, &main, SLOW, None).await;
    let mut nowhere = family_message();
    nowhere["destination"] = Value::Null;
    assert!(
        refusal(&tool(&app, &token, "draft_message", nowhere).await).starts_with("INVALID_DRAFT"),
        "a telegram message names its chat"
    );
    let id = draft(&app, &token, family_message()).await;
    wait(&app, first, "waiting").await;
    let card = shown(&app, &main, first).await[0].clone();
    assert_eq!(card["content"]["channel"], "message");
    assert_eq!(card["content"]["platform"], "telegram");
    assert_eq!(card["content"]["destination"], "Family group");
    assert_eq!(card["content"]["account"], "@main_bot");
    for key in ["from", "to", "cc", "bcc", "subject"] {
        assert!(card["content"].get(key).is_none(), "{key} in {card}");
    }

    let with_subject = send(
        &app,
        &main,
        "Make it 9.",
        Some(json!({ "id": id, "version": 1, "subject": "Dinner" })),
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
        status(&app, first).await,
        "waiting",
        "the message was refused"
    );

    let (second, token) = running(
        &app,
        &main,
        "[synthetic:slow] Make it 9 and ask who brings dessert.",
        Some(json!({ "id": id, "version": 1, "body": "Dinner at 9 on Friday?" })),
    )
    .await;
    let old = shown(&app, &main, first).await[0].clone();
    assert_eq!(old["version"], 2, "the owner's edits were saved");
    assert_eq!(old["content"]["body"], "Dinner at 9 on Friday?");
    let packet = synthetic.packets().await.last().unwrap().clone();
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
    let switched = tool(&app, &token, "draft_email", as_email).await;
    assert!(
        refusal(&switched).starts_with("CHANNEL_CHANGED"),
        "a message cannot become an email"
    );
    let mut as_post = ship_post();
    as_post["replaces"] = json!(id);
    let switched = tool(&app, &token, "draft_message", as_post).await;
    assert!(
        refusal(&switched).starts_with("CHANNEL_CHANGED"),
        "a Telegram message cannot become an X post"
    );
    assert_eq!(shown(&app, &main, first).await[0]["status"], "pending");

    let mut revised = family_message();
    revised["body"] = json!("Dinner at 9 on Friday? Who brings dessert?");
    revised["replaces"] = json!(id);
    let revised = tool_text(&tool(&app, &token, "draft_message", revised).await);
    assert_eq!(revised["status"], "pending", "{revised}");
    assert_eq!(revised["draft"]["revision"], 2);
    assert_eq!(revised["draft"]["lineage_id"], old["lineage_id"]);
    assert_eq!(revised["draft"]["content"]["platform"], "telegram");
    let new_id: Uuid = revised["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(shown(&app, &main, first).await[0]["status"], "superseded");
    assert_eq!(
        shown(&app, &main, second).await[0]["id"],
        new_id.to_string()
    );
    let obsolete = send(
        &app,
        &main,
        "Send it.",
        Some(json!({ "id": id, "version": 2 })),
    )
    .await;
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

    wait(&app, second, "waiting").await;
    let (third, token) = running(
        &app,
        &main,
        "[synthetic:slow] Perfect, send it.",
        Some(json!({ "id": new_id, "version": 1 })),
    )
    .await;
    let confirmed = tool_text(
        &tool(
            &app,
            &token,
            "confirm_draft_approval",
            json!({ "draft_id": new_id }),
        )
        .await,
    );
    assert_eq!(confirmed["status"], "approved", "{confirmed}");
    assert!(
        confirmed["next"]
            .as_str()
            .unwrap()
            .contains("Telegram message")
    );
    let card = shown(&app, &main, third).await[0].clone();
    assert_eq!(card["approval_source"], "message");
    assert_eq!(card["approved_version"], 1);
    let started = tool_text(
        &tool(
            &app,
            &token,
            "start_draft_send",
            json!({ "draft_id": new_id }),
        )
        .await,
    );
    assert_eq!(started["status"], "sending", "{started}");
    assert_eq!(
        started["draft"]["content"]["body"],
        "Dinner at 9 on Friday? Who brings dessert?"
    );
    let uncertain = tool_text(
        &tool(
            &app,
            &token,
            "report_draft_send",
            json!({ "draft_id": new_id, "status": "uncertain", "detail": "Bot API timed out" }),
        )
        .await,
    );
    assert_eq!(uncertain["status"], "uncertain");
    assert_eq!(uncertain["medium"], "Telegram message");
    let receipt = journaled(&app, &main, new_id).await;
    assert_eq!(receipt["status"], "uncertain");
    assert_eq!(receipt["delivery"]["via"], "host");
    assert_eq!(receipt["content"]["destination"], "Family group");
    assert_eq!(questions(&app).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_approval_binds_the_open_message_version_only() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let (first, token) = running(&app, &main, SLOW, None).await;
    let id = draft(&app, &token, family_message()).await;
    wait(&app, first, "waiting").await;

    let (_, token) = running(
        &app,
        &main,
        "[synthetic:slow] Send it.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let edited = owner_action(
        &app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "body": "Dinner at 7?" }),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);
    let changed = tool(
        &app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert!(
        refusal(&changed).starts_with("NOT_IN_CONTEXT"),
        "the owner changed it after sending"
    );
    let send = tool(&app, &token, "computer_run", send_args(id)).await;
    assert!(refusal(&send).starts_with("DRAFT_SETTLED"));
    assert_eq!(journaled(&app, &main, id).await["status"], "pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connected_email_sends_the_owners_approved_edit_once_through_its_account() {
    let mail = Arc::new(FakeMail::default());
    let Harness { app, synthetic } = harness_with(Some(mail.clone())).await;
    let main = app.main_bot().await;
    let account = connect(&app, &main, "main@owner.example", true).await;
    let unpermitted = connect(&app, &main, "other@owner.example", false).await;
    let (turn, token) = running(&app, &main, SLOW, None).await;

    let direct = tool(
        &app,
        &token,
        "mail_action",
        json!({ "account_id": account, "action_id": Uuid::new_v4(),
                "command": { "operation": "send", "draft": {
                    "to": ["ada@example.com"], "subject": "Hi", "body": "Unapproved" } } }),
    )
    .await;
    assert_eq!(refusal(&direct), "mail_send_needs_approved_draft");
    let reply = tool(
        &app,
        &token,
        "mail_action",
        json!({ "account_id": account, "action_id": Uuid::new_v4(),
                "command": { "operation": "reply", "body": "Unapproved", "send": true,
                             "message": { "folder": "INBOX", "uid_validity": 7, "uid": 42 } } }),
    )
    .await;
    assert_eq!(refusal(&reply), "mail_send_needs_approved_draft");
    assert!(
        mail.commands.lock().unwrap().is_empty(),
        "nothing reached the server"
    );
    let forbidden = tool(&app, &token, "draft_email", connected_reply(unpermitted)).await;
    assert!(
        refusal(&forbidden).starts_with("MAIL_FORBIDDEN"),
        "{}",
        forbidden.body
    );
    let mut spoofed = connected_reply(account);
    spoofed["from"] = json!("ceo@owner.example");
    assert!(
        refusal(&tool(&app, &token, "draft_email", spoofed).await).starts_with("INVALID_DRAFT")
    );

    let paused = connect(&app, &main, "paused@owner.example", true).await;
    let paused_response = app
        .owner(
            "PATCH",
            &format!("/v1/connectors/email/accounts/{paused}"),
            Some(json!({"active":false})),
        )
        .await;
    assert_eq!(paused_response.status, StatusCode::OK);
    assert!(
        refusal(&tool(&app, &token, "draft_email", connected_reply(paused)).await)
            .starts_with("MAIL_FORBIDDEN")
    );

    let folder = "Projects/\"urgent\"/INBOX";
    let mut args = connected_reply(account);
    args["reply_to"]["folder"] = json!(folder);
    let id = draft(&app, &token, args).await;
    wait(&app, turn, "waiting").await;
    let card = shown(&app, &main, turn).await[0].clone();
    assert_eq!(
        card["content"]["from"], "main@owner.example",
        "the account's own address"
    );
    assert_eq!(card["content"]["account_id"], account.to_string());
    assert_eq!(card["content"]["reply_to"]["uid"], 42);
    assert!(!card.to_string().contains("private-app-password"));

    let approved = owner_action(
        &app,
        &main,
        id,
        "approve",
        json!({ "version": 1, "subject": "Re: Lunch Friday", "body": "Friday at noon. Main" }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    wait(&app, turn, "running").await;
    let packet = synthetic.packets().await.last().unwrap().clone();
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

    let token = grant(&app, turn).await;
    for (name, args) in [
        ("computer_run", send_args(id)),
        ("start_draft_send", json!({ "draft_id": id })),
    ] {
        let other = tool(&app, &token, name, args).await;
        assert!(
            refusal(&other).starts_with("CONNECTED_ACCOUNT"),
            "{name}: {}",
            other.body
        );
    }
    let sent = tool(&app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(tool_text(&sent)["status"], "sent", "{}", sent.body);
    let again = tool(&app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(
        tool_text(&again)["status"],
        "sent",
        "a repeat reads the receipt"
    );
    let contradicted = tool(
        &app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "failed" }),
    )
    .await;
    assert!(refusal(&contradicted).starts_with("DRAFT_SETTLED"));

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
    let receipt = journaled(&app, &main, id).await;
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
    let Harness { app, .. } = harness_with(Some(mail.clone())).await;
    let main = app.main_bot().await;
    let account = connect(&app, &main, "main@owner.example", true).await;
    let (first, token) = running(&app, &main, SLOW, None).await;
    let id = draft(&app, &token, connected_reply(account)).await;
    wait(&app, first, "waiting").await;
    owner_action(&app, &main, id, "approve", json!({ "version": 1 })).await;
    wait(&app, first, "running").await;
    let token = grant(&app, first).await;
    let lost = tool(&app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(tool_text(&lost)["status"], "uncertain", "{}", lost.body);
    let again = tool(&app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(tool_text(&again)["status"], "uncertain");
    assert_eq!(mail.sends().len(), 1, "an uncertain send is never repeated");
    wait(&app, first, "completed").await;
    assert_eq!(journaled(&app, &main, id).await["status"], "uncertain");

    mail.connection_lost
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let (second, token) = running(
        &app,
        &main,
        "[synthetic:slow] I checked Sent, it is not there. Send it again.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let confirmed = tool(
        &app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(
        tool_text(&confirmed)["status"],
        "approved",
        "{}",
        confirmed.body
    );
    permit(&app, account, &main, false).await;
    let revoked = tool(&app, &token, "send_draft", json!({ "draft_id": id })).await;
    let revoked = tool_text(&revoked);
    assert_eq!(revoked["status"], "failed", "{revoked}");
    assert!(
        revoked["detail"]
            .as_str()
            .unwrap()
            .contains("Nothing was sent"),
        "{revoked}"
    );
    assert_eq!(mail.sends().len(), 1, "a revoked account sends nothing");
    wait(&app, second, "completed").await;

    permit(&app, account, &main, true).await;
    let (third, token) = running(
        &app,
        &main,
        "[synthetic:slow] Make it shorter.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let mut unbound = connected_reply(account);
    unbound["account_id"] = Value::Null;
    unbound["reply_to"] = Value::Null;
    unbound["from"] = json!("main@owner.example");
    unbound["replaces"] = json!(id);
    let changed = tool(&app, &token, "draft_email", unbound).await;
    assert!(
        refusal(&changed).starts_with("CHANNEL_CHANGED"),
        "{}",
        changed.body
    );
    let mut revision = connected_reply(account);
    revision["body"] = json!("Friday works.");
    revision["replaces"] = json!(id);
    let revised = draft(&app, &token, revision).await;
    wait(&app, third, "waiting").await;
    let card = shown(&app, &main, third).await[0].clone();
    assert_eq!(card["id"], revised.to_string());
    assert_eq!(card["status"], "pending", "a revision is approved afresh");
    assert_eq!(card["content"]["account_id"], account.to_string());
    owner_action(&app, &main, revised, "approve", json!({ "version": 1 })).await;
    wait(&app, third, "running").await;
    let token = grant(&app, third).await;
    let sent = tool(&app, &token, "send_draft", json!({ "draft_id": revised })).await;
    assert_eq!(tool_text(&sent)["status"], "sent", "{}", sent.body);
    let sends = mail.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[1].body, "Friday works.");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connected_send_settles_from_its_mail_receipt_even_after_the_request_ends() {
    use else_api::app::outgoing::Delivery;
    use else_api::db::outgoing_drafts;
    let mail = Arc::new(FakeMail::default());
    let Harness { app, .. } = harness_with(Some(mail.clone())).await;
    let main = app.main_bot().await;
    let bot: Uuid = main.parse().unwrap();
    let account = connect(&app, &main, "main@owner.example", true).await;
    let generation = store::account(&app.db.pool, account)
        .await
        .unwrap()
        .unwrap()
        .generation;
    let mut outcomes = Vec::new();
    for dispatched in [false, true] {
        let (turn, token) = running(&app, &main, SLOW, None).await;
        let mut args = connected_reply(account);
        args["body"] = json!(format!("Dispatched: {dispatched}"));
        let id = draft(&app, &token, args).await;
        wait(&app, turn, "waiting").await;
        owner_action(&app, &main, id, "approve", json!({ "version": 1 })).await;
        wait(&app, turn, "running").await;
        let action_id = Uuid::new_v4();
        let claimed = outgoing_drafts::claim(
            &app.db.pool,
            turn,
            id,
            &Delivery::Mail {
                account_id: account,
                action_id,
            },
        )
        .await
        .unwrap();
        if dispatched {
            let email = match &claimed.content.0 {
                else_api::app::outgoing::Content::Email(email) => email.clone(),
                _ => unreachable!(),
            };
            let started = store::begin_action(
                &app.db.pool,
                account,
                generation,
                bot,
                turn,
                action_id,
                &email.send_command(),
            )
            .await
            .unwrap();
            assert!(matches!(started, store::ActionStart::Dispatch));
        }
        wait(&app, turn, "completed").await;
        let ended = journaled(&app, &main, id).await;
        outcomes.push((id, action_id, ended["status"].as_str().unwrap().to_owned()));
        assert!(ended["result_detail"].is_string(), "{ended}");
    }
    assert_eq!(outcomes[0].2, "failed", "never recorded, so never sent");
    assert_eq!(outcomes[1].2, "uncertain", "dispatched without a receipt");
    let (id, action_id, _) = outcomes[1];
    assert!(
        store::finish_action(
            &app.db.pool,
            account,
            action_id,
            store::ActionStatus::Succeeded,
            &json!({ "action_id": action_id, "status": "succeeded" }),
        )
        .await
        .unwrap()
    );
    let status: String = sqlx::query_scalar("SELECT status FROM outgoing_drafts WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "sent", "the late receipt settles it");
    assert_eq!(
        journaled(&app, &main, id).await["status"],
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
async fn delegated_task(app: &TestApp) -> (String, Uuid, Uuid, Uuid) {
    let main = app.main_bot().await;
    let leaf = app.create_bot("Telegram").await;
    let warm = app
        .send(&leaf, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    app.wait_turn(warm.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let (root, _) = running(app, &main, SLOW, None).await;
    let parent = app
        .scheduler
        .grants()
        .issue(
            main.parse().unwrap(),
            sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
                .bind(root)
                .fetch_one(&app.db.pool)
                .await
                .unwrap(),
            root,
            vec![Tool::ReadMemory, Tool::Delegate],
            DEFAULT_TTL,
        )
        .await;
    let delegated = tool(
        app,
        &parent,
        "delegate",
        json!({ "target_bot_id": leaf, "task": SLOW }),
    )
    .await;
    let task: Uuid = tool_text(&delegated)["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("delegated: {}", delegated.body))
        .parse()
        .unwrap();
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    wait(app, child, "running").await;
    (leaf, root, task, child)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_approval_of_a_delegated_draft_answers_the_delegating_request() {
    let Harness { app, .. } = harness().await;
    let (leaf, root, task, child) = delegated_task(&app).await;
    let id = draft(&app, &grant(&app, child).await, lunch()).await;
    wait(&app, child, "waiting").await;

    let (reply, token) = running(
        &app,
        &leaf,
        "[synthetic:slow] Looks good, send it.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let takeover: Option<Uuid> =
        sqlx::query_scalar("SELECT takeover_turn_id FROM delegations WHERE id = ?1")
            .bind(task)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(
        takeover,
        Some(reply),
        "the owner's reply took over the task"
    );
    assert_eq!(status(&app, child).await, "interrupted");

    let confirmed = tool(
        &app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(
        tool_text(&confirmed)["status"],
        "approved",
        "{}",
        confirmed.body
    );
    wait(&app, reply, "completed").await;
    let outcome: String = sqlx::query_scalar("SELECT status FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(outcome, "completed");
    assert_ne!(
        status(&app, root).await,
        "waiting",
        "the delegating request continues"
    );
}
