//! A delegating request that waits on its tasks: parking, limits, stops, and
//! owner messages that arrive while it waits.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_child_parks_the_parent_and_a_continuation_uses_its_result() {
    let h = harness(TestApp::builder().max_active(1)).await;
    let app = &h.app;
    let school = leaf(app, "School").await;
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE).await;

    let reply = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &school, "task": "Collect the planetarium form details." }),
    )
    .await;
    let pending = reply.result;
    assert_eq!(pending["status"], "pending", "{}", reply.body);
    let task_id: Uuid = pending["task_id"].as_str().unwrap().parse().unwrap();
    let again = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &school, "task": "Collect the planetarium form details." }),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT, "the attempt is parked");

    h.release(root).await;
    app.wait_turn(root, &["completed"]).await;
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

    let parent = app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await
        .body;
    let replies: Vec<&Value> = parent["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["turn_id"] == root.to_string().as_str() && i["kind"] == "assistant")
        .map(|i| &i["attempt"])
        .collect();
    assert_eq!(
        replies,
        [&json!(1), &json!(2)],
        "each reply keeps its attempt: {parent}"
    );
    let turn = parent["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == root.to_string().as_str())
        .unwrap();
    assert_eq!(turn["attempt"], 2);

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
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let work = leaf(app, "Work").await;
    let family = leaf(app, "Family").await;
    let gone = leaf(app, "Gone").await;
    app.owner("DELETE", &format!("/v1/bots/{gone}"), None).await;
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE).await;

    assert_eq!(
        tool(
            app,
            &token,
            "delegate",
            json!({ "target_bot_id": main, "task": "do it yourself" })
        )
        .await
        .code(),
        "SELF_DELEGATION"
    );
    for missing in [gone, Uuid::new_v4().to_string()] {
        let unavailable = tool(
            app,
            &token,
            "delegate",
            json!({ "target_bot_id": missing, "task": "anything" }),
        )
        .await;
        assert_eq!(unavailable.code(), "BOT_UNAVAILABLE");
    }
    let no_delegate = h.grant_with(root, vec![Tool::ReadMemory]).await;
    assert_eq!(
        tool(
            app,
            &no_delegate,
            "delegate",
            json!({ "target_bot_id": work, "task": "x" })
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    let first = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &work, "task": format!("{GATE} first task") }),
    )
    .await
    .result;
    assert_eq!(first["status"], "pending");
    h.release(root).await;
    app.wait_turn(
        root,
        &["waiting", "queued", "preparing", "running", "completed"],
    )
    .await;
    let second = else_api::db::delegations::create(
        &app.db.pool,
        root,
        family.parse().unwrap(),
        "second in the same attempt",
        None,
        &[],
        None,
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
    h.started(child).await;
    let loop_back = else_api::db::delegations::create(
        &app.db.pool,
        child,
        main.parse().unwrap(),
        "loop back",
        None,
        &[],
        None,
    )
    .await;
    assert!(
        matches!(
            loop_back,
            Err(else_api::db::delegations::DelegateError::UpstreamTarget)
                | Err(else_api::db::delegations::DelegateError::NotRunning)
        ),
        "{loop_back:?}"
    );
    h.release(child).await;
    app.wait_turn(root, &["completed"]).await;
    let children: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(children, 1);

    let (root, _) = h.running(&main, GATE).await;
    let budget = else_api::db::delegations::MAX_CHILDREN as usize;
    for (n, target) in [&work, &family].iter().cycle().take(budget).enumerate() {
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
            None,
            &[],
            None,
        )
        .await
        .unwrap();
    }
    sqlx::query("UPDATE turns SET attempt = ?2 WHERE id = ?1")
        .bind(root)
        .bind(budget as i32 + 1)
        .execute(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        else_api::db::delegations::create(
            &app.db.pool,
            root,
            work.parse().unwrap(),
            "one past the budget",
            None,
            &[],
            None
        )
        .await,
        Err(else_api::db::delegations::DelegateError::BudgetExhausted)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_keeps_a_task_the_owner_stops_on_its_own_and_deletion_resolves_it() {
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let work = leaf(app, "Work").await;
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE).await;
    let reply = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &work, "task": "[synthetic:hang] long research" }),
    )
    .await;
    let task_id: Uuid = reply.result["task_id"].as_str().unwrap().parse().unwrap();
    h.release(root).await;
    app.wait_turn(root, &["waiting"]).await;
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
    let task_status = || async {
        sqlx::query_as::<_, (String, Option<String>, Option<i32>)>(
            "SELECT status, error, delivered_attempt FROM delegations WHERE id = ?1",
        )
        .bind(task_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
    };
    common::never_polled(
        "Stop ends the delegated task",
        Duration::from_secs(1),
        || async {
            let status = task_status().await.0;
            (status != "running").then_some(status)
        },
    )
    .await;
    app.wait_turn(root, &["waiting"]).await;

    let cancel = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/waits/{task_id}/cancel"),
            None,
        )
        .await;
    assert_eq!(cancel.status, StatusCode::OK, "{}", cancel.body);
    assert_eq!(cancel.body["kind"], "task", "{}", cancel.body);
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(task_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    app.wait_turn(child, &["interrupted"]).await;
    app.wait_turn(root, &["completed"]).await;
    let (status, _, delivered) = task_status().await;
    assert_eq!((status.as_str(), delivered), ("cancelled", Some(2)));
    let again = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/waits/{task_id}/cancel"),
            None,
        )
        .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(again.body["created"], false, "a retry returns the stop");

    let (root, token) = h.running(&main, GATE).await;
    let reply = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &work, "task": "[synthetic:hang] another long task" }),
    )
    .await;
    let task_id: Uuid = reply.result["task_id"].as_str().unwrap().parse().unwrap();
    h.release(root).await;
    app.wait_turn(root, &["waiting"]).await;
    app.owner("DELETE", &format!("/v1/bots/{work}"), None).await;
    app.wait_turn(root, &["completed"]).await;
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
async fn an_owner_follow_up_keeps_earlier_tasks_running_and_its_reply_receives_every_result() {
    let h = harness(TestApp::builder().max_active(3)).await;
    let app = &h.app;
    let growth = leaf(app, "Growth").await;
    let scout = leaf(app, "Scout").await;
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let audit = format!("{GATE} Check the affected Search Console URLs and the X Ads campaign.");
    let reply = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &growth, "task": &audit }),
    )
    .await;
    let audit_id: Uuid = reply.result["task_id"].as_str().unwrap().parse().unwrap();
    h.release(root).await;
    app.wait_turn(root, &["waiting"]).await;
    let audit_child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
            .bind(audit_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    h.started(audit_child).await;

    let follow = app
        .send(
            &main,
            Uuid::new_v4(),
            "Also learn from Scout how to generate VIP4 redeem links.",
            Some("synthetic"),
        )
        .await;
    assert_eq!(follow.status, StatusCode::ACCEPTED, "{}", follow.body);
    assert_eq!(follow.body["turn"]["steered_into"], root.to_string());
    let follow: Uuid = follow.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    let token = h.continued(root, &token).await;
    let links = format!("{GATE} Explain how to generate VIP4 redeem links.");
    let reply = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": scout, "task": links }),
    )
    .await;
    let links_id: Uuid = reply.result["task_id"].as_str().unwrap().parse().unwrap();

    h.release(root).await;
    app.wait_turn(root, &["waiting"]).await;
    h.release(audit_child).await;
    let links_child = child_of(app, links_id).await;
    let token = h.continued(root, &token).await;
    h.release(root).await;
    app.wait_turn(root, &["waiting"]).await;
    h.release(links_child).await;
    h.continued(root, &token).await;
    h.release(root).await;
    app.wait_turn(root, &["completed"]).await;
    app.wait_turn(follow, &["completed"]).await;
    let tasks: Vec<(Uuid, Uuid, String, Option<i64>)> = sqlx::query_as(
        "SELECT id, root_turn_id, status, delivered_attempt FROM delegations
         WHERE id IN (?1, ?2) ORDER BY created_at",
    )
    .bind(audit_id)
    .bind(links_id)
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(tasks.len(), 2);
    for (id, carrier, status, delivered) in tasks {
        assert_eq!(carrier, root, "{id} reports to the reply");
        assert_eq!(status, "completed", "{id} kept running after the follow-up");
        assert!(delivered.is_some(), "{id} reached the reply");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_aside_waits_for_a_running_delegated_task() {
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let work = leaf(app, "Work").await;
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE).await;
    let reply = tool(app, &token, "delegate", json!({ "target_bot_id": &work, "task": format!("{GATE} Merge the landing PR into dev once its checks pass.") }))
    .await;
    let task_id: Uuid = reply.result["task_id"].as_str().unwrap().parse().unwrap();
    h.release(root).await;
    let child = child_of(app, task_id).await;
    app.wait_turn(child, &["running"]).await;

    let aside = "Release a patch.";
    let sent = app
        .send(&work, Uuid::new_v4(), aside, Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["status"], "queued", "{}", sent.body);
    let aside_turn: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    let takeover: Option<Uuid> =
        sqlx::query_scalar("SELECT takeover_turn_id FROM delegations WHERE id = ?1")
            .bind(task_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(takeover, None, "an aside leaves the task to its bot");

    h.release(child).await;
    let (_, result, _) = wait_task(app, task_id, "completed").await;
    assert_eq!(child_of(app, task_id).await, child);
    app.wait_turn(child, &["completed"]).await;
    assert!(
        result.as_deref().is_some_and(|r| !r.contains(aside)),
        "the delegating bot gets the task's own reply: {result:?}"
    );
    app.wait_turn(aside_turn, &["completed"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_aside_runs_after_the_running_task_before_other_queued_tasks() {
    let h = harness(TestApp::builder().max_active(3)).await;
    let app = &h.app;
    let work = leaf(app, "Work").await;
    let other = leaf(app, "Other").await;
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE).await;
    tool(app, &token, "delegate", json!({ "target_bot_id": &work, "task": format!("{GATE} Merge the landing PR into dev once its checks pass.") }))
    .await;
    h.release(root).await;
    let (other_root, other_token) = h.running(&other, GATE).await;
    tool(
        app,
        &other_token,
        "delegate",
        json!({ "target_bot_id": &work, "task": "[synthetic:hang] second research" }),
    )
    .await;
    h.release(other_root).await;
    let children: Vec<Uuid> = sqlx::query_scalar(
        "SELECT d.child_turn_id FROM delegations d JOIN turns t ON t.id = d.child_turn_id
         WHERE d.target_bot_id = ?1 ORDER BY t.turn_no",
    )
    .bind(work.parse::<Uuid>().unwrap())
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    let [first, second] = children[..] else {
        panic!("two tasks: {children:?}");
    };
    app.wait_turn(first, &["running"]).await;

    let sent = app
        .send(&work, Uuid::new_v4(), "Release a patch.", Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let aside: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;
    app.wait_turn(aside, &["completed"]).await;
    let second_status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(second)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let second_started: Option<String> =
        sqlx::query_scalar("SELECT started_at FROM turns WHERE id = ?1")
            .bind(second)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    let aside_started: String = sqlx::query_scalar("SELECT started_at FROM turns WHERE id = ?1")
        .bind(aside)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert!(
        second_started.is_none_or(|second| second > aside_started),
        "the aside ran before the other queued task ({second_status})"
    );
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
    let shown = serde_json::to_value(else_wire::turns::Turn::from(turn)).unwrap();
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
    for status in [TurnStatus::Running, TurnStatus::Completed] {
        let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        common::seed::force_status(&mut *tx, child, status).await;
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

/// A delegating bot's activity names the bots it waits on only while its
/// turn is parked on their tasks.
#[tokio::test(flavor = "multi_thread")]
async fn the_delegating_bot_shows_whom_it_waits_on_until_the_task_ends() {
    let db = TestDb::migrated().await;
    let (main, root, child, _) = seed(&db).await;
    let target: Uuid = sqlx::query_scalar("SELECT bot_id FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let waiting_on = || async {
        let bot = else_api::db::bots::find(&db.pool, main)
            .await
            .unwrap()
            .unwrap();
        serde_json::to_value(else_wire::bots::Bot::from(&bot)).unwrap()["activity"]["waiting_on"]
            .clone()
    };
    assert_eq!(waiting_on().await, Value::Null, "a running turn is working");

    common::seed::force_status(&db.pool, root, TurnStatus::Waiting).await;
    let shown = waiting_on().await;
    assert_eq!(
        shown["bots"],
        json!([{ "id": target.to_string(), "name": "Target", "tasks": 1 }])
    );
    assert_eq!(shown["jobs"], 0, "{shown}");
    assert!(shown["since"].is_string(), "{shown}");

    sqlx::query(
        "INSERT INTO mcp_waits (id, bot_id, turn_id, attempt, mcp_id, tool, arguments,
             status_pointer, running_values, purpose, interval_seconds, deadline_at, next_check_at)
         VALUES (?1, ?2, ?3, 1, 't3-code', 'task_status', '{}', '/workState', '[\"working\"]',
             'Review the PR', 60, '2099-01-01T00:00:00.000+00:00', '2099-01-01T00:00:00.000+00:00')",
    )
    .bind(Uuid::new_v4())
    .bind(main)
    .bind(root)
    .execute(&db.pool)
    .await
    .unwrap();
    let both = waiting_on().await;
    assert_eq!(
        (&both["bots"][0]["name"], &both["jobs"]),
        (&json!("Target"), &json!(1)),
        "a turn waiting on a task and a job names both: {both}"
    );
    assert_eq!(both["since"], shown["since"], "the older task leads");
    sqlx::query("UPDATE mcp_waits SET status = 'stopped' WHERE turn_id = ?1")
        .bind(root)
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(waiting_on().await, shown, "a stopped wait no longer counts");

    for status in [TurnStatus::Running, TurnStatus::Completed] {
        common::seed::force_status(&db.pool, child, status).await;
        if status == TurnStatus::Running {
            assert_eq!(waiting_on().await["bots"][0]["tasks"], 1, "a running task");
        }
    }
    assert_eq!(
        waiting_on().await,
        Value::Null,
        "the ended task woke the turn"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_delegating_turn_hears_a_tasks_progress_while_its_message_streams() {
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let growth = leaf(app, "Growth").await;
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE).await;
    let task = format!("{GATE_AFTER_OPENING} Audit the indexing notice.");
    let reply = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &growth, "task": &task }),
    )
    .await;
    let task_id: Uuid = reply.result["task_id"].as_str().unwrap().parse().unwrap();
    let child = child_of(app, task_id).await;
    let task_id = task_id.to_string();
    h.release(root).await;
    let main: Uuid = main.parse().unwrap();
    let deadline = std::time::Instant::now() + common::SETTLE;
    loop {
        let heard = turn_events(&app.db.pool, main, root)
            .await
            .into_iter()
            .any(|(_, data)| {
                data["turn"]["delegations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|d| {
                        d["id"] == task_id.as_str()
                            && d["status"] == "running"
                            && d["progress"]
                                .as_str()
                                .is_some_and(|p| p.starts_with("Synthetic reply"))
                    })
            });
        if heard {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no progress reached the delegating turn while the task ran"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    h.release(child).await;
}
