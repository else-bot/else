use crate::common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::{TestApp, TestDb};
use else_api::grants::{DEFAULT_TTL, Tool};
use serde_json::{Value, json};
use uuid::Uuid;

const SLOW: &str = "[synthetic:slow] fix the checkout errors; ask the right bot to triage them.";

async fn app() -> TestApp {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    TestApp::with_options(db, 2)
}

async fn grant(app: &TestApp, turn: Uuid, tools: Vec<Tool>) -> String {
    let (bot, conversation): (Uuid, Uuid) =
        sqlx::query_as("SELECT bot_id, conversation_id FROM turns WHERE id = ?1")
            .bind(turn)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    app.scheduler
        .grants()
        .issue(bot, conversation, turn, tools, DEFAULT_TTL)
        .await
}

async fn tool(app: &TestApp, token: &str, name: &str, arguments: Value) -> (bool, Value) {
    let reply = app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": name, "arguments": arguments } })),
            Some(token),
        )
        .await;
    let result = &reply.body["result"];
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    (
        result["isError"] == true,
        serde_json::from_str(text).unwrap_or_else(|_| json!(text)),
    )
}

/// A running task the main bot delegated to Tech Manager, with its grant.
struct Delegated {
    target: String,
    child: Uuid,
    child_token: String,
}

async fn delegated(app: &TestApp) -> Delegated {
    let target = app.create_bot("Tech Manager").await;
    let main = app.main_bot().await;
    let sent = app
        .send(&main, Uuid::new_v4(), SLOW, Some("synthetic"))
        .await;
    let root: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&root.to_string(), &["running"]).await;
    let root_token = grant(app, root, vec![Tool::Delegate, Tool::ReadHistory]).await;
    let (failed, result) = tool(
        app,
        &root_token,
        "delegate",
        json!({ "target_bot_id": target, "task": "[synthetic:hang] triage checkout 500s" }),
    )
    .await;
    assert!(!failed, "{result}");
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    app.wait_turn(&child.to_string(), &["running"]).await;
    let child_token = grant(app, child, vec![Tool::ReadHistory]).await;
    Delegated {
        target,
        child,
        child_token,
    }
}

async fn notify(app: &TestApp, token: &str, summary: &str) -> (bool, Value) {
    tool(
        app,
        token,
        "notify_owner",
        json!({ "kind": "blocked", "summary": summary,
                "tried": "Delegated to Dev; refused, no eligible session." }),
    )
    .await
}

async fn notices(app: &TestApp, bot: &str, turn: Uuid) -> Vec<Value> {
    let page = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    page.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .map(|t| t["owner_notices"].as_array().unwrap().clone())
        .unwrap_or_default()
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

async fn dismiss(app: &TestApp, bot: &str, id: &Value) -> common::Reply {
    let id = id.as_str().unwrap();
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/owner-notices/{id}/dismiss"),
        None,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_task_shows_the_owner_what_it_needs_until_it_is_handled() {
    let app = app().await;
    let task = delegated(&app).await;

    let (failed, first) = notify(&app, &task.child_token, "Dev can't start the fix.").await;
    assert!(!failed, "{first}");
    assert_eq!(first["status"], "pending");
    let (failed, again) = notify(
        &app,
        &task.child_token,
        "Dev has no eligible session, so the fix can't start. Start one or reassign it.",
    )
    .await;
    assert!(!failed, "{again}");
    assert_eq!(
        again["notice_id"], first["notice_id"],
        "a retry rewrites the notice"
    );

    let shown = notices(&app, &task.target, task.child).await;
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(shown[0]["kind"], "blocked");
    assert_eq!(shown[0]["status"], "pending");
    assert!(
        shown[0]["summary"]
            .as_str()
            .unwrap()
            .starts_with("Dev has no eligible")
    );
    assert_eq!(
        shown[0]["tried"],
        "Delegated to Dev; refused, no eligible session."
    );
    let status = sidebar_status(&app, &task.target).await;
    assert_eq!(status["kind"], "needs_you", "{status}");

    let handled = dismiss(&app, &task.target, &first["notice_id"]).await;
    assert_eq!(handled.status, StatusCode::OK, "{}", handled.body);
    assert_eq!(handled.body["notice"]["status"], "dismissed");
    let repeated = dismiss(&app, &task.target, &first["notice_id"]).await;
    assert_eq!(
        repeated.status,
        StatusCode::OK,
        "dismissing again is a no-op"
    );
    assert_eq!(repeated.body["notice"]["status"], "dismissed");
    assert_eq!(
        notices(&app, &task.target, task.child).await[0]["status"],
        "dismissed"
    );
    assert!(sidebar_status(&app, &task.target).await.is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_message_answers_the_notice_and_it_can_no_longer_be_dismissed() {
    let app = app().await;
    let task = delegated(&app).await;
    let (failed, flagged) = tool(
        &app,
        &task.child_token,
        "notify_owner",
        json!({ "kind": "decision", "summary": "Roll back the deploy or hotfix it?" }),
    )
    .await;
    assert!(!failed, "{flagged}");

    let sent = app
        .send(&task.target, Uuid::new_v4(), "hotfix it", Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let shown = notices(&app, &task.target, task.child).await;
    assert_eq!(shown[0]["status"], "replied", "{shown:?}");
    assert_eq!(shown[0]["tried"], Value::Null);

    let late = dismiss(&app, &task.target, &flagged["notice_id"]).await;
    assert_eq!(late.status, StatusCode::CONFLICT, "{}", late.body);
    assert_eq!(late.body["error"]["code"], "owner_notice_settled");
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_running_delegated_task_can_notify_the_owner() {
    let app = app().await;
    let task = delegated(&app).await;

    let solo = app.create_bot("Solo").await;
    let sent = app
        .send(&solo, Uuid::new_v4(), SLOW, Some("synthetic"))
        .await;
    let own: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&own.to_string(), &["running"]).await;
    let own_token = grant(&app, own, vec![Tool::ReadHistory]).await;
    let (failed, refused) = notify(&app, &own_token, "I need you.").await;
    assert!(failed, "{refused}");
    assert!(
        refused.as_str().unwrap().starts_with("NOT_DELEGATED"),
        "{refused}"
    );

    let (failed, invalid) = tool(
        &app,
        &task.child_token,
        "notify_owner",
        json!({ "kind": "fyi", "summary": "Done." }),
    )
    .await;
    assert!(failed);
    assert!(
        invalid.as_str().unwrap().starts_with("INVALID_REQUEST"),
        "{invalid}"
    );

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM owner_notices")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
