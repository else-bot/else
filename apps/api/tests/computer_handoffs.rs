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

async fn harness() -> (TestApp, Arc<Synthetic>) {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let synthetic = Arc::new(Synthetic::new(Duration::from_millis(5)));
    let registry = else_api::providers::Registry::new(vec![synthetic.clone()]);
    (TestApp::with_registry(db, 4, registry), synthetic)
}

async fn app() -> TestApp {
    harness().await.0
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

async fn turn_status(app: &TestApp, turn: Uuid) -> String {
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

#[tokio::test(flavor = "multi_thread")]
async fn a_handoff_parks_the_turn_until_the_owner_hands_the_computer_back() {
    let (app, synthetic) = harness().await;
    let bot = app.main_bot().await;
    let (turn, token) = running(&app, &bot).await;

    let asked = hand_off(&app, &token, "X wants a 2FA code at x.com/login.").await;
    assert_eq!(asked["status"], "pending", "{asked}");
    assert_eq!(asked["parked"], true, "{asked}");
    let again = hand_off(&app, &token, "Log in to X again.").await;
    assert!(
        again
            .as_str()
            .is_some_and(|t| t.starts_with("ATTEMPT_PARKED")),
        "a parked attempt asks nothing more: {again}"
    );
    wait(&app, turn, "waiting").await;

    let taken = settle(&app, &bot, &asked["handoff_id"], "take").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
    assert_eq!(taken.body["handoff"]["status"], "taken");
    assert!(
        taken.body["handoff"]["taken_at"].is_string(),
        "{}",
        taken.body
    );
    assert_eq!(
        turn_status(&app, turn).await,
        "waiting",
        "taking over does not continue the bot"
    );
    let retried = settle(&app, &bot, &asked["handoff_id"], "take").await;
    assert_eq!(
        retried.status,
        StatusCode::OK,
        "taking over twice is a no-op"
    );
    let declined = settle(&app, &bot, &asked["handoff_id"], "dismiss").await;
    assert_eq!(
        declined.status,
        StatusCode::CONFLICT,
        "a taken request is only handed back: {}",
        declined.body
    );
    assert_eq!(declined.body["error"]["code"], "computer_handoff_settled");

    let back = settle(&app, &bot, &asked["handoff_id"], "hand-back").await;
    assert_eq!(back.status, StatusCode::OK, "{}", back.body);
    assert_eq!(back.body["handoff"]["status"], "handed_back");
    wait(&app, turn, "completed").await;
    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(packet.contains("[else.bot take-over results]"), "{packet}");
    assert!(
        packet.contains("result=\"handed_back: the owner took over and handed it back\""),
        "{packet}"
    );
    assert!(
        packet.contains("X wants a 2FA code at x.com/login."),
        "{packet}"
    );

    let twice = settle(&app, &bot, &asked["handoff_id"], "hand-back").await;
    assert_eq!(
        twice.status,
        StatusCode::OK,
        "handing back twice is a no-op"
    );
    assert_eq!(status(&app, &asked["handoff_id"]).await, "handed_back");
}

#[tokio::test(flavor = "multi_thread")]
async fn declining_a_handoff_continues_the_turn_and_it_cannot_be_handed_back() {
    let (app, synthetic) = harness().await;
    let bot = app.main_bot().await;
    let (turn, token) = running(&app, &bot).await;
    let asked = hand_off(&app, &token, "A captcha blocks the login page.").await;
    wait(&app, turn, "waiting").await;

    let early = settle(&app, &bot, &asked["handoff_id"], "hand-back").await;
    assert_eq!(
        early.status,
        StatusCode::CONFLICT,
        "nothing to hand back before taking over: {}",
        early.body
    );
    let declined = settle(&app, &bot, &asked["handoff_id"], "dismiss").await;
    assert_eq!(declined.status, StatusCode::OK, "{}", declined.body);
    assert_eq!(declined.body["handoff"]["status"], "dismissed");
    wait(&app, turn, "completed").await;
    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(packet.contains("result=\"declined:"), "{packet}");
    let late = settle(&app, &bot, &asked["handoff_id"], "take").await;
    assert_eq!(late.status, StatusCode::CONFLICT, "{}", late.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_message_withdraws_a_pending_handoff() {
    let (app, _) = harness().await;
    let bot = app.main_bot().await;
    let (turn, token) = running(&app, &bot).await;
    let handoff = hand_off(&app, &token, "A captcha blocks the login page.").await;
    assert_eq!(handoff["status"], "pending", "{handoff}");
    wait(&app, turn, "waiting").await;

    let sent = app
        .send(&bot, Uuid::new_v4(), "Skip X for today.", Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(status(&app, &handoff["handoff_id"]).await, "skipped");
    let later = settle(&app, &bot, &handoff["handoff_id"], "dismiss").await;
    assert_eq!(later.status, StatusCode::CONFLICT, "{}", later.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn chatting_while_in_control_keeps_the_computer_with_the_owner() {
    let (app, synthetic) = harness().await;
    let bot = app.main_bot().await;
    let (turn, token) = running(&app, &bot).await;
    let asked = hand_off(&app, &token, "Log in to X as @scout.").await;
    wait(&app, turn, "waiting").await;
    let taken = settle(&app, &bot, &asked["handoff_id"], "take").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);

    let sent = app
        .send(
            &bot,
            Uuid::new_v4(),
            "Which account was it?",
            Some("synthetic"),
        )
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let next: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    wait(&app, next, "completed").await;
    assert_eq!(
        status(&app, &asked["handoff_id"]).await,
        "taken",
        "a message is not a hand back"
    );
    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(packet.contains("[else.bot computer control]"), "{packet}");
    assert!(packet.contains("They have not handed it back"), "{packet}");

    let (_, token) = running(&app, &bot).await;
    let refused = hand_off(&app, &token, "Solve the captcha.").await;
    assert!(
        refused
            .as_str()
            .is_some_and(|t| t.starts_with("OWNER_IN_CONTROL")),
        "{refused}"
    );
    let back = settle(&app, &bot, &asked["handoff_id"], "hand-back").await;
    assert_eq!(back.status, StatusCode::OK, "{}", back.body);
    assert_eq!(
        turn_status(&app, turn).await,
        "interrupted",
        "the newer message superseded the waiting turn"
    );
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
async fn a_taken_handoff_stays_in_the_snapshot_after_its_turn_leaves_the_page() {
    let app = app().await;
    let bot = app.main_bot().await;
    let (turn, token) = running(&app, &bot).await;
    let handoff = hand_off(&app, &token, "X Chat is blank at x.com/messages.").await;
    wait(&app, turn, "waiting").await;
    let taken = settle(&app, &bot, &handoff["handoff_id"], "take").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
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
    assert_eq!(status(&app, &handoff["handoff_id"]).await, "taken");

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
    assert_eq!(shown["computer_handoffs"][0]["status"], "taken");
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_turn_withdraws_a_handoff_the_owner_has_not_taken() {
    let app = app().await;
    let bot = app.main_bot().await;
    let (turn, token) = running(&app, &bot).await;
    let handoff = hand_off(&app, &token, "X Chat is blank at x.com/messages.").await;
    wait(&app, turn, "waiting").await;
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
    assert_eq!(status(&app, &handoff["handoff_id"]).await, "cancelled");
    let late = settle(&app, &bot, &handoff["handoff_id"], "take").await;
    assert_eq!(late.status, StatusCode::CONFLICT, "{}", late.body);
}

async fn sidebar_status(app: &TestApp, bot: &str) -> Value {
    let list = app.owner("GET", "/v1/bots", None).await;
    list.body["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == bot)
        .unwrap()["sidebar_status"]
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_handoff_shows_in_the_sidebar_until_it_is_settled() {
    let app = app().await;
    let bot = app.main_bot().await;
    let (_, token) = running(&app, &bot).await;
    let handoff = hand_off(&app, &token, "Inspect the open X conversation.").await;

    let shown = sidebar_status(&app, &bot).await;
    assert_eq!(shown["kind"], "needs_takeover", "{shown}");
    assert_eq!(shown["preview"], "Inspect the open X conversation.");
    assert!(
        shown["since"]
            .as_str()
            .is_some_and(|at| at.parse::<chrono::DateTime<chrono::Utc>>().is_ok()),
        "{shown}"
    );

    let dismissed = settle(&app, &bot, &handoff["handoff_id"], "dismiss").await;
    assert_eq!(dismissed.status, StatusCode::OK, "{}", dismissed.body);
    assert_ne!(sidebar_status(&app, &bot).await["kind"], "needs_takeover");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_from_before_hand_back_still_tells_later_turns_who_holds_the_computer() {
    let (app, synthetic) = harness().await;
    let bot = app.main_bot().await;
    let (turn, _) = running(&app, &bot).await;
    let bot_id: Uuid = bot.parse().unwrap();
    let legacy = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO computer_handoffs (id, bot_id, turn_id, attempt, reason, delivered_attempt)
         VALUES (?1, ?2, ?3, 0, 'Log in to X as @scout.', 0)",
    )
    .bind(legacy)
    .bind(bot_id)
    .bind(turn)
    .execute(&app.db.pool)
    .await
    .unwrap();
    let id = json!(legacy.to_string());
    let taken = settle(&app, &bot, &id, "take").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);

    let sent = app
        .send(&bot, Uuid::new_v4(), "Any replies yet?", Some("synthetic"))
        .await;
    app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(packet.contains("They have not handed it back"), "{packet}");
}
