mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::{TestApp, TestDb};
use else_api::grants::{DEFAULT_TTL, Tool};
use serde_json::{Value, json};
use uuid::Uuid;

const SLOW: &str = "[synthetic:slow] please plan the week: groceries, school forms, the work \
                    review, and the dentist; ask the right bot for the school part of this.";

async fn app_with(max_active: usize) -> TestApp {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    TestApp::with_options(db, max_active)
}

async fn leaf(app: &TestApp, name: &str) -> String {
    let bot = app.create_bot(name).await;
    let warm = app
        .send(&bot, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    app.wait_turn(warm.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    bot
}

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
            vec![
                Tool::ReadHistory,
                Tool::ReadMemory,
                Tool::Delegate,
                Tool::CreateBot,
            ],
            DEFAULT_TTL,
        )
        .await;
    (main, turn_id, token)
}

async fn create_bot(app: &TestApp, token: &str, arguments: Value) -> common::Reply {
    app.call(
        "POST",
        "/tools/v1/mcp",
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                     "params": { "name": "create_bot", "arguments": arguments } })),
        Some(token),
    )
    .await
}

async fn bot_count(app: &TestApp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM bots")
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn forged(app: &TestApp, turn: Uuid) -> String {
    let (bot, conversation): (Uuid, Uuid) =
        sqlx::query_as("SELECT bot_id, conversation_id FROM turns WHERE id = ?1")
            .bind(turn)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    app.scheduler
        .grants()
        .issue(bot, conversation, turn, vec![Tool::CreateBot], DEFAULT_TTL)
        .await
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
async fn a_new_bot_acknowledges_delegated_work_in_its_conversation() {
    let app = app_with(1).await;
    let target = app.create_bot("GitHub").await;
    sqlx::query("UPDATE bots SET selected_provider_id = 'synthetic' WHERE id = ?1")
        .bind(target.parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
    let (main, root, token) = running_main(&app).await;
    let task = "Set up the repository cleanup routine every six hours.";
    let reply = delegate(&app, &token, &target, task).await;
    assert_eq!(tool_text(&reply)["status"], "pending", "{}", reply.body);

    let snapshot = app
        .owner("GET", &format!("/v1/bots/{target}/conversation"), None)
        .await
        .body;
    let items = snapshot["items"].as_array().unwrap();
    assert!(
        !items.is_empty(),
        "receiving bot conversation is empty after accepting delegated work: {snapshot}"
    );
    let sender: String = sqlx::query_scalar("SELECT name FROM bots WHERE id = ?1")
        .bind(main.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert!(
        items.iter().any(|item| {
            item["kind"] != "user"
                && item["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(&sender) && text.contains(task))
        }),
        "missing acknowledgement of sender and task: {snapshot}"
    );

    app.wait_turn_for(&root.to_string(), &["completed"], Duration::from_secs(40))
        .await;
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{target}/conversation"), None)
        .await
        .body;
    assert!(
        snapshot["items"].as_array().unwrap().iter().any(|item| {
            item["kind"] == "assistant"
                && item["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("Synthetic reply"))
        }),
        "receiving bot reply is missing: {snapshot}"
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn a_delegation_receipt_is_shown_once_before_the_child_runs() {
    let db = TestDb::migrated().await;
    let (main, root, child, delegation) = seed(&db).await;
    let target: Uuid = sqlx::query_scalar("SELECT target_bot_id FROM delegations WHERE id = ?1")
        .bind(delegation)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let retry = else_api::db::delegations::create(&db.pool, root, target, "seeded task")
        .await
        .unwrap();
    assert_eq!((retry.created, retry.delegation_id), (false, delegation));

    let sender: String = sqlx::query_scalar("SELECT name FROM bots WHERE id = ?1")
        .bind(main)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let snapshot = else_api::db::conversation::snapshot(&db.pool, target, 50, None)
        .await
        .unwrap()
        .unwrap();
    let shown: Vec<(&str, Uuid)> = snapshot
        .items
        .iter()
        .map(|i| (i.kind.as_str(), i.turn_id))
        .collect();
    assert_eq!(shown, [("status", child)], "one receipt, no owner message");
    let task = snapshot.items[0].bot_task.as_ref().unwrap();
    assert_eq!(task.sender_bot_id, main);
    assert_eq!(task.sender_name, sender);
    assert_eq!(task.prompt, "seeded task");
    let receipt = &snapshot.items[0].text;
    assert!(
        receipt.contains(&sender) && receipt.contains("seeded task"),
        "{receipt}"
    );
    let turns: Vec<(Uuid, &str)> = snapshot
        .turns
        .iter()
        .map(|t| (t.id, t.status.as_str()))
        .collect();
    assert_eq!(turns, [(child, "queued")]);

    let source: Uuid = sqlx::query_scalar("SELECT owner_item_id FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    else_api::db::events::item_upsert(&mut tx, target, source)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let conversation = owner_conversation(&db.pool, target).await;
    let events = turn_events(&db.pool, target, child).await;
    assert_eq!(
        events[0].1["item"]["bot_task"]["sender_bot_id"],
        main.to_string()
    );
    assert_eq!(events[0].1["item"]["bot_task"]["prompt"], "seeded task");
    let kinds: Vec<&str> = events.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(kinds, ["item.upsert", "turn.updated"], "{events:?}");
    for (_, data) in &events {
        assert_eq!(data["conversation_id"], conversation.to_string().as_str());
    }
    assert_eq!(
        events[0].1["item"]["id"],
        snapshot.items[0].id.to_string().as_str()
    );
    assert_eq!(events[1].1["turn"]["status"], "queued");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_reply_streams_into_the_receiving_chat_in_order() {
    let app = app_with(1).await;
    let school = leaf(&app, "School").await;
    let school_id: Uuid = school.parse().unwrap();
    let (_, root, token) = running_main(&app).await;
    let task = "Collect the planetarium form details.";
    delegate(&app, &token, &school, task).await;
    app.wait_turn_for(&root.to_string(), &["completed"], Duration::from_secs(40))
        .await;
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();

    let page = |query: &'static str| {
        let app = &app;
        let school = &school;
        async move {
            app.owner(
                "GET",
                &format!("/v1/bots/{school}/conversation?{query}"),
                None,
            )
            .await
            .body
        }
    };
    let rows = |snapshot: &Value| -> Vec<(i64, String, String)> {
        snapshot["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                (
                    i["seq"].as_i64().unwrap(),
                    i["kind"].as_str().unwrap().to_owned(),
                    i["turn_id"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let newest = page("limit=2").await;
    let child_id = child.to_string();
    assert_eq!(
        rows(&newest),
        [
            (3, "status".to_owned(), child_id.clone()),
            (4, "assistant".to_owned(), child_id.clone())
        ],
        "{newest}"
    );
    assert_eq!(newest["older_before_seq"], 3);
    let older = page("limit=2&before_seq=3").await;
    let older_kinds: Vec<String> = rows(&older).into_iter().map(|(_, kind, _)| kind).collect();
    assert_eq!(
        older_kinds,
        ["user", "assistant"],
        "the earlier owner exchange"
    );
    assert_eq!(older["older_before_seq"], Value::Null);

    let conversation = owner_conversation(&app.db.pool, school_id).await;
    let events = turn_events(&app.db.pool, school_id, child).await;
    assert!(
        events
            .iter()
            .all(|(_, data)| data["conversation_id"] == conversation.to_string().as_str()),
        "{events:?}"
    );
    let reply_upserts = events
        .iter()
        .filter(|(kind, data)| kind == "item.upsert" && data["item"]["kind"] == "assistant")
        .count();
    assert_eq!(reply_upserts, 1, "{events:?}");
    let statuses: Vec<&str> = events
        .iter()
        .filter(|(kind, _)| kind == "turn.updated")
        .map(|(_, data)| data["turn"]["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses.first(), Some(&"queued"), "{statuses:?}");
    assert!(statuses.contains(&"running"), "{statuses:?}");
    assert_eq!(statuses.last(), Some(&"completed"), "{statuses:?}");

    let replies: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM memory_entry WHERE bot_id = ?1 AND body LIKE '%Synthetic reply%planetarium form details%'",
    )
    .bind(school_id)
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(replies, ["assistant_message"]);

    let next = app
        .send(
            &school,
            Uuid::new_v4(),
            "[synthetic:hang] and now?",
            Some("synthetic"),
        )
        .await;
    let next: Uuid = next.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    let input = else_api::db::turns::input(&app.db.pool, next)
        .await
        .unwrap();
    let packet = else_api::providers::prompt::packet(&input, 0);
    assert!(
        packet.contains("Delegated task (another bot's request, not an owner message)")
            && !packet.contains(&format!("Owner: {task}")),
        "{packet}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn task_history_pages_its_source_and_reply_without_the_owner_receipt() {
    let app = app_with(2).await;
    let target = app.create_bot("GitHub").await;
    let target_id: Uuid = target.parse().unwrap();
    sqlx::query("UPDATE bots SET selected_provider_id = 'synthetic' WHERE id = ?1")
        .bind(target_id)
        .execute(&app.db.pool)
        .await
        .unwrap();
    let (_, root, token) = running_main(&app).await;
    delegate(&app, &token, &target, "[synthetic:slow] Set up the repository cleanup routine every six hours. Preserve protected branches and report any access blockers. Keep a record of changes and verify the schedule before you finish.").await;
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    wait_status(&app, child, "running").await;
    let (task_conversation, reply): (Uuid, Uuid) =
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let found: Option<(Uuid, Uuid)> = sqlx::query_as(
                    "SELECT t.conversation_id, i.id FROM turns t JOIN items i ON i.turn_id = t.id
                 WHERE t.id = ?1 AND i.kind = 'assistant'",
                )
                .bind(child)
                .fetch_optional(&app.db.pool)
                .await
                .unwrap();
                if let Some(found) = found {
                    break found;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    let grant = app
        .scheduler
        .grants()
        .issue(
            target_id,
            task_conversation,
            child,
            vec![Tool::ReadHistory],
            DEFAULT_TTL,
        )
        .await;
    let newest = app
        .call("GET", "/tools/v1/history?limit=1", None, Some(&grant))
        .await;
    assert_eq!(newest.body["items"][0]["id"], reply.to_string());
    let older = app
        .call("GET", "/tools/v1/history?before_seq=2", None, Some(&grant))
        .await;
    assert_eq!(older.status, StatusCode::OK, "{}", older.body);
    let items = older.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{}", older.body);
    assert_eq!(items[0]["kind"], "user");
    for id in [
        reply.to_string(),
        items[0]["id"].as_str().unwrap().to_owned(),
    ] {
        let own = app
            .call("GET", &format!("/tools/v1/items/{id}"), None, Some(&grant))
            .await;
        assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    }
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{target}/conversation"), None)
        .await;
    let receipt = snapshot.body["items"][0]["id"].as_str().unwrap();
    let hidden = app
        .call(
            "GET",
            &format!("/tools/v1/items/{receipt}"),
            None,
            Some(&grant),
        )
        .await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_delegated_task_keeps_its_scope_and_its_stop_is_shown() {
    let app = app_with(2).await;
    let work = leaf(&app, "Work").await;
    let work_id: Uuid = work.parse().unwrap();
    let (_, root, token) = running_main(&app).await;
    delegate(&app, &token, &work, "[synthetic:hang] long research").await;
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    wait_status(&app, child, "running").await;

    let task_conversation: Uuid =
        sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
            .bind(child)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    let grant = app
        .scheduler
        .grants()
        .issue(
            work_id,
            task_conversation,
            child,
            vec![Tool::ReadHistory],
            DEFAULT_TTL,
        )
        .await;
    let history = app
        .call("GET", "/tools/v1/history", None, Some(&grant))
        .await;
    assert_eq!(history.status, StatusCode::OK, "{}", history.body);
    let items = history.body["items"].as_array().unwrap();
    let kinds: Vec<&str> = items.iter().map(|i| i["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["user"], "its task, without the owner's transcript");
    for item in items {
        let id = item["id"].as_str().unwrap();
        let own = app
            .call("GET", &format!("/tools/v1/items/{id}"), None, Some(&grant))
            .await;
        assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    }
    let hello: Uuid = sqlx::query_scalar(
        "SELECT i.id FROM items i JOIN conversations c ON c.id = i.conversation_id
         WHERE c.bot_id = ?1 AND c.kind = 'owner' AND i.kind = 'user'",
    )
    .bind(work_id)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    let foreign = app
        .call(
            "GET",
            &format!("/tools/v1/items/{hello}"),
            None,
            Some(&grant),
        )
        .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);

    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{work}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    wait_status(&app, child, "interrupted").await;
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{work}/conversation"), None)
        .await
        .body;
    let shown = snapshot["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == child.to_string().as_str())
        .map(|t| t["status"].clone());
    assert_eq!(shown, Some(json!("interrupted")), "{snapshot}");
    let last = turn_events(&app.db.pool, work_id, child)
        .await
        .pop()
        .unwrap();
    assert_eq!(last.1["turn"]["status"], "interrupted");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_child_parks_the_parent_and_a_continuation_uses_its_result() {
    let app = app_with(1).await;
    let school = leaf(&app, "School").await;
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
    let again = delegate(
        &app,
        &token,
        &school,
        "Collect the planetarium form details.",
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT, "the attempt is parked");

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

    let snapshot = app
        .owner("GET", &format!("/v1/bots/{school}/conversation"), None)
        .await
        .body;
    let shown: Vec<(&str, &str)> = snapshot["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["turn_id"] == child.to_string().as_str())
        .map(|i| (i["kind"].as_str().unwrap(), i["text"].as_str().unwrap()))
        .collect();
    assert!(
        matches!(shown.as_slice(), [("status", receipt), ("assistant", reply)]
            if receipt.contains("planetarium form details")
                && reply.contains("planetarium form details")),
        "{shown:?}"
    );
    let child_status = snapshot["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == child.to_string().as_str())
        .map(|t| t["status"].clone());
    assert_eq!(child_status, Some(json!("completed")), "{snapshot}");
    let child_conversation: String = sqlx::query_scalar(
        "SELECT c.kind FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE t.id = ?1",
    )
    .bind(child)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(child_conversation, "task");
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
    let work = leaf(&app, "Work").await;
    let family = leaf(&app, "Family").await;
    let gone = leaf(&app, "Gone").await;
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
    let first = tool_text(&delegate(&app, &token, &work, "first task").await);
    assert_eq!(first["status"], "pending");
    app.wait_turn_for(
        &root.to_string(),
        &["waiting", "queued", "preparing", "running", "completed"],
        Duration::from_secs(20),
    )
    .await;
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
    let work = leaf(&app, "Work").await;
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

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_follow_up_ends_a_waiting_root_and_its_task_only() {
    let app = app_with(2).await;
    let work = leaf(&app, "Work").await;
    let (main, root, token) = running_main(&app).await;
    let reply = delegate(&app, &token, &work, "[synthetic:hang] long research").await;
    let task_id: Uuid = tool_text(&reply)["task_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    wait_status(&app, root, "waiting").await;
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(task_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    wait_status(&app, child, "running").await;

    let aside = app
        .send(
            &work,
            Uuid::new_v4(),
            "an aside for Work",
            Some("synthetic"),
        )
        .await;
    assert_eq!(aside.status, StatusCode::ACCEPTED, "{}", aside.body);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let child_status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(child_status, "running");

    let follow = app
        .send(
            &main,
            Uuid::new_v4(),
            "Do the school part yourself.",
            Some("synthetic"),
        )
        .await;
    assert_eq!(follow.status, StatusCode::ACCEPTED, "{}", follow.body);
    wait_status(&app, root, "interrupted").await;
    wait_status(&app, child, "interrupted").await;
    let status: String = sqlx::query_scalar("SELECT status FROM delegations WHERE id = ?1")
        .bind(task_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "cancelled", "the task does not outlive its root");
    app.wait_turn(follow.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    app.wait_turn(aside.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
}

async fn seed(db: &TestDb) -> (Uuid, Uuid, Uuid, Uuid) {
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let main: Uuid = sqlx::query_scalar("SELECT default_bot_id FROM installation")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let target = Uuid::new_v4();
    sqlx::query("INSERT INTO bots (id, name) VALUES (?1, 'Target')")
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
async fn the_delegating_turn_shows_its_handoff_as_the_child_progresses() {
    let db = TestDb::migrated().await;
    let (main, root, child, delegation) = seed(&db).await;
    let snapshot = else_api::db::conversation::snapshot(&db.pool, main, 50, None)
        .await
        .unwrap()
        .unwrap();
    let turn = snapshot.turns.iter().find(|t| t.id == root).unwrap();
    let shown = else_api::views::turn_json(turn);
    assert_eq!(
        shown["delegations"][0]["id"],
        delegation.to_string().as_str()
    );
    assert_eq!(shown["delegations"][0]["target_name"], "Target");
    assert_eq!(shown["delegations"][0]["task"], "seeded task");
    assert_eq!(shown["delegations"][0]["status"], "queued");
    assert_eq!(shown["delegations"][0]["attempt"], 1);

    let target: Uuid = sqlx::query_scalar("SELECT bot_id FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    for status in ["running", "completed"] {
        let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        sqlx::query("UPDATE turns SET status = ?1 WHERE id = ?2")
            .bind(status)
            .bind(child)
            .execute(&mut *tx)
            .await
            .unwrap();
        else_api::db::events::turn_changed(&mut tx, target, child)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let handoffs: Vec<Value> = turn_events(&db.pool, main, root)
        .await
        .into_iter()
        .filter(|(kind, _)| kind == "turn.updated")
        .map(|(_, data)| data["turn"]["delegations"][0]["status"].clone())
        .collect();
    assert_eq!(
        handoffs,
        [json!("queued"), json!("running"), json!("completed")]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_child_settling_while_the_parent_parks_still_requeues_it() {
    let db = TestDb::migrated().await;
    let (_, root, child, delegation) = seed(&db).await;
    let mut parent = db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("SELECT 1 FROM turns WHERE id = ?1")
        .bind(root)
        .execute(&mut *parent)
        .await
        .unwrap();
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
    sqlx::query("INSERT INTO bots (id, name) VALUES (?1, 'Other')")
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
    let work = leaf(&app, "Work").await;
    let private = leaf(&app, "Private").await;
    let shared = leaf(&app, "Shared").await;
    let main = app.main_bot().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn the_main_bot_creates_specialists_it_can_delegate_to_at_once() {
    let app = app_with(2).await;
    let (_, root, token) = running_main(&app).await;

    let reply = create_bot(&app, &token, json!({ "name": "  GitHub " })).await;
    let created = tool_text(&reply);
    assert_eq!(created["created"], true, "{}", reply.body);
    assert_eq!(created["status"], "active");
    assert_eq!(created["name"], "GitHub");
    let github = created["bot_id"].as_str().unwrap().to_owned();
    let listed = app.owner("GET", "/v1/bots", None).await.body;
    let shown = listed["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == github.as_str())
        .unwrap_or_else(|| panic!("{listed}"))
        .clone();
    assert_eq!(shown["name"], "GitHub");
    let profile: i64 = sqlx::query_scalar("SELECT count(*) FROM bot_profiles WHERE bot_id = ?1")
        .bind(github.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(profile, 0, "no profile of its own");
    let grants: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_read_grants WHERE reader_bot_id = ?1 OR target_bot_id = ?1",
    )
    .bind(github.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(grants, 0, "no shared memory");

    let before = bot_count(&app).await;
    let args = json!({ "name": "GitHub" });
    let (a, b, c) = tokio::join!(
        create_bot(&app, &token, args.clone()),
        create_bot(&app, &token, args.clone()),
        create_bot(&app, &token, args.clone()),
    );
    for reply in [a, b, c] {
        let again = tool_text(&reply);
        assert_eq!(again["bot_id"], github.as_str(), "{}", reply.body);
        assert_eq!(again["created"], false);
    }
    sqlx::query("UPDATE turns SET attempt = attempt + 1 WHERE id = ?1")
        .bind(root)
        .execute(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        tool_text(&create_bot(&app, &token, args).await)["bot_id"],
        github.as_str()
    );
    assert_eq!(bot_count(&app).await, before);

    let ops = json!({ "name": "Ops" });
    let (a, b, c) = tokio::join!(
        create_bot(&app, &token, ops.clone()),
        create_bot(&app, &token, ops.clone()),
        create_bot(&app, &token, ops.clone()),
    );
    let replies: Vec<Value> = [a, b, c].iter().map(tool_text).collect();
    assert_eq!(
        replies.iter().filter(|r| r["created"] == true).count(),
        1,
        "{replies:?}"
    );
    assert!(replies.iter().all(|r| r["bot_id"] == replies[0]["bot_id"]));
    assert_ne!(
        replies[0]["bot_id"],
        github.as_str(),
        "another name is another bot"
    );
    assert_eq!(bot_count(&app).await, before + 1);

    let refusals = [
        (
            json!({ "name": "X", "role": "Maintain repositories." }),
            "INVALID_ARGUMENTS",
        ),
        (
            json!({ "name": "X", "profile": "gpt" }),
            "INVALID_ARGUMENTS",
        ),
        (json!({}), "INVALID_ARGUMENTS"),
        (json!({ "name": 7 }), "INVALID_ARGUMENTS"),
        (json!({ "name": "   " }), "INVALID_BOT"),
    ];
    for (arguments, code) in refusals {
        let reply = create_bot(&app, &token, arguments.clone()).await;
        assert_eq!(reply.body["result"]["isError"], true, "{arguments}");
        let text = reply.body["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with(code), "{arguments}: {text}");
    }
    assert_eq!(bot_count(&app).await, before + 1);

    let pending = tool_text(&delegate(&app, &token, &github, "List stale branches.").await);
    assert_eq!(pending["status"], "pending", "{pending}");
    assert_eq!(pending["target"]["bot_id"], github.as_str());
    let parked = create_bot(&app, &token, json!({ "name": "Late" })).await;
    assert_eq!(parked.status, StatusCode::CONFLICT);
    let name = else_api::app::bots::BotName::parse("Late").unwrap();
    let direct =
        else_api::db::bots::create_for_turn(&app.db.pool, root, &name, None, None, None).await;
    assert!(
        matches!(
            direct,
            Err(else_api::db::bots::BotError::Refused(
                else_api::db::bots::Refusal::AttemptDelegated
                    | else_api::db::bots::Refusal::NotRunning
            ))
        ),
        "{direct:?}"
    );
    assert_eq!(bot_count(&app).await, before + 1);
    wait_status(&app, root, "completed").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_creation_replays_as_deleted() {
    let app = app_with(2).await;
    let (_, _, token) = running_main(&app).await;
    let args = json!({ "name": "GitHub" });
    let github = tool_text(&create_bot(&app, &token, args.clone()).await)["bot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let deleted = app
        .owner("DELETE", &format!("/v1/bots/{github}"), None)
        .await;
    assert!(deleted.status.is_success(), "{}", deleted.body);
    let before = bot_count(&app).await;
    let replay = tool_text(&create_bot(&app, &token, args).await);
    assert_eq!(replay["bot_id"], github.as_str());
    assert_eq!(replay["created"], false);
    assert_eq!(replay["status"], "deleted");
    assert_eq!(bot_count(&app).await, before, "no replacement");
}

#[tokio::test(flavor = "multi_thread")]
async fn creation_requires_an_owner_request_and_a_purpose_for_peer_bots() {
    let app = app_with(3).await;
    let work = leaf(&app, "Work").await;
    let (main, root, token) = running_main(&app).await;
    let before = bot_count(&app).await;
    let args = json!({ "name": "GitHub" });
    let text = |reply: &common::Reply| {
        reply.body["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };

    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(root)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let plain = app
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
        create_bot(&app, &plain, args.clone()).await.status,
        StatusCode::FORBIDDEN
    );

    let sent = app
        .send(&work, Uuid::new_v4(), SLOW, Some("synthetic"))
        .await;
    let leaf_turn: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&leaf_turn.to_string(), &["running"]).await;
    let reply = create_bot(&app, &forged(&app, leaf_turn).await, args.clone()).await;
    assert!(
        text(&reply).starts_with("PURPOSE_REQUIRED"),
        "{}",
        reply.body
    );

    let pending = tool_text(&delegate(&app, &token, &work, "[synthetic:hang] research").await);
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(
            pending["task_id"]
                .as_str()
                .unwrap()
                .parse::<Uuid>()
                .unwrap(),
        )
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    app.wait_turn_for(&child.to_string(), &["running"], Duration::from_secs(20))
        .await;
    let reply = create_bot(&app, &forged(&app, child).await, args.clone()).await;
    assert!(text(&reply).starts_with("NOT_ALLOWED"), "{}", reply.body);
    let name = else_api::app::bots::BotName::parse("GitHub").unwrap();
    assert!(matches!(
        else_api::db::bots::create_for_turn(&app.db.pool, child, &name, None, None, None).await,
        Err(else_api::db::bots::BotError::Refused(
            else_api::db::bots::Refusal::NotOwnerConversation
        ))
    ));

    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    wait_status(&app, root, "interrupted").await;
    assert_eq!(
        create_bot(&app, &token, args).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert!(matches!(
        else_api::db::bots::create_for_turn(&app.db.pool, root, &name, None, None, None).await,
        Err(else_api::db::bots::BotError::Refused(
            else_api::db::bots::Refusal::NotRunning
        ))
    ));
    assert_eq!(bot_count(&app).await, before);
}

async fn delegation_of(
    app: &TestApp,
    root: Uuid,
) -> (Uuid, Uuid, String, Option<i32>, Option<String>) {
    sqlx::query_as(
        "SELECT parent_bot_id, target_bot_id, status, delivered_attempt, result
         FROM delegations WHERE root_turn_id = ?1",
    )
    .bind(root)
    .fetch_one(&app.db.pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn any_bot_hands_work_to_a_peer_or_the_main_bot_and_gets_the_reply() {
    use common::recorder::{GATE, harness, tool};
    let h = harness(2).await;
    let main = h.app.main_bot().await;
    let (work, family) = tokio::join!(h.app.create_bot("Work"), h.app.create_bot("Family"));

    for (target, task) in [
        (&family, "Find the date of the school recital."),
        (&main, "Note the dentist appointment on Friday."),
    ] {
        let (root, token) = h.running(&work, GATE).await;
        let pending = tool(
            &h.app,
            &token,
            "delegate",
            json!({ "target_bot_id": target, "task": task }),
        )
        .await;
        assert_eq!(pending.result["status"], "pending", "{}", pending.body);
        h.release(root).await;
        wait_status(&h.app, root, "completed").await;
        let (parent, to, status, delivered, result) = delegation_of(&h.app, root).await;
        assert_eq!(
            (parent.to_string(), to.to_string()),
            (work.clone(), target.clone())
        );
        assert_eq!((status.as_str(), delivered), ("completed", Some(2)));
        assert!(result.unwrap().contains(task));
        let attempts = h.inputs(root).await;
        let results = else_api::providers::prompt::outcomes_block(&attempts[1]);
        assert!(results.contains(task), "{results}");
        let rules = else_api::providers::prompt::instructions(&attempts[0], "/p");
        assert!(rules.contains("register it with create_bot"), "{rules}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_bots_waiting_on_each_other_both_finish_with_one_provider_slot() {
    use common::recorder::{GATE, harness, tool};
    let h = harness(1).await;
    let (a, b) = tokio::join!(h.app.create_bot("Alpha"), h.app.create_bot("Beta"));
    let root_a = h.send(&a, GATE).await;
    let root_b = h.send(&b, GATE).await;

    let (_, token) = h.started(root_a).await;
    let asked = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": b, "task": "Alpha asks: what is on the calendar?" }),
    )
    .await;
    assert_eq!(asked.result["status"], "pending", "{}", asked.body);
    h.release(root_a).await;
    wait_status(&h.app, root_a, "waiting").await;

    let (_, token) = h.started(root_b).await;
    let asked = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": a, "task": "Beta asks: which bills are due?" }),
    )
    .await;
    assert_eq!(asked.result["status"], "pending", "{}", asked.body);
    h.release(root_b).await;

    tokio::join!(
        wait_status(&h.app, root_a, "completed"),
        wait_status(&h.app, root_b, "completed"),
    );
    let outcomes = tokio::join!(delegation_of(&h.app, root_a), delegation_of(&h.app, root_b));
    for (_, _, status, delivered, _) in [outcomes.0, outcomes.1] {
        assert_eq!((status.as_str(), delivered), ("completed", Some(2)));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_peer_cancels_its_task_on_the_main_bot() {
    use common::recorder::{GATE, harness, tool};
    let h = harness(1).await;
    let main = h.app.main_bot().await;
    let peer = h.app.create_bot("Work").await;
    let (root, token) = h.running(&peer, GATE).await;
    let pending = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": main, "task": "[synthetic:hang] research the appointment details" }),
    )
    .await;
    assert_eq!(pending.result["status"], "pending", "{}", pending.body);
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    h.release(root).await;
    wait_status(&h.app, root, "waiting").await;
    h.started(child).await;
    let stop = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{peer}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    tokio::join!(
        wait_status(&h.app, root, "interrupted"),
        wait_status(&h.app, child, "interrupted"),
    );
    let (_, _, status, _, _) = delegation_of(&h.app, root).await;
    assert_eq!(status, "cancelled");
}

async fn bot_events(pool: &sqlx::SqlitePool, bot: Uuid) -> Vec<(String, Value)> {
    sqlx::query_as("SELECT kind, data FROM bot_events WHERE bot_id = ?1 ORDER BY seq")
        .bind(bot)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_parent_chat_shows_the_childs_reply_once_before_its_continuation() {
    let app = app_with(1).await;
    let school = leaf(&app, "School").await;
    let (main, root, token) = running_main(&app).await;
    let main_id: Uuid = main.parse().unwrap();
    let task = "Collect the planetarium form details.";
    let reply = delegate(&app, &token, &school, task).await;
    let task_id = tool_text(&reply)["task_id"].as_str().unwrap().to_owned();
    app.wait_turn_for(&root.to_string(), &["completed"], Duration::from_secs(40))
        .await;
    let result: String = sqlx::query_scalar("SELECT result FROM delegations WHERE id = ?1")
        .bind(task_id.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();

    let snapshot = app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await
        .body;
    let items = snapshot["items"].as_array().unwrap();
    let root_items: Vec<&Value> = items
        .iter()
        .filter(|i| i["turn_id"] == root.to_string().as_str())
        .collect();
    let kinds: Vec<&str> = root_items
        .iter()
        .map(|i| i["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["user", "assistant", "status", "assistant"],
        "{snapshot}"
    );
    let shown = root_items[2];
    assert_eq!(shown["id"], task_id.as_str(), "the reply is the delegation");
    assert_eq!(shown["text"], result.as_str());
    assert!(result.contains("planetarium form details"), "{result}");
    assert_eq!(
        shown["bot_reply"],
        json!({ "sender_bot_id": school, "sender_name": "School",
                "prompt": task, "status": "completed" })
    );
    assert_eq!(shown["bot_task"], Value::Null);
    assert!(
        items
            .iter()
            .filter(|i| i["id"] != task_id.as_str())
            .all(|i| i["bot_reply"].is_null()),
        "{snapshot}"
    );

    let paged = app
        .owner(
            "GET",
            &format!(
                "/v1/bots/{main}/conversation?limit=1&before_seq={}",
                shown["seq"].as_i64().unwrap() + 1
            ),
            None,
        )
        .await
        .body;
    assert_eq!(
        paged["items"],
        json!([shown]),
        "pagination shows the same item"
    );

    let events = bot_events(&app.db.pool, main_id).await;
    let upserts: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, (kind, data))| kind == "item.upsert" && data["item"]["id"] == task_id.as_str())
        .map(|(at, _)| at)
        .collect();
    assert_eq!(upserts.len(), 1, "journaled once");
    let at = upserts[0];
    assert_eq!(events[at].1["item"], *shown, "replay matches the snapshot");
    assert_eq!(
        events[at].1["conversation_id"],
        owner_conversation(&app.db.pool, main_id)
            .await
            .to_string()
            .as_str()
    );
    let root_status = |range: &[(String, Value)]| -> Vec<String> {
        range
            .iter()
            .filter(|(kind, data)| {
                kind == "turn.updated" && data["turn"]["id"] == root.to_string().as_str()
            })
            .map(|(_, data)| data["turn"]["status"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        root_status(&events[..at]).last().map(String::as_str),
        Some("waiting"),
        "the root was parked"
    );
    assert_eq!(
        root_status(&events[at..]).get(..2),
        Some(&["queued".to_owned(), "preparing".to_owned()][..]),
        "the root requeues with the reply, then resumes"
    );
    let continuation = root_items[3]["id"].as_str().unwrap();
    assert!(
        events[..at]
            .iter()
            .all(|(_, data)| data["item"]["id"] != continuation),
        "the continuation's text comes after the reply"
    );

    let target = app
        .owner("GET", &format!("/v1/bots/{school}/conversation"), None)
        .await
        .body;
    assert!(
        target["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["bot_reply"].is_null() && i["id"] != task_id.as_str()),
        "the child's own history has no parent reply card: {target}"
    );

    let logged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_entry WHERE bot_id = ?1 AND request_key = ?2",
    )
    .bind(main_id)
    .bind(format!("item:{task_id}"))
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(logged, 0, "the display item creates no memory entry");
    let next = app
        .send(
            &main,
            Uuid::new_v4(),
            "[synthetic:hang] and now?",
            Some("synthetic"),
        )
        .await;
    let next: Uuid = next.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    let input = else_api::db::turns::input(&app.db.pool, next)
        .await
        .unwrap();
    let kinds: Vec<&str> = input.history.iter().map(|h| h.kind.as_str()).collect();
    assert!(!kinds.contains(&"status"), "{kinds:?}");
}

async fn parent_replies(db: &TestDb, main: Uuid) -> (Vec<(String, String, Uuid)>, usize) {
    let snapshot = else_api::db::conversation::snapshot(&db.pool, main, 50, None)
        .await
        .unwrap()
        .unwrap();
    let shown = snapshot
        .items
        .iter()
        .filter_map(|i| {
            let reply = i.bot_reply.as_ref()?;
            assert_eq!(reply.sender_name, "Target");
            assert_eq!(reply.prompt, "seeded task");
            Some((reply.status.clone(), i.text.clone(), i.turn_id))
        })
        .collect();
    let upserts = bot_events(&db.pool, main)
        .await
        .iter()
        .filter(|(kind, data)| kind == "item.upsert" && !data["item"]["bot_reply"].is_null())
        .count();
    (shown, upserts)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_child_shows_its_error_once_however_often_it_is_settled() {
    let db = TestDb::migrated().await;
    let (main, root, child, _) = seed(&db).await;
    sqlx::query("UPDATE turns SET status = 'preparing' WHERE id = ?1")
        .bind(child)
        .execute(&db.pool)
        .await
        .unwrap();
    else_api::db::turns::fail_unstarted(&db.pool, child, "no input")
        .await
        .unwrap();
    else_api::db::turns::fail_unstarted(&db.pool, child, "no input")
        .await
        .unwrap();
    let target: Uuid = sqlx::query_scalar("SELECT bot_id FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    else_api::db::events::turn_changed(&mut tx, target, child)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (shown, upserts) = parent_replies(&db, main).await;
    assert_eq!(shown, [("failed".to_owned(), "no input".to_owned(), root)]);
    assert_eq!(upserts, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_child_keeps_its_partial_reply_and_an_empty_one_shows_empty() {
    let db = TestDb::migrated().await;
    let (main, root, child, _) = seed(&db).await;
    let (conversation, seq): (Uuid, i64) = sqlx::query_as(
        "SELECT c.id, c.last_item_seq + 1 FROM turns t
         JOIN conversations c ON c.bot_id = t.bot_id AND c.kind = 'owner' WHERE t.id = ?1",
    )
    .bind(child)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
         VALUES (?1, ?2, ?3, ?4, 'assistant', 'Half of the form')",
    )
    .bind(Uuid::new_v4())
    .bind(conversation)
    .bind(seq)
    .bind(child)
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE conversations SET last_item_seq = ?2 WHERE id = ?1")
        .bind(conversation)
        .bind(seq)
        .execute(&db.pool)
        .await
        .unwrap();
    else_api::db::turns::recover_after_restart(&db.pool)
        .await
        .unwrap();
    let (shown, upserts) = parent_replies(&db, main).await;
    assert_eq!(
        shown,
        [("cancelled".to_owned(), "Half of the form".to_owned(), root)]
    );
    assert_eq!(upserts, 1);

    let db = TestDb::migrated().await;
    let (main, root, child, _) = seed(&db).await;
    sqlx::query("UPDATE turns SET status = 'completed' WHERE id = ?1")
        .bind(child)
        .execute(&db.pool)
        .await
        .unwrap();
    let target: Uuid = sqlx::query_scalar("SELECT bot_id FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    else_api::db::events::turn_changed(&mut tx, target, child)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (shown, upserts) = parent_replies(&db, main).await;
    assert_eq!(shown, [("completed".to_owned(), String::new(), root)]);
    assert_eq!(upserts, 1);
}
