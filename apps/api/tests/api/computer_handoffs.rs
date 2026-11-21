use crate::common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::guest::Guest;
use common::recorder::{GATE, GATE_EVERY_ATTEMPT, Harness, harness, tool};
use common::{TestApp, TestDb};
use else_api::computers::{ComputerConfig, ComputerManager};
use serde_json::{Value, json};
use uuid::Uuid;

async fn with_computer() -> Harness {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        Arc::new(Guest::new()),
        ComputerConfig::default(),
    ));
    manager.reconcile_startup().await.unwrap();
    harness(TestApp::builder().db(db).computers(manager)).await
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

#[tokio::test(flavor = "multi_thread")]
async fn a_handoff_parks_the_turn_until_the_owner_hands_the_computer_back() {
    let h = with_computer().await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;

    let asked = tool(
        app,
        &token,
        "computer_handoff",
        json!({ "reason": "X wants a 2FA code at x.com/login." }),
    )
    .await
    .result;
    assert_eq!(asked["status"], "pending", "{asked}");
    assert_eq!(asked["parked"], true, "{asked}");
    let again = tool(app, &token, "computer_screenshot", json!({})).await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.body);
    assert_eq!(
        again.body["error"]["code"], "attempt_parked",
        "a parked attempt runs no more tools"
    );
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;

    let taken = settle(app, &bot, &asked["handoff_id"], "take").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);
    assert_eq!(taken.body["handoff"]["status"], "taken");
    assert!(
        taken.body["handoff"]["taken_at"].is_string(),
        "{}",
        taken.body
    );
    assert_eq!(
        turn_status(app, turn).await,
        "waiting",
        "taking over does not continue the bot"
    );
    let retried = settle(app, &bot, &asked["handoff_id"], "take").await;
    assert_eq!(
        retried.status,
        StatusCode::OK,
        "taking over twice is a no-op"
    );
    let declined = settle(app, &bot, &asked["handoff_id"], "dismiss").await;
    assert_eq!(
        declined.status,
        StatusCode::CONFLICT,
        "a taken request is only handed back: {}",
        declined.body
    );
    assert_eq!(declined.body["error"]["code"], "computer_handoff_settled");

    let back = settle(app, &bot, &asked["handoff_id"], "hand-back").await;
    assert_eq!(back.status, StatusCode::OK, "{}", back.body);
    assert_eq!(back.body["handoff"]["status"], "handed_back");
    app.wait_turn(turn, &["completed"]).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("[else.bot take-over results]"), "{packet}");
    assert!(
        packet.contains("result=\"handed_back: the owner took over and handed it back\""),
        "{packet}"
    );
    assert!(
        packet.contains("X wants a 2FA code at x.com/login."),
        "{packet}"
    );

    let twice = settle(app, &bot, &asked["handoff_id"], "hand-back").await;
    assert_eq!(
        twice.status,
        StatusCode::OK,
        "handing back twice is a no-op"
    );
    assert_eq!(status(app, &asked["handoff_id"]).await, "handed_back");
}

#[tokio::test(flavor = "multi_thread")]
async fn declining_a_handoff_continues_the_turn_and_it_cannot_be_handed_back() {
    let h = with_computer().await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;
    let asked = tool(
        app,
        &token,
        "computer_handoff",
        json!({ "reason": "A captcha blocks the login page." }),
    )
    .await
    .result;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;

    let early = settle(app, &bot, &asked["handoff_id"], "hand-back").await;
    assert_eq!(
        early.status,
        StatusCode::CONFLICT,
        "nothing to hand back before taking over: {}",
        early.body
    );
    let declined = settle(app, &bot, &asked["handoff_id"], "dismiss").await;
    assert_eq!(declined.status, StatusCode::OK, "{}", declined.body);
    assert_eq!(declined.body["handoff"]["status"], "dismissed");
    app.wait_turn(turn, &["completed"]).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("result=\"declined:"), "{packet}");
    let late = settle(app, &bot, &asked["handoff_id"], "take").await;
    assert_eq!(late.status, StatusCode::CONFLICT, "{}", late.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_message_withdraws_a_pending_handoff() {
    let h = with_computer().await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;
    let handoff = tool(
        app,
        &token,
        "computer_handoff",
        json!({ "reason": "A captcha blocks the login page." }),
    )
    .await
    .result;
    assert_eq!(handoff["status"], "pending", "{handoff}");
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;

    let sent = app
        .send(&bot, Uuid::new_v4(), "Skip X for today.", Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(status(app, &handoff["handoff_id"]).await, "skipped");
    let later = settle(app, &bot, &handoff["handoff_id"], "dismiss").await;
    assert_eq!(later.status, StatusCode::CONFLICT, "{}", later.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn chatting_while_in_control_keeps_the_computer_with_the_owner() {
    let h = with_computer().await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE_EVERY_ATTEMPT).await;
    let asked = tool(
        app,
        &token,
        "computer_handoff",
        json!({ "reason": "Log in to X as @scout." }),
    )
    .await
    .result;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let taken = settle(app, &bot, &asked["handoff_id"], "take").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);

    let question = "Which account was it?";
    let sent = app
        .send(&bot, Uuid::new_v4(), question, Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["steered_into"], turn.to_string());
    let next: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    let token = h.continued(turn, &token).await;
    let refused = tool(
        app,
        &token,
        "computer_handoff",
        json!({ "reason": "Solve the captcha." }),
    )
    .await
    .result;
    assert!(
        refused
            .as_str()
            .is_some_and(|t| t.starts_with("OWNER_IN_CONTROL")),
        "{refused}"
    );
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    assert_eq!(
        status(app, &asked["handoff_id"]).await,
        "taken",
        "a message is not a hand back"
    );
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains(question), "{packet}");
    assert!(packet.contains("[else.bot computer control]"), "{packet}");
    assert!(packet.contains("They have not handed it back"), "{packet}");
    assert_eq!(turn_status(app, next).await, "queued");

    let back = settle(app, &bot, &asked["handoff_id"], "hand-back").await;
    assert_eq!(back.status, StatusCode::OK, "{}", back.body);
    h.continued(turn, &token).await;
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    app.wait_turn(next, &["completed"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handoff_needs_a_short_reason() {
    let h = with_computer().await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (_, token) = h.running(&bot, GATE).await;
    let reply = tool(
        app,
        &token,
        "computer_handoff",
        json!({ "reason": &"x".repeat(201) }),
    )
    .await
    .result;
    assert!(
        reply
            .as_str()
            .is_some_and(|t| t.starts_with("INVALID_REQUEST")),
        "{reply}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_taken_handoff_stays_in_the_snapshot_after_its_turn_leaves_the_page() {
    let h = with_computer().await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;
    let handoff = tool(
        app,
        &token,
        "computer_handoff",
        json!({ "reason": "X Chat is blank at x.com/messages." }),
    )
    .await
    .result;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let taken = settle(app, &bot, &handoff["handoff_id"], "take").await;
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
    app.wait_turn(turn, &["interrupted"]).await;
    assert_eq!(status(app, &handoff["handoff_id"]).await, "taken");

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
    let h = with_computer().await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;
    let handoff = tool(
        app,
        &token,
        "computer_handoff",
        json!({ "reason": "X Chat is blank at x.com/messages." }),
    )
    .await
    .result;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
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
    app.wait_turn(turn, &["interrupted"]).await;
    assert_eq!(status(app, &handoff["handoff_id"]).await, "cancelled");
    let late = settle(app, &bot, &handoff["handoff_id"], "take").await;
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
    let h = with_computer().await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (_, token) = h.running(&bot, GATE).await;
    let handoff = tool(
        app,
        &token,
        "computer_handoff",
        json!({ "reason": "Inspect the open X conversation." }),
    )
    .await
    .result;

    let shown = sidebar_status(app, &bot).await;
    assert_eq!(shown["kind"], "needs_takeover", "{shown}");
    assert_eq!(shown["preview"], "Inspect the open X conversation.");
    assert!(
        shown["since"]
            .as_str()
            .is_some_and(|at| at.parse::<chrono::DateTime<chrono::Utc>>().is_ok()),
        "{shown}"
    );

    let dismissed = settle(app, &bot, &handoff["handoff_id"], "dismiss").await;
    assert_eq!(dismissed.status, StatusCode::OK, "{}", dismissed.body);
    assert_ne!(sidebar_status(app, &bot).await["kind"], "needs_takeover");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_taken_handoff_shows_the_owner_has_the_computer_until_hand_back() {
    let h = with_computer().await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;
    let handoff = tool(
        app,
        &token,
        "computer_handoff",
        json!({ "reason": "Log in to X as @scout." }),
    )
    .await
    .result;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let taken = settle(app, &bot, &handoff["handoff_id"], "take").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);

    let shown = sidebar_status(app, &bot).await;
    assert_eq!(shown["kind"], "has_computer", "{shown}");
    assert!(
        shown["since"]
            .as_str()
            .is_some_and(|at| at.parse::<chrono::DateTime<chrono::Utc>>().is_ok()),
        "{shown}"
    );

    let back = settle(app, &bot, &handoff["handoff_id"], "hand-back").await;
    assert_eq!(back.status, StatusCode::OK, "{}", back.body);
    assert_ne!(sidebar_status(app, &bot).await["kind"], "has_computer");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_from_before_hand_back_still_tells_later_turns_who_holds_the_computer() {
    let h = with_computer().await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, _) = h.running(&bot, GATE).await;
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
    let taken = settle(app, &bot, &id, "take").await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);

    h.release(turn).await;
    let sent = app
        .send(&bot, Uuid::new_v4(), "Any replies yet?", Some("synthetic"))
        .await;
    app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("They have not handed it back"), "{packet}");
}
