use crate::common;

use std::time::Duration;

use axum::http::StatusCode;
use common::TestApp;
use common::recorder::{
    Called, GATE, GATE_AFTER_OPENING, GATE_EVERY_ATTEMPT, Harness, harness, tool,
};
use else_api::grants::Tool;
use serde_json::{Value, json};
use uuid::Uuid;
mod approvals;
mod delegated;
mod questions;
mod settling;

fn choice_args() -> Value {
    json!({ "question": "Which template for the newsletter?",
            "choices": ["Plain text", "HTML digest"],
            "context": "Draft: Hello everyone, this month..." })
}

fn delete_args() -> Value {
    json!({ "command": "rm -rf ~/work/old-site", "effect": "destructive" })
}

async fn answer(app: &TestApp, bot: &str, question: Uuid, choice: &str) -> common::Reply {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/questions/{question}/answer"),
        Some(json!({ "choice": choice })),
    )
    .await
}

async fn status(app: &TestApp, turn: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn question_row(app: &TestApp, question: Uuid) -> (String, Option<String>, Option<i32>) {
    sqlx::query_as("SELECT status, answer, consumed_attempt FROM owner_questions WHERE id = ?1")
        .bind(question)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn permissions(app: &TestApp, bot: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM bot_permissions WHERE bot_id = ?1")
        .bind(bot.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn shown_turn(app: &TestApp, bot: &str, turn: Uuid) -> Value {
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .expect("the turn is in the snapshot")
        .clone()
}

async fn journaled(app: &TestApp, bot: &str, question: Uuid) -> Value {
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
            data["turn"]["questions"]
                .as_array()?
                .iter()
                .find(|q| q["id"] == question.to_string())
                .cloned()
        })
        .next_back()
        .expect("the question was journaled")
}

async fn sidebar_status(app: &TestApp, bot: &str) -> Value {
    let list = app.owner("GET", "/v1/bots", None).await;
    let status = list.body["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == bot)
        .unwrap()["sidebar_status"]
        .clone();
    let data: String = sqlx::query_scalar(
        "SELECT data FROM bot_events WHERE bot_id = ?1 AND kind = 'bot.updated' ORDER BY seq DESC LIMIT 1",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    let event: Value = serde_json::from_str(&data).unwrap();
    assert_eq!(event["bot"]["sidebar_status"], status);
    without_since(status)
}

/// The status with its request time checked and removed.
fn without_since(mut status: Value) -> Value {
    if let Some(since) = status.as_object_mut().and_then(|f| f.remove("since")) {
        assert!(
            since
                .as_str()
                .is_some_and(|at| at.parse::<chrono::DateTime<chrono::Utc>>().is_ok()),
            "{since}"
        );
    }
    status
}

async fn select(app: &TestApp, bot: &str, question: Uuid, choices: Value) -> common::Reply {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/questions/{question}/answer"),
        Some(json!({ "choices": choices })),
    )
    .await
}

async fn attempts(app: &TestApp, turn: Uuid) -> i32 {
    sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

/// Send a follow-up while `turn` waits on a card: it joins the reply,
/// skips the card, and the reply continues. Returns a grant for the
/// continuation.
async fn follow(h: &Harness, bot: &str, turn: Uuid, token: &str) -> String {
    let app = &h.app;
    let sent = app
        .send(bot, Uuid::new_v4(), "Try the next one.", Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["steered_into"], turn.to_string());
    h.continued(turn, token).await
}
