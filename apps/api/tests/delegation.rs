//! POC-016: one-hop asynchronous delegation through the real router,
//! scheduler, and SQLite, with the synthetic provider.

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
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(turn_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let token = app
        .scheduler
        .grants()
        .issue(
            main.parse().unwrap(),
            conversation,
            turn_id,
            vec![Tool::ReadHistory, Tool::ReadMemory, Tool::Delegate],
            DEFAULT_TTL,
        )
        .await;
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
            "SELECT status, result, delivered_attempt, child_turn_id FROM delegations WHERE id = ?1",
        )
        .bind(task_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "completed");
    assert!(result.unwrap().contains("planetarium form"));
    assert_eq!(delivered, Some(2), "the second attempt received it");
    let attempts: i32 = sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(root)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(attempts, 2);
    let children: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delegations WHERE root_turn_id = ?1")
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
        "SELECT c.kind FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE t.id = ?1",
    )
    .bind(child)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(child_conversation, "task");
    // Memory: the task is model-authored in the target; the result is tool
    // activity in the parent.
    let task_entry: (String, String) = sqlx::query_as(
        "SELECT kind, authority FROM memory_entry WHERE bot_id = ?1 AND body LIKE '%planetarium form details%'
         ORDER BY seq LIMIT 1",
    )
    .bind(school.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(task_entry, ("delegated_task".into(), "assistant".into()));
    let parent_entry: String = sqlx::query_scalar(
        "SELECT authority FROM memory_entry WHERE bot_id = ?1 AND request_key = ?2",
    )
    .bind(main.parse::<Uuid>().unwrap())
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
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(root)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let no_delegate = app
        .scheduler
        .grants()
        .issue(
            main.parse().unwrap(),
            conversation,
            root,
            vec![Tool::ReadMemory],
            DEFAULT_TTL,
        )
        .await;
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
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    for _ in 0..200 {
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
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
        sqlx::query_scalar("SELECT count(*) FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(children, 1);

    // A request whose two children are used up refuses a third, whatever
    // the attempt: simulate a third attempt that is running.
    let (_, root, _) = running_main(&app).await;
    for (n, target) in [&work, &family].iter().enumerate() {
        sqlx::query("UPDATE turns SET attempt = ?2 WHERE id = ?1")
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
    sqlx::query("UPDATE turns SET attempt = 3 WHERE id = ?1")
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
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
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
    let status: String = sqlx::query_scalar("SELECT status FROM delegations WHERE id = ?1")
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
        sqlx::query_as("SELECT status, error, delivered_attempt FROM delegations WHERE id = ?1")
            .bind(task_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(status, "cancelled");
    assert!(error.is_some());
    assert_eq!(delivered, Some(2), "the continuation received the outcome");
}

/// Rows for a main bot's running root with one queued child on Target,
/// written directly so states can be set precisely (no scheduler runs).
async fn seed(db: &TestDb) -> (Uuid, Uuid, Uuid, Uuid) {
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let main: Uuid = sqlx::query_scalar("SELECT default_bot_id FROM installation")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let target = Uuid::new_v4();
    sqlx::query("INSERT INTO bots (id, name, role) VALUES (?1, 'Target', 'target role')")
        .bind(target)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations (id, bot_id) VALUES (?1, ?2)")
        .bind(Uuid::new_v4())
        .bind(target)
        .execute(&db.pool)
        .await
        .unwrap();
    let conversation: Uuid = sqlx::query_scalar("SELECT id FROM conversations WHERE bot_id = ?1")
        .bind(main)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let root = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, status, attempt)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, 'synthetic', 'running', 1)",
    )
    .bind(root)
    .bind(main)
    .bind(conversation)
    .bind(Uuid::new_v4())
    .bind(vec![0u8; 32])
    .bind(Uuid::new_v4())
    .execute(&db.pool)
    .await
    .unwrap();
    let accepted = else_api::db::delegations::create(&db.pool, root, target, "seeded task")
        .await
        .unwrap();
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(accepted.delegation_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    (main, root, child, accepted.delegation_id)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_child_settling_while_the_parent_parks_still_requeues_it() {
    let db = TestDb::migrated().await;
    let (_, root, child, delegation) = seed(&db).await;
    // The parent's finish holds the root lock and decides to park.
    let mut parent = db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("SELECT 1 FROM turns WHERE id = ?1")
        .bind(root)
        .execute(&mut *parent)
        .await
        .unwrap();
    // Meanwhile the child ends; its trigger waits for the root lock.
    let pool = db.pool.clone();
    let child_end = tokio::spawn(async move {
        sqlx::query("UPDATE turns SET status = 'completed', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE id = ?1")
            .bind(child)
            .execute(&pool)
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !child_end.is_finished(),
        "settlement waits for the parent's decision"
    );
    sqlx::query("UPDATE turns SET status = 'waiting' WHERE id = ?1")
        .bind(root)
        .execute(&mut *parent)
        .await
        .unwrap();
    parent.commit().await.unwrap();
    child_end.await.unwrap();
    let (root_status, delegation_status): (String, String) = sqlx::query_as(
        "SELECT t.status, d.status FROM turns t JOIN delegations d ON d.root_turn_id = t.id
         WHERE d.id = ?1",
    )
    .bind(delegation)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        (root_status.as_str(), delegation_status.as_str()),
        ("queued", "completed")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_reconciliation_ends_the_children_of_an_ended_root() {
    let db = TestDb::migrated().await;
    let (_, root, child, delegation) = seed(&db).await;
    else_api::db::turns::recover_after_restart(&db.pool)
        .await
        .unwrap();
    let (root_status, child_status, delegation_status): (String, String, String) = sqlx::query_as(
        "SELECT r.status, c.status, d.status FROM delegations d
         JOIN turns r ON r.id = d.root_turn_id JOIN turns c ON c.id = d.child_turn_id
         WHERE d.id = ?1",
    )
    .bind(delegation)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let _ = (root, child);
    assert_eq!(root_status, "uncertain");
    assert_eq!(child_status, "interrupted", "no orphan child runs");
    assert_eq!(delegation_status, "cancelled");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_task_to_another_bot_is_a_different_child() {
    let db = TestDb::migrated().await;
    let (main, root, _, first) = seed(&db).await;
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO bots (id, name, role) VALUES (?1, 'Other', 'other role')")
        .bind(other)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations (id, bot_id) VALUES (?1, ?2)")
        .bind(Uuid::new_v4())
        .bind(other)
        .execute(&db.pool)
        .await
        .unwrap();
    let _ = main;
    sqlx::query("UPDATE turns SET attempt = 2 WHERE id = ?1")
        .bind(root)
        .execute(&db.pool)
        .await
        .unwrap();
    let second = else_api::db::delegations::create(&db.pool, root, other, "seeded task")
        .await
        .unwrap();
    assert!(second.created);
    assert_ne!(second.delegation_id, first);
    // A retry of a settled child reports its outcome, not `pending`.
    let target: Uuid = sqlx::query_scalar("SELECT target_bot_id FROM delegations WHERE id = ?1")
        .bind(first)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(first)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE turns SET status = 'completed', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE id = ?1")
        .bind(child)
        .execute(&db.pool)
        .await
        .unwrap();
    let retry = else_api::db::delegations::create(&db.pool, root, target, "seeded task")
        .await
        .unwrap();
    assert!(!retry.created);
    assert_eq!(retry.status, "completed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_task_cannot_read_memory_the_parent_could_not() {
    let app = app_with(2).await;
    let work = leaf(&app, "Work", "Work projects").await;
    let private = leaf(&app, "Private", "Private notes").await;
    let shared = leaf(&app, "Shared", "Shared notes").await;
    let main = app.main_bot().await;
    // Work may read Private and Shared; the main bot may read only Shared.
    for (reader, target) in [(&work, &private), (&work, &shared), (&main, &shared)] {
        let reply = app
            .owner(
                "PUT",
                &format!("/v1/bots/{reader}/memory-access/{target}"),
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
    }
    let scope = else_api::scheduler::delegated_scope(
        &app.db.pool,
        main.parse().unwrap(),
        work.parse().unwrap(),
    )
    .await
    .unwrap();
    let work_id: Uuid = work.parse().unwrap();
    let shared_id: Uuid = shared.parse().unwrap();
    let private_id: Uuid = private.parse().unwrap();
    assert!(scope.contains(&work_id) && scope.contains(&shared_id));
    assert!(
        !scope.contains(&private_id),
        "Work's own grant to Private is not inherited"
    );
    // A scoped grant is refused outside its scope even though Work itself
    // may read Private.
    let warm: Uuid = sqlx::query_scalar(
        "SELECT id FROM turns WHERE bot_id = ?1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(work_id)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    let reply = app
        .send(&work, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn: Uuid = reply.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&turn.to_string(), &["running"]).await;
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let _ = warm;
    let token = app
        .scheduler
        .grants()
        .issue_scoped(
            work_id,
            conversation,
            turn,
            vec![Tool::ReadMemory],
            Some(scope),
            DEFAULT_TTL,
        )
        .await;
    let call = |bot: Uuid| {
        app.call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": "memory_search",
                                     "arguments": { "query": "hello", "bot_id": bot } } })),
            Some(&token),
        )
    };
    assert_eq!(call(private_id).await.body["result"]["isError"], true);
    assert_ne!(call(shared_id).await.body["result"]["isError"], true);
}
