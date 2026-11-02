//! POC-016: one-hop asynchronous delegation through the real router,
//! scheduler, and PostgreSQL, with the synthetic provider.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::{TestApp, TestDb};
use else_api::grants::{DEFAULT_TTL, Tool};
use serde_json::{Value, json};
use uuid::Uuid;

/// Long enough (about six seconds) to call tools while it streams.
const SLOW: &str = "[synthetic:slow] please plan the week: groceries, school forms, the work \
                    review, and the dentist; ask the right bot for the school part of this.";

async fn app_with(max_active: usize) -> TestApp {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    TestApp::with_options(db, max_active)
}

/// A bot that answers with the synthetic provider (its selected provider).
async fn leaf(app: &TestApp, name: &str, role: &str) -> String {
    let bot = app.create_bot(name, role).await;
    let warm = app
        .send(&bot, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    app.wait_turn(warm.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    bot
}

/// Start a slow main-bot turn and return it with a grant for its attempt.
async fn running_main(app: &TestApp) -> (String, Uuid, String) {
    let main = app.main_bot().await;
    let sent = app
        .send(&main, Uuid::new_v4(), SLOW, Some("synthetic"))
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["running"]).await;
    let turn_id: Uuid = turn.parse().unwrap();
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = $1")
        .bind(turn_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let token = app.scheduler.grants().issue(
        main.parse().unwrap(),
        conversation,
        turn_id,
        vec![Tool::ReadHistory, Tool::ReadMemory, Tool::Delegate],
        DEFAULT_TTL,
    );
    (main, turn_id, token)
}

async fn delegate(app: &TestApp, token: &str, target: &str, task: &str) -> common::Reply {
    app.call(
        "POST",
        "/tools/v1/mcp",
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                     "params": { "name": "delegate",
                                 "arguments": { "target_bot_id": target, "task": task } } })),
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

async fn wait_status(app: &TestApp, turn: Uuid, status: &str) {
    app.wait_turn_for(&turn.to_string(), &[status], Duration::from_secs(20))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_child_parks_the_parent_and_a_continuation_uses_its_result() {
    // One provider slot: the parent must release it for the child.
    let app = app_with(1).await;
    let school = leaf(&app, "School", "School forms, trips, and teachers").await;
    let (main, root, token) = running_main(&app).await;

    let reply = delegate(
        &app,
        &token,
        &school,
        "Collect the planetarium form details.",
    )
    .await;
    let pending = tool_text(&reply);
    assert_eq!(pending["status"], "pending", "{}", reply.body);
    let task_id: Uuid = pending["task_id"].as_str().unwrap().parse().unwrap();
    // A retried call returns the same child; further calls are refused.
    let again = delegate(
        &app,
        &token,
        &school,
        "Collect the planetarium form details.",
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT, "the attempt is parked");

    // The parent ends its attempt and waits without holding the slot; the
    // child runs, then the parent continues and completes.
    app.wait_turn_for(&root.to_string(), &["completed"], Duration::from_secs(40))
        .await;
    let (status, result, delivered, child): (String, Option<String>, Option<i32>, Uuid) =
        sqlx::query_as(
            "SELECT status, result, delivered_attempt, child_turn_id FROM delegations WHERE id = $1",
        )
        .bind(task_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "completed");
    assert!(result.unwrap().contains("planetarium form"));
    assert_eq!(delivered, Some(2), "the second attempt received it");
    let attempts: i32 = sqlx::query_scalar("SELECT attempt FROM turns WHERE id = $1")
        .bind(root)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(attempts, 2);
    let children: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delegations WHERE root_turn_id = $1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(children, 1, "retries created no second child");

    // The child stayed out of the target's owner conversation.
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{school}/conversation"), None)
        .await
        .body;
    let texts: Vec<&str> = snapshot["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["text"].as_str().unwrap())
        .collect();
    assert!(
        !texts.iter().any(|t| t.contains("planetarium")),
        "{texts:?}"
    );
    let child_conversation: String = sqlx::query_scalar(
        "SELECT c.kind FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE t.id = $1",
    )
    .bind(child)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(child_conversation, "task");
    // Memory: the task is model-authored in the target; the result is tool
    // activity in the parent.
    let task_entry: (String, String) = sqlx::query_as(
        "SELECT kind, authority FROM memory_entry WHERE bot_id = $1::uuid AND body LIKE '%planetarium form details%'
         ORDER BY seq LIMIT 1",
    )
    .bind(&school)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(task_entry, ("delegated_task".into(), "assistant".into()));
    let parent_entry: String = sqlx::query_scalar(
        "SELECT authority FROM memory_entry WHERE bot_id = $1::uuid AND request_key = $2",
    )
    .bind(&main)
    .bind(format!("delegation:{task_id}"))
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(parent_entry, "tool");
}

#[tokio::test(flavor = "multi_thread")]
async fn delegation_limits_fail_in_code() {
    let app = app_with(2).await;
    let work = leaf(&app, "Work", "Work projects").await;
    let family = leaf(&app, "Family", "Family logistics").await;
    let gone = leaf(&app, "Gone", "Deleted soon").await;
    app.owner("DELETE", &format!("/v1/bots/{gone}"), None).await;
    let (main, root, token) = running_main(&app).await;

    let error = |reply: &common::Reply| {
        reply.body["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    assert!(
        error(&delegate(&app, &token, &main, "do it yourself").await)
            .starts_with("SELF_DELEGATION")
    );
    assert!(error(&delegate(&app, &token, &gone, "anything").await).starts_with("BOT_UNAVAILABLE"));
    assert!(
        error(&delegate(&app, &token, &Uuid::new_v4().to_string(), "anything").await)
            .starts_with("BOT_UNAVAILABLE")
    );
    // A grant without the delegate tool (any leaf or other bot) is refused.
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = $1")
        .bind(root)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let no_delegate = app.scheduler.grants().issue(
        main.parse().unwrap(),
        conversation,
        root,
        vec![Tool::ReadMemory],
        DEFAULT_TTL,
    );
    assert_eq!(
        delegate(&app, &no_delegate, &work, "x").await.status,
        StatusCode::FORBIDDEN
    );
    // Budget: two children per request, one per attempt.
    let first = tool_text(&delegate(&app, &token, &work, "first task").await);
    assert_eq!(first["status"], "pending");
    app.wait_turn_for(
        &root.to_string(),
        &["waiting", "queued", "preparing", "running", "completed"],
        Duration::from_secs(20),
    )
    .await;
    // The database rules hold even without the grant check: a second child
    // in the same attempt is refused (or the attempt has already ended).
    let second = else_api::db::delegations::create(
        &app.db.pool,
        root,
        family.parse().unwrap(),
        "second in the same attempt",
    )
    .await;
    assert!(
        matches!(
            second,
            Err(else_api::db::delegations::DelegateError::AttemptParked)
                | Err(else_api::db::delegations::DelegateError::NotRunning)
        ),
        "{second:?}"
    );
    // A leaf (a child turn) cannot delegate even with a forged grant.
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = $1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    for _ in 0..200 {
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = $1")
            .bind(child)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
        if status == "running" || status == "completed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let leaf_result = else_api::db::delegations::create(
        &app.db.pool,
        child,
        family.parse().unwrap(),
        "loop back",
    )
    .await;
    assert!(
        matches!(
            leaf_result,
            Err(else_api::db::delegations::DelegateError::NotAllowed)
                | Err(else_api::db::delegations::DelegateError::NotRunning)
        ),
        "{leaf_result:?}"
    );
    wait_status(&app, root, "completed").await;
    let children: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delegations WHERE root_turn_id = $1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(children, 1);

    // A request whose two children are used up refuses a third, whatever
    // the attempt: simulate a third attempt that is running.
    let (_, root, _) = running_main(&app).await;
    for (n, target) in [&work, &family].iter().enumerate() {
        sqlx::query("UPDATE turns SET attempt = $2 WHERE id = $1")
            .bind(root)
            .bind(n as i32 + 1)
            .execute(&app.db.pool)
            .await
            .unwrap();
        else_api::db::delegations::create(
            &app.db.pool,
            root,
            target.parse().unwrap(),
            &format!("task {n}"),
        )
        .await
        .unwrap();
    }
    sqlx::query("UPDATE turns SET attempt = 3 WHERE id = $1")
        .bind(root)
        .execute(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        else_api::db::delegations::create(&app.db.pool, root, work.parse().unwrap(), "a third")
            .await,
        Err(else_api::db::delegations::DelegateError::BudgetExhausted)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_root_and_deleting_the_target_resolve_child_work() {
    let app = app_with(2).await;
    let work = leaf(&app, "Work", "Work projects").await;
    // The child hangs until stopped.
    let (main, root, token) = running_main(&app).await;
    let reply = delegate(&app, &token, &work, "[synthetic:hang] long research").await;
    let task_id: Uuid = tool_text(&reply)["task_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    wait_status(&app, root, "waiting").await;
    let view = app.owner("GET", "/v1/bots", None).await.body;
    let main_view = view["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == main.as_str())
        .unwrap()
        .clone();
    assert_eq!(main_view["activity"]["phase"], "working", "{main_view}");
    // Stop work on the main bot ends the root and its pending child.
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    wait_status(&app, root, "interrupted").await;
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = $1")
        .bind(task_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    app.wait_turn_for(
        &child.to_string(),
        &["interrupted"],
        Duration::from_secs(20),
    )
    .await;
    let status: String = sqlx::query_scalar("SELECT status FROM delegations WHERE id = $1")
        .bind(task_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "cancelled");

    // Deleting the target of a pending child gives the parent a recorded
    // failure to continue with.
    let (_, root, token) = running_main(&app).await;
    let reply = delegate(&app, &token, &work, "[synthetic:hang] another long task").await;
    let task_id: Uuid = tool_text(&reply)["task_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    wait_status(&app, root, "waiting").await;
    app.owner("DELETE", &format!("/v1/bots/{work}"), None).await;
    wait_status(&app, root, "completed").await;
    let (status, error, delivered): (String, Option<String>, Option<i32>) =
        sqlx::query_as("SELECT status, error, delivered_attempt FROM delegations WHERE id = $1")
            .bind(task_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(status, "cancelled");
    assert!(error.is_some());
    assert_eq!(delivered, Some(2), "the continuation received the outcome");
}
