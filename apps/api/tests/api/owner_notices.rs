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
    main: String,
    target: String,
    child: Uuid,
    child_token: String,
}

async fn delegated(app: &TestApp) -> Delegated {
    delegated_with(
        app,
        "[synthetic:hang] triage checkout 500s",
        vec![Tool::ReadHistory],
    )
    .await
}

async fn delegated_with(app: &TestApp, task: &str, tools: Vec<Tool>) -> Delegated {
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
        json!({ "target_bot_id": target, "task": task, "title": "Triage checkout errors" }),
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
    let child_token = grant(app, child, tools).await;
    Delegated {
        main,
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

    for invalid in [
        json!({ "kind": "maybe", "summary": "Done." }),
        json!({ "kind": "fyi" }),
        json!({ "kind": "decision" }),
    ] {
        let (failed, refused) = tool(&app, &task.child_token, "notify_owner", invalid).await;
        assert!(failed);
        assert!(
            refused.as_str().unwrap().starts_with("INVALID_REQUEST"),
            "{refused}"
        );
    }

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM owner_notices")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn repeating_a_handled_notice_keeps_it_handled() {
    let app = app().await;
    let task = delegated(&app).await;
    let summary = "Dev can't start the fix. Start a Dev session.";
    let (_, flagged) = notify(&app, &task.child_token, summary).await;
    let handled = dismiss(&app, &task.target, &flagged["notice_id"]).await;
    assert_eq!(handled.status, StatusCode::OK, "{}", handled.body);

    let (failed, again) = notify(&app, &task.child_token, summary).await;
    assert!(!failed, "{again}");
    assert_eq!(
        again["notice_id"], flagged["notice_id"],
        "the retry returns the handled notice"
    );
    let shown = notices(&app, &task.target, task.child).await;
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(shown[0]["status"], "dismissed");
    assert!(sidebar_status(&app, &task.target).await.is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_card_raised_after_the_notice_replaces_it() {
    let app = app().await;
    let task = delegated_with(
        &app,
        "[synthetic:slow] triage the checkout 500s: read the logs, find the failing requests, \
         compare the webhook secrets, and report what blocks the fix.",
        vec![Tool::ReadHistory, Tool::AskOwner],
    )
    .await;
    let (failed, flagged) = tool(
        &app,
        &task.child_token,
        "notify_owner",
        json!({ "kind": "decision", "summary": "Roll back the deploy or hotfix it?" }),
    )
    .await;
    assert!(!failed, "{flagged}");
    let (failed, asked) = tool(
        &app,
        &task.child_token,
        "ask_owner",
        json!({ "question": "Roll back the deploy or hotfix it?" }),
    )
    .await;
    assert!(!failed, "{asked}");
    app.wait_turn(&task.child.to_string(), &["waiting"]).await;

    let shown = notices(&app, &task.target, task.child).await;
    assert_eq!(shown[0]["status"], "superseded", "{shown:?}");
    let status = sidebar_status(&app, &task.target).await;
    assert_ne!(
        status["kind"], "needs_you",
        "the card's own status shows: {status}"
    );
    let late = dismiss(&app, &task.target, &flagged["notice_id"]).await;
    assert_eq!(late.status, StatusCode::CONFLICT, "{}", late.body);
}

async fn item_where(app: &TestApp, bot: &str, field: &str) -> Value {
    let page = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    page.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| !item[field].is_null())
        .unwrap_or_else(|| panic!("no {field} item: {}", page.body))[field]
        .clone()
}

async fn declare(app: &TestApp, token: &str, arguments: Value) -> (bool, Value) {
    tool(app, token, "notify_owner", arguments).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_declares_what_it_needs_and_both_chats_show_it() {
    let app = app().await;
    let task = delegated_with(
        &app,
        "[synthetic:slow] sort the overnight WhatsApp DMs: read each thread, decline the offers \
         we never take, draft replies for the rest, and report which ones need the owner.",
        vec![Tool::ReadHistory],
    )
    .await;

    let (failed, fyi) = declare(
        &app,
        &task.child_token,
        json!({ "kind": "fyi", "summary": "Declined MIRAY myself." }),
    )
    .await;
    assert!(!failed, "{fyi}");
    assert_eq!(fyi["status"], "recorded");
    let received = item_where(&app, &task.target, "bot_task").await;
    assert_eq!(received["title"], "Triage checkout errors");
    assert_eq!(received["owner_need"], "fyi", "{received}");
    assert_eq!(received["owner_line"], "Declined MIRAY myself.");

    let (failed, asked) = declare(
        &app,
        &task.child_token,
        json!({ "kind": "decision", "summary": "Approve the Romow DM before I send it." }),
    )
    .await;
    assert!(!failed, "{asked}");
    assert_eq!(asked["status"], "pending");

    let (failed, quiet) = declare(&app, &task.child_token, json!({ "kind": "nothing" })).await;
    assert!(failed, "a quiet declaration cannot hide a pending ask");
    assert!(
        quiet.as_str().unwrap().starts_with("ASK_PENDING"),
        "{quiet}"
    );
    let received = item_where(&app, &task.target, "bot_task").await;
    assert_eq!(received["owner_need"], "decision", "{received}");
    assert_eq!(received["owner_line"], Value::Null);

    app.wait_turn(&task.child.to_string(), &["completed"]).await;
    let reply = item_where(&app, &task.main, "bot_reply").await;
    assert_eq!(reply["title"], "Triage checkout errors");
    assert_eq!(reply["owner_need"], "decision", "{reply}");
    let asks = reply["asks"].as_array().unwrap();
    assert_eq!(asks.len(), 1, "{reply}");
    assert_eq!(asks[0]["id"], asked["notice_id"]);
    assert_eq!(asks[0]["bot_name"], "Tech Manager");
    assert_eq!(asks[0]["kind"], "decision");
    assert_eq!(asks[0]["status"], "pending");
    assert_eq!(asks[0]["summary"], "Approve the Romow DM before I send it.");

    let handled = dismiss(&app, &task.target, &asked["notice_id"]).await;
    assert_eq!(handled.status, StatusCode::OK, "{}", handled.body);
    let reply = item_where(&app, &task.main, "bot_reply").await;
    assert_eq!(
        reply["asks"][0]["status"], "dismissed",
        "the delegating chat follows the ask: {reply}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_that_declares_nothing_keeps_its_reply_quiet() {
    let app = app().await;
    let task = delegated_with(
        &app,
        "[synthetic:slow] count this week's reach on every channel, compare it with last \
         week, and report the totals without changing anything.",
        vec![Tool::ReadHistory],
    )
    .await;
    let (failed, quiet) = declare(&app, &task.child_token, json!({ "kind": "nothing" })).await;
    assert!(!failed, "{quiet}");
    assert_eq!(quiet["kind"], "nothing");
    app.wait_turn(&task.child.to_string(), &["completed"]).await;
    let reply = item_where(&app, &task.main, "bot_reply").await;
    assert_eq!(reply["owner_need"], "nothing", "{reply}");
    assert!(reply.get("asks").is_none(), "{reply}");
}

#[test]
fn a_title_is_one_short_line() {
    use else_api::db::delegations::title;
    assert_eq!(
        title(Some("  Triage\n overnight   DMs ")),
        Some("Triage overnight DMs".into())
    );
    assert_eq!(title(Some("   ")), None);
    assert_eq!(title(None), None);
    let long = title(Some(&"word ".repeat(40))).unwrap();
    assert!(long.ends_with('…'), "{long}");
    assert!(long.chars().count() <= 81, "{long}");
}

/// A second pending ask of the task's bot, so a message has two to choose
/// from. Settling reads only the bot, so any other turn will do.
async fn second_ask(app: &TestApp, task: &Delegated) -> Uuid {
    let id = Uuid::new_v4();
    let root: Uuid =
        sqlx::query_scalar("SELECT root_turn_id FROM delegations WHERE child_turn_id = ?1")
            .bind(task.child)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO owner_notices (id, bot_id, turn_id, kind, summary)
         VALUES (?1, ?2, ?3, 'decision', 'Renew the Sentry plan?')",
    )
    .bind(id)
    .bind(task.target.parse::<Uuid>().unwrap())
    .bind(root)
    .execute(&app.db.pool)
    .await
    .unwrap();
    id
}

async fn status_of(app: &TestApp, id: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM owner_notices WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn reply(
    app: &TestApp,
    bot: &str,
    request: Uuid,
    text: &str,
    notice: Option<Uuid>,
) -> common::Reply {
    let mut body = json!({ "request_id": request, "text": text, "provider_id": "synthetic" });
    if let Some(notice) = notice {
        body["answers_notice"] = json!(notice);
    }
    app.owner("POST", &format!("/v1/bots/{bot}/turns"), Some(body))
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_settles_only_the_ask_it_answers() {
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
    let first: Uuid = flagged["notice_id"].as_str().unwrap().parse().unwrap();
    let other = second_ask(&app, &task).await;

    // With two asks pending, a message without a target cannot tell which
    // it answers, so both stay.
    let plain = reply(&app, &task.target, Uuid::new_v4(), "one moment", None).await;
    assert_eq!(plain.status, StatusCode::ACCEPTED, "{}", plain.body);
    assert_eq!(status_of(&app, first).await, "pending");
    assert_eq!(status_of(&app, other).await, "pending");

    // Another bot's ask is refused, and the message is not stored.
    let foreign = reply(&app, &task.main, Uuid::new_v4(), "hotfix it", Some(first)).await;
    assert_eq!(foreign.status, StatusCode::BAD_REQUEST, "{}", foreign.body);
    assert_eq!(foreign.body["error"]["code"], "invalid_notice");
    assert_eq!(status_of(&app, first).await, "pending");

    let request = Uuid::new_v4();
    let targeted = reply(&app, &task.target, request, "hotfix it", Some(first)).await;
    assert_eq!(targeted.status, StatusCode::ACCEPTED, "{}", targeted.body);
    assert_eq!(status_of(&app, first).await, "replied");
    assert_eq!(status_of(&app, other).await, "pending");

    // A retry returns the stored message; the same ID with another target
    // is a different request.
    let retry = reply(&app, &task.target, request, "hotfix it", Some(first)).await;
    assert_eq!(retry.status, StatusCode::OK, "{}", retry.body);
    assert_eq!(retry.body["turn"]["id"], targeted.body["turn"]["id"]);
    let conflict = reply(&app, &task.target, request, "hotfix it", Some(other)).await;
    assert_eq!(conflict.status, StatusCode::CONFLICT, "{}", conflict.body);
    assert_eq!(status_of(&app, other).await, "pending");

    // Answering a settled ask still delivers the message and changes nothing.
    let late = reply(&app, &task.target, Uuid::new_v4(), "also this", Some(first)).await;
    assert_eq!(late.status, StatusCode::ACCEPTED, "{}", late.body);
    assert_eq!(status_of(&app, first).await, "replied");
    assert_eq!(status_of(&app, other).await, "pending");

    // With one ask left, a plain message answers it, as before.
    let last = reply(&app, &task.target, Uuid::new_v4(), "yes renew", None).await;
    assert_eq!(last.status, StatusCode::ACCEPTED, "{}", last.body);
    assert_eq!(status_of(&app, other).await, "replied");
}
