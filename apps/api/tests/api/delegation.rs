use crate::common;

use std::time::Duration;

use axum::http::StatusCode;
use common::recorder::{GATE, GATE_AFTER_OPENING, GATE_EVERY_ATTEMPT, Harness, harness, tool};
use common::{TestApp, TestDb};
use else_api::db::turns::{ItemKind, TurnStatus};
use else_api::grants::Tool;
use serde_json::{Value, json};
use uuid::Uuid;
mod chats;
mod child_replies;
mod creation;
mod isolation;
mod joining;
mod peers;
mod results;
mod sub_tasks;
mod updates;
mod waiting;

async fn leaf(app: &TestApp, name: &str) -> String {
    let bot = app.create_bot(name).await;
    let warm = app
        .send(&bot, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    app.wait_turn(warm.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    bot
}

async fn owner_conversation(pool: &sqlx::SqlitePool, bot: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT id FROM conversations WHERE bot_id = ?1 AND kind = 'owner'")
        .bind(bot)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn turn_events(pool: &sqlx::SqlitePool, bot: Uuid, turn: Uuid) -> Vec<(String, Value)> {
    let events: Vec<(String, Value)> =
        sqlx::query_as("SELECT kind, data FROM bot_events WHERE bot_id = ?1 ORDER BY seq")
            .bind(bot)
            .fetch_all(pool)
            .await
            .unwrap();
    let turn = turn.to_string();
    events
        .into_iter()
        .filter(|(kind, data)| match kind.as_str() {
            "item.upsert" => data["item"]["turn_id"] == turn.as_str(),
            "turn.updated" => data["turn"]["id"] == turn.as_str(),
            _ => false,
        })
        .collect()
}

async fn seed(db: &TestDb) -> (Uuid, Uuid, Uuid, Uuid) {
    seed_from(db, ItemKind::User, "Find the form.").await
}

/// Seed main's running reply to a `kind` message, delegating to Target.
/// Returns (main, root, child, delegation).
async fn seed_from(db: &TestDb, kind: ItemKind, text: &str) -> (Uuid, Uuid, Uuid, Uuid) {
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let main: Uuid = sqlx::query_scalar("SELECT default_bot_id FROM installation")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let target = common::seed::bot(&db.pool, "Target").await.id;
    let root = common::seed::turn(main)
        .status(TurnStatus::Running)
        .attempt(1)
        .message(kind, text)
        .insert(&db.pool)
        .await;
    let accepted =
        else_api::db::delegations::create(&db.pool, root, target, "seeded task", None, &[], None)
            .await
            .unwrap();
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(accepted.delegation_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    (main, root, child, accepted.delegation_id)
}

async fn bot_events(pool: &sqlx::SqlitePool, bot: Uuid) -> Vec<(String, Value)> {
    sqlx::query_as("SELECT kind, data FROM bot_events WHERE bot_id = ?1 ORDER BY seq")
        .bind(bot)
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn steered_into(db: &TestDb, turn: Uuid) -> Option<Uuid> {
    sqlx::query_scalar("SELECT into_turn_id FROM turn_steers WHERE turn_id = ?1")
        .bind(turn)
        .fetch_optional(&db.pool)
        .await
        .unwrap()
}

async fn undelivered(db: &TestDb, root: Uuid) -> Vec<else_api::db::delegations::Outcome> {
    let mut conn = db.pool.acquire().await.unwrap();
    else_api::db::delegations::undelivered(&mut conn, root)
        .await
        .unwrap()
}

/// A task to `target` from the main bot's running turn, whose waiting
/// reply a second owner message joins and continues with a hang, as when
/// the owner follows up while the task runs. Returns the task, its running
/// child turn, and a delegate grant for the continued reply.
async fn task_carried_to_a_follow_up(
    h: &Harness,
    target: &str,
    task: &str,
) -> (Uuid, Uuid, String) {
    let app = &h.app;
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE).await;
    let reply = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": target, "task": task }),
    )
    .await;
    let task_id: Uuid = reply.result["task_id"].as_str().unwrap().parse().unwrap();
    h.release(root).await;
    app.wait_turn(root, &["waiting"]).await;
    let child = child_of(app, task_id).await;
    h.started(child).await;
    let follow = app
        .send(
            &main,
            Uuid::new_v4(),
            "[synthetic:hang] Any news?",
            Some("synthetic"),
        )
        .await;
    assert_eq!(
        follow.body["turn"]["steered_into"],
        root.to_string(),
        "{}",
        follow.body
    );
    (task_id, child, h.continued(root, &token).await)
}

async fn child_of(app: &TestApp, task: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn wait_task(
    app: &TestApp,
    task: Uuid,
    status: &str,
) -> (String, Option<String>, Option<String>) {
    let deadline = tokio::time::Instant::now() + common::SETTLE;
    loop {
        let row: (String, Option<String>, Option<String>) =
            sqlx::query_as("SELECT status, result, error FROM delegations WHERE id = ?1")
                .bind(task)
                .fetch_one(&app.db.pool)
                .await
                .unwrap();
        if row.0 == status {
            return row;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "task {task} never reached {status}: {row:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
