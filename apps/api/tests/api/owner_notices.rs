use crate::common;

use axum::http::StatusCode;
use common::TestApp;
use common::recorder::{GATE, Harness, harness, tool};
use serde_json::{Value, json};
use uuid::Uuid;

/// A running task the main bot delegated to Tech Manager, with its grant.
struct Delegated {
    main: String,
    target: String,
    child: Uuid,
    child_token: String,
}

async fn delegated(h: &Harness, task: &str) -> Delegated {
    let app = &h.app;
    let target = app.create_bot("Tech Manager").await;
    let main = app.main_bot().await;
    let (root, root_token) = h.running(&main, GATE).await;
    let delegated = tool(
        app,
        &root_token,
        "delegate",
        json!({ "target_bot_id": target, "task": format!("{GATE} {task}"), "title": "Triage checkout errors" }),
    )
    .await;
    delegated.ok();
    h.release(root).await;
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    let (_, child_token) = h.started(child).await;
    Delegated {
        main,
        target,
        child,
        child_token,
    }
}

fn blocked_notice(summary: &str) -> Value {
    json!({ "kind": "blocked", "summary": summary,
            "tried": "Delegated to Dev; refused, no eligible session." })
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
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let task = delegated(&h, "triage checkout 500s").await;

    let first = tool(
        app,
        &task.child_token,
        "notify_owner",
        blocked_notice("Dev can't start the fix."),
    )
    .await;
    first.ok();
    let first = first.result;
    assert_eq!(first["status"], "pending");
    let again = tool(
        app,
        &task.child_token,
        "notify_owner",
        blocked_notice(
            "Dev has no eligible session, so the fix can't start. Start one or reassign it.",
        ),
    )
    .await;
    again.ok();
    let again = again.result;
    assert_eq!(
        again["notice_id"], first["notice_id"],
        "a retry rewrites the notice"
    );

    let shown = notices(app, &task.target, task.child).await;
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
    let status = sidebar_status(app, &task.target).await;
    assert_eq!(status["kind"], "needs_you", "{status}");

    let handled = dismiss(app, &task.target, &first["notice_id"]).await;
    assert_eq!(handled.status, StatusCode::OK, "{}", handled.body);
    assert_eq!(handled.body["notice"]["status"], "dismissed");
    let repeated = dismiss(app, &task.target, &first["notice_id"]).await;
    assert_eq!(
        repeated.status,
        StatusCode::OK,
        "dismissing again is a no-op"
    );
    assert_eq!(repeated.body["notice"]["status"], "dismissed");
    assert_eq!(
        notices(app, &task.target, task.child).await[0]["status"],
        "dismissed"
    );
    assert!(sidebar_status(app, &task.target).await.is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_message_answers_the_notice_and_it_can_no_longer_be_dismissed() {
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let task = delegated(&h, "triage checkout 500s").await;
    let flagged = tool(
        app,
        &task.child_token,
        "notify_owner",
        json!({ "kind": "decision", "summary": "Roll back the deploy or hotfix it?" }),
    )
    .await;
    flagged.ok();
    let flagged = flagged.result;

    let sent = app
        .send(&task.target, Uuid::new_v4(), "hotfix it", Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let shown = notices(app, &task.target, task.child).await;
    assert_eq!(shown[0]["status"], "replied", "{shown:?}");
    assert_eq!(shown[0]["tried"], Value::Null);

    let late = dismiss(app, &task.target, &flagged["notice_id"]).await;
    assert_eq!(late.status, StatusCode::CONFLICT, "{}", late.body);
    assert_eq!(late.body["error"]["code"], "owner_notice_settled");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_never_answers_the_notice() {
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let target = app.create_bot("Tech Manager").await;
    // A message of the bot's to react to, before it takes the task.
    let hello = app
        .send(&target, Uuid::new_v4(), "Hello there", Some("synthetic"))
        .await;
    let hello_turn = hello.body["turn"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        app.wait_turn(&hello_turn, &["completed"]).await,
        "completed"
    );
    let assistant: Uuid = sqlx::query_scalar(
        "SELECT id FROM items WHERE turn_id = ?1 AND kind = 'assistant' ORDER BY seq",
    )
    .bind(hello_turn.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    let main = app.main_bot().await;
    let (root, root_token) = h.running(&main, GATE).await;
    tool(
        app,
        &root_token,
        "delegate",
        json!({ "target_bot_id": target, "task": format!("{GATE} triage checkout 500s") }),
    )
    .await
    .ok();
    h.release(root).await;
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    let (_, child_token) = h.started(child).await;
    tool(
        app,
        &child_token,
        "notify_owner",
        json!({ "kind": "decision", "summary": "Roll back the deploy or hotfix it?" }),
    )
    .await
    .ok();

    let reacted = app
        .owner(
            "POST",
            &format!("/v1/bots/{target}/turns"),
            Some(json!({
                "request_id": Uuid::new_v4(),
                "text": "👍 Feedback on your earlier message",
                "provider_id": "synthetic",
                "reaction": { "item_id": assistant, "kind": "thumbs_up" },
            })),
        )
        .await;
    assert_eq!(reacted.status, StatusCode::ACCEPTED, "{}", reacted.body);
    let shown = notices(app, &target, child).await;
    assert_eq!(shown[0]["status"], "pending", "{shown:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_running_delegated_task_can_notify_the_owner() {
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let task = delegated(&h, "triage checkout 500s").await;

    let solo = app.create_bot("Solo").await;
    let (_, own_token) = h.running(&solo, GATE).await;
    let refused = tool(
        app,
        &own_token,
        "notify_owner",
        blocked_notice("I need you."),
    )
    .await;
    assert!(refused.error, "{}", refused.body);
    let refused = refused.result;
    assert!(
        refused.as_str().unwrap().starts_with("NOT_DELEGATED"),
        "{refused}"
    );

    for invalid in [
        json!({ "kind": "maybe", "summary": "Done." }),
        json!({ "kind": "fyi" }),
        json!({ "kind": "decision" }),
    ] {
        let refused = tool(app, &task.child_token, "notify_owner", invalid).await;
        assert!(refused.error);
        let refused = refused.result;
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
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let task = delegated(&h, "triage checkout 500s").await;
    let summary = "Dev can't start the fix. Start a Dev session.";
    let flagged = tool(
        app,
        &task.child_token,
        "notify_owner",
        blocked_notice(summary),
    )
    .await
    .result;
    let handled = dismiss(app, &task.target, &flagged["notice_id"]).await;
    assert_eq!(handled.status, StatusCode::OK, "{}", handled.body);

    let again = tool(
        app,
        &task.child_token,
        "notify_owner",
        blocked_notice(summary),
    )
    .await;
    again.ok();
    let again = again.result;
    assert_eq!(
        again["notice_id"], flagged["notice_id"],
        "the retry returns the handled notice"
    );
    let shown = notices(app, &task.target, task.child).await;
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(shown[0]["status"], "dismissed");
    assert!(sidebar_status(app, &task.target).await.is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_card_raised_after_the_notice_replaces_it() {
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let task = delegated(
        &h,
        "triage the checkout 500s: read the logs, find the failing requests, \
         compare the webhook secrets, and report what blocks the fix.",
    )
    .await;
    let flagged = tool(
        app,
        &task.child_token,
        "notify_owner",
        json!({ "kind": "decision", "summary": "Roll back the deploy or hotfix it?" }),
    )
    .await;
    flagged.ok();
    let flagged = flagged.result;
    let asked = tool(
        app,
        &task.child_token,
        "ask_owner",
        json!({ "question": "Roll back the deploy or hotfix it?" }),
    )
    .await;
    asked.ok();
    h.release(task.child).await;
    app.wait_turn(task.child, &["waiting"]).await;

    let shown = notices(app, &task.target, task.child).await;
    assert_eq!(shown[0]["status"], "superseded", "{shown:?}");
    let status = sidebar_status(app, &task.target).await;
    assert_ne!(
        status["kind"], "needs_you",
        "the card's own status shows: {status}"
    );
    let late = dismiss(app, &task.target, &flagged["notice_id"]).await;
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

#[tokio::test(flavor = "multi_thread")]
async fn a_task_declares_what_it_needs_and_both_chats_show_it() {
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let task = delegated(
        &h,
        "sort the overnight WhatsApp DMs: read each thread, decline the offers \
         we never take, draft replies for the rest, and report which ones need the owner.",
    )
    .await;

    let fyi = tool(
        app,
        &task.child_token,
        "notify_owner",
        json!({ "kind": "fyi", "summary": "Declined MIRAY myself." }),
    )
    .await;
    fyi.ok();
    let fyi = fyi.result;
    assert_eq!(fyi["status"], "recorded");
    let received = item_where(app, &task.target, "bot_task").await;
    assert_eq!(received["title"], "Triage checkout errors");
    assert_eq!(received["owner_need"], "fyi", "{received}");
    assert_eq!(received["owner_line"], "Declined MIRAY myself.");

    let asked = tool(
        app,
        &task.child_token,
        "notify_owner",
        json!({ "kind": "decision", "summary": "Approve the Romow DM before I send it." }),
    )
    .await;
    asked.ok();
    let asked = asked.result;
    assert_eq!(asked["status"], "pending");

    let quiet = tool(
        app,
        &task.child_token,
        "notify_owner",
        json!({ "kind": "nothing" }),
    )
    .await;
    assert!(quiet.error, "a quiet declaration cannot hide a pending ask");
    let quiet = quiet.result;
    assert!(
        quiet.as_str().unwrap().starts_with("ASK_PENDING"),
        "{quiet}"
    );
    let received = item_where(app, &task.target, "bot_task").await;
    assert_eq!(received["owner_need"], "decision", "{received}");
    assert_eq!(received["owner_line"], Value::Null);

    h.release(task.child).await;
    app.wait_turn(task.child, &["completed"]).await;
    let reply = item_where(app, &task.main, "bot_reply").await;
    assert_eq!(reply["title"], "Triage checkout errors");
    assert_eq!(reply["owner_need"], "decision", "{reply}");
    let asks = reply["asks"].as_array().unwrap();
    assert_eq!(asks.len(), 1, "{reply}");
    assert_eq!(asks[0]["id"], asked["notice_id"]);
    assert_eq!(asks[0]["bot_name"], "Tech Manager");
    assert_eq!(asks[0]["kind"], "decision");
    assert_eq!(asks[0]["status"], "pending");
    assert_eq!(asks[0]["summary"], "Approve the Romow DM before I send it.");

    let handled = dismiss(app, &task.target, &asked["notice_id"]).await;
    assert_eq!(handled.status, StatusCode::OK, "{}", handled.body);
    let reply = item_where(app, &task.main, "bot_reply").await;
    assert_eq!(
        reply["asks"][0]["status"], "dismissed",
        "the delegating chat follows the ask: {reply}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_that_declares_nothing_keeps_its_reply_quiet() {
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let task = delegated(
        &h,
        "count this week's reach on every channel, compare it with last \
         week, and report the totals without changing anything.",
    )
    .await;
    let quiet = tool(
        app,
        &task.child_token,
        "notify_owner",
        json!({ "kind": "nothing" }),
    )
    .await;
    quiet.ok();
    let quiet = quiet.result;
    assert_eq!(quiet["kind"], "nothing");
    h.release(task.child).await;
    app.wait_turn(task.child, &["completed"]).await;
    let reply = item_where(app, &task.main, "bot_reply").await;
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
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let task = delegated(&h, "triage checkout 500s").await;
    let flagged = tool(
        app,
        &task.child_token,
        "notify_owner",
        json!({ "kind": "decision", "summary": "Roll back the deploy or hotfix it?" }),
    )
    .await;
    flagged.ok();
    let flagged = flagged.result;
    let first: Uuid = flagged["notice_id"].as_str().unwrap().parse().unwrap();
    let other = second_ask(app, &task).await;

    // With two asks pending, a message without a target cannot tell which
    // it answers, so both stay.
    let plain = reply(app, &task.target, Uuid::new_v4(), "one moment", None).await;
    assert_eq!(plain.status, StatusCode::ACCEPTED, "{}", plain.body);
    assert_eq!(status_of(app, first).await, "pending");
    assert_eq!(status_of(app, other).await, "pending");

    // Another bot's ask is refused, and the message is not stored.
    let foreign = reply(app, &task.main, Uuid::new_v4(), "hotfix it", Some(first)).await;
    assert_eq!(foreign.status, StatusCode::BAD_REQUEST, "{}", foreign.body);
    assert_eq!(foreign.body["error"]["code"], "invalid_notice");
    assert_eq!(status_of(app, first).await, "pending");

    let request = Uuid::new_v4();
    let targeted = reply(app, &task.target, request, "hotfix it", Some(first)).await;
    assert_eq!(targeted.status, StatusCode::ACCEPTED, "{}", targeted.body);
    assert_eq!(status_of(app, first).await, "replied");
    assert_eq!(status_of(app, other).await, "pending");

    // A retry returns the stored message; the same ID with another target
    // is a different request.
    let retry = reply(app, &task.target, request, "hotfix it", Some(first)).await;
    assert_eq!(retry.status, StatusCode::OK, "{}", retry.body);
    assert_eq!(retry.body["turn"]["id"], targeted.body["turn"]["id"]);
    let conflict = reply(app, &task.target, request, "hotfix it", Some(other)).await;
    assert_eq!(conflict.status, StatusCode::CONFLICT, "{}", conflict.body);
    assert_eq!(status_of(app, other).await, "pending");

    // Answering a settled ask still delivers the message and changes nothing.
    let late = reply(app, &task.target, Uuid::new_v4(), "also this", Some(first)).await;
    assert_eq!(late.status, StatusCode::ACCEPTED, "{}", late.body);
    assert_eq!(status_of(app, first).await, "replied");
    assert_eq!(status_of(app, other).await, "pending");

    // With one ask left, a plain message answers it, as before.
    let last = reply(app, &task.target, Uuid::new_v4(), "yes renew", None).await;
    assert_eq!(last.status, StatusCode::ACCEPTED, "{}", last.body);
    assert_eq!(status_of(app, other).await, "replied");
}
