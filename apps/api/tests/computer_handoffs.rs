mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::{TestApp, TestDb};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::synthetic::Synthetic;
use serde_json::{Value, json};
use uuid::Uuid;

const SLOW: &str = "[synthetic:slow] check the inbox on your computer.";

async fn app() -> TestApp {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let synthetic = Arc::new(Synthetic::new(Duration::from_millis(5)));
    let registry = else_api::providers::Registry::new(vec![synthetic]);
    TestApp::with_registry(db, 4, registry)
}

async fn running(app: &TestApp, bot: &str) -> (Uuid, String) {
    let sent = app.send(bot, Uuid::new_v4(), SLOW, Some("synthetic")).await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["running"]).await;
    let turn_id: Uuid = turn.parse().unwrap();
    let (bot, conversation): (Uuid, Uuid) =
        sqlx::query_as("SELECT bot_id, conversation_id FROM turns WHERE id = ?1")
            .bind(turn_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    let token = app
        .scheduler
        .grants()
        .issue(
            bot,
            conversation,
            turn_id,
            vec![Tool::Computer],
            DEFAULT_TTL,
        )
        .await;
    (turn_id, token)
}

async fn hand_off(app: &TestApp, token: &str, reason: &str) -> Value {
    let reply = app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": "computer_handoff",
                                     "arguments": { "reason": reason } } })),
            Some(token),
        )
        .await;
    let text = reply.body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    serde_json::from_str(text).unwrap_or_else(|_| json!(text))
}

async fn status(app: &TestApp, id: &Value) -> String {
    let id: Uuid = id.as_str().unwrap().parse().unwrap();
    sqlx::query_scalar("SELECT status FROM computer_handoffs WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn settle(app: &TestApp, bot: &str, id: &Value, action: &str) -> common::Reply {
    let id = id.as_str().unwrap();
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/computer-handoffs/{id}/{action}"),
        None,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handoff_stays_pending_without_holding_the_turn_until_the_owner_acts() {
    let app = app().await;
    let bot = app.main_bot().await;
    let (turn, token) = running(&app, &bot).await;

    let first = hand_off(&app, &token, "X Chat is blank at x.com/messages.").await;
    assert_eq!(first["status"], "pending", "{first}");
    let turn_status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(turn_status, "running", "a handoff never parks the turn");

    let second = hand_off(&app, &token, "Log in to X again.").await;
    assert_eq!(
        status(&app, &first["handoff_id"]).await,
        "skipped",
        "a newer request replaces the pending one"
    );

    let taken = settle(&app, &bot, &second["handoff_id"], "take").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
    assert_eq!(taken.body["handoff"]["status"], "taken");
    let again = settle(&app, &bot, &second["handoff_id"], "take").await;
    assert_eq!(again.status, StatusCode::OK, "taking over twice is a no-op");
    let dismissed = settle(&app, &bot, &second["handoff_id"], "dismiss").await;
    assert_eq!(dismissed.status, StatusCode::CONFLICT, "{}", dismissed.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_message_withdraws_a_pending_handoff() {
    let app = app().await;
    let bot = app.main_bot().await;
    let (_, token) = running(&app, &bot).await;
    let handoff = hand_off(&app, &token, "A captcha blocks the login page.").await;
    assert_eq!(handoff["status"], "pending", "{handoff}");

    let sent = app
        .send(&bot, Uuid::new_v4(), "Skip X for today.", Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(status(&app, &handoff["handoff_id"]).await, "skipped");
    let later = settle(&app, &bot, &handoff["handoff_id"], "dismiss").await;
    assert_eq!(later.status, StatusCode::CONFLICT, "{}", later.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handoff_needs_a_short_reason() {
    let app = app().await;
    let bot = app.main_bot().await;
    let (_, token) = running(&app, &bot).await;
    let reply = hand_off(&app, &token, &"x".repeat(201)).await;
    assert!(
        reply
            .as_str()
            .is_some_and(|t| t.starts_with("INVALID_REQUEST")),
        "{reply}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_handoff_stays_in_the_snapshot_after_its_turn_leaves_the_page() {
    let app = app().await;
    let bot = app.main_bot().await;
    let (turn, token) = running(&app, &bot).await;
    let handoff = hand_off(&app, &token, "X Chat is blank at x.com/messages.").await;
    let request: Uuid = sqlx::query_scalar("SELECT request_id FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let stopped = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": request })),
        )
        .await;
    assert!(stopped.status.is_success(), "{}", stopped.body);
    app.wait_turn(&turn.to_string(), &["interrupted"]).await;
    assert_eq!(status(&app, &handoff["handoff_id"]).await, "pending");

    let first: i64 = sqlx::query_scalar("SELECT min(seq) FROM items WHERE turn_id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let page = app
        .owner(
            "GET",
            &format!("/v1/bots/{bot}/conversation?limit=1&before_seq={first}"),
            None,
        )
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let turns = page.body["turns"].as_array().unwrap();
    let shown = turns
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .expect("the handoff's turn is in the snapshot");
    assert_eq!(shown["computer_handoffs"][0]["status"], "pending");
}
