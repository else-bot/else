use crate::common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::recorder::{self, GATE};
use common::{TestApp, TestDb};
use else_api::db::turns::{ItemKind, TurnStatus};
use serde_json::json;
use uuid::Uuid;

mod outcomes;
mod profiles;
mod reactions;
mod scheduling;
mod sends;

async fn items(app: &TestApp, bot: &str) -> Vec<(i64, String, String)> {
    sqlx::query_as(
        "SELECT i.seq, i.kind, i.text FROM items i JOIN conversations c ON c.id = i.conversation_id
         WHERE c.bot_id = ?1 ORDER BY i.seq",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_all(&app.db.pool)
    .await
    .unwrap()
}

async fn turn_count(app: &TestApp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM turns")
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}
