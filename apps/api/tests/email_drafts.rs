//! Email drafts: a bot drafts an email over MCP, the owner edits and
//! approves or discards it on its card or answers in chat, and the bot sends
//! the exact approved version once through its existing tools and reports
//! the result. Stale versions, replays, and other requests approve nothing.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::{TestApp, TestDb};
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
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let synthetic = Arc::new(Synthetic::new(Duration::from_millis(5)));
    let registry = else_api::providers::Registry::new(vec![synthetic.clone()]);
    Harness {
        app: TestApp::with_registry(db, 4, registry),
        synthetic,
    }
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
            vec![Tool::ReadMemory, Tool::AskOwner, Tool::Computer],
            DEFAULT_TTL,
        )
        .await
}

async fn send(app: &TestApp, bot: &str, text: &str, draft: Option<Value>) -> common::Reply {
    let mut body =
        json!({ "request_id": Uuid::new_v4(), "text": text, "provider_id": "synthetic" });
    if let Some(draft) = draft {
        body["email_draft"] = draft;
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

async fn draft(app: &TestApp, token: &str, args: Value) -> Uuid {
    let drafted = tool(app, token, "draft_email", args).await;
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
        "edit" => ("PUT", format!("/v1/bots/{bot}/email-drafts/{draft}")),
        _ => (
            "POST",
            format!("/v1/bots/{bot}/email-drafts/{draft}/{action}"),
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
        .expect("the turn is in the snapshot")["email_drafts"]
        .as_array()
        .unwrap()
        .clone()
}

/// The latest journaled state of a draft, as a reconnecting app replays it.
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
            data["turn"]["email_drafts"]
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
    json!({ "command": "python3 send_mail.py", "effect": "external", "email_draft": draft })
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
    assert_eq!(card["from"], "main@bots.example");
    assert_eq!(card["to"], json!(["ada@example.com"]));
    assert_eq!(card["cc"], json!(["bob@example.com"]));
    assert_eq!(card["bcc"], json!([]));
    assert_eq!(card["subject"], "Lunch on Friday?");
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
    assert!(packet.contains("[else.bot email drafts]"), "{packet}");
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
        json!({ "command": "true", "effect": "local", "email_draft": id }),
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
        "report_email_send",
        json!({ "draft_id": id, "status": "sent", "detail": "250 OK id=abc" }),
    )
    .await;
    assert_eq!(tool_text(&sent)["status"], "sent", "{}", sent.body);
    let replayed = tool(
        &app,
        &token,
        "report_email_send",
        json!({ "draft_id": id, "status": "sent" }),
    )
    .await;
    assert_eq!(tool_text(&replayed)["status"], "sent", "{}", replayed.body);
    let contradicted = tool(
        &app,
        &token,
        "report_email_send",
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
        "start_email_send",
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
        "report_email_send",
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
    assert_eq!(old["body"], "Hi Ada, lunch Friday? My treat.");
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
    assert_eq!(drafted["draft"]["email_id"], old["email_id"]);

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
        "confirm_email_approval",
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
        "confirm_email_approval",
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

    let started = tool(&app, &token, "start_email_send", json!({ "draft_id": id })).await;
    let started = tool_text(&started);
    assert_eq!(started["status"], "sending", "{started}");
    assert_eq!(started["draft"]["subject"], "Lunch on Friday?");
    let twice = tool(&app, &token, "start_email_send", json!({ "draft_id": id })).await;
    assert!(refusal(&twice).starts_with("DRAFT_SETTLED"));
    let uncertain = tool(
        &app,
        &token,
        "report_email_send",
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
        "confirm_email_approval",
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
        "confirm_email_approval",
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
        "confirm_email_approval",
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
        "confirm_email_approval",
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
        "report_email_send",
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
