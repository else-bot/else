use crate::common;

use std::time::Duration;

use axum::http::StatusCode;
use chrono::{DateTime, SubsecRound, Utc};
use common::recorder::{GATE, GATE_EVERY_ATTEMPT, Harness, harness, tool};
use common::{TestApp, TestDb};
use else_api::grants::Tool;
use serde_json::{Value, json};
use uuid::Uuid;
mod bot_tools;
mod editing;
mod failures;
mod migrations;
mod runs;
mod scheduling;

const YEARLY: &str = "0 9 1 1 *";

fn digest(prompt: &str) -> Value {
    json!({ "name": "Morning digest", "prompt": prompt, "cron": YEARLY })
}

async fn create(app: &TestApp, token: &str, args: Value) -> Value {
    let called = tool(app, token, "routine_create", args).await;
    assert!(!called.error, "{}", called.body);
    called.result["routine"].clone()
}

fn at(value: &Value) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value.as_str().unwrap())
        .unwrap()
        .to_utc()
}

async fn fire(app: &TestApp, now: DateTime<Utc>) -> usize {
    let queued = else_api::db::routines::admit_due(&app.db.pool, &app.state.registry, None, now, 8)
        .await
        .unwrap()
        .queued;
    app.scheduler.wake();
    queued
}

async fn runs(app: &TestApp, routine: &str) -> Vec<(Option<Uuid>, Option<String>, String)> {
    sqlx::query_as(
        "SELECT turn_id, skip_reason, name FROM routine_runs WHERE routine_id = ?1 ORDER BY due_at",
    )
    .bind(routine.parse::<Uuid>().unwrap())
    .fetch_all(&app.db.pool)
    .await
    .unwrap()
}

async fn owner_routines(app: &TestApp, bot: &str) -> Vec<Value> {
    let reply = app
        .owner("GET", &format!("/v1/bots/{bot}/routines"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.body["routines"].as_array().unwrap().clone()
}

async fn set_enabled(app: &TestApp, bot: &str, routine: &str, enabled: bool) -> common::Reply {
    app.owner(
        "PATCH",
        &format!("/v1/bots/{bot}/routines/{routine}"),
        Some(json!({ "enabled": enabled })),
    )
    .await
}

async fn events(app: &TestApp, bot: &str) -> Vec<(String, Value)> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT kind, data FROM bot_events WHERE bot_id = ?1 ORDER BY seq")
            .bind(bot.parse::<Uuid>().unwrap())
            .fetch_all(&app.db.pool)
            .await
            .unwrap();
    rows.into_iter()
        .map(|(kind, data)| (kind, serde_json::from_str(&data).unwrap()))
        .collect()
}

async fn retry(app: &TestApp, bot: &str, routine: &str, request_id: Uuid) -> common::Reply {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/routines/{routine}/retry"),
        Some(json!({ "request_id": request_id })),
    )
    .await
}
