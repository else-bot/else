//! How a wait checks its job's status, and when it gives up.

use super::*;

async fn wait_for_check(app: &TestApp, turn: Uuid, check: i64) {
    common::until(
        &format!("MCP check {check} for turn {turn}"),
        common::poll(|| async { (wait_row(app, turn).await.2 >= check).then_some(()) }),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_waiting_on_a_running_mcp_job_continues_by_itself_when_it_finishes() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    installed.ok();

    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert!(
        else_api::app::tools::parks("mcp_wait", &parked.body["result"]),
        "{}",
        parked.body
    );
    assert_eq!(parked.result["status"], "waiting");
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    assert_eq!(attempts(app, turn).await, 1);

    check_now(app).await;
    wait_for_check(app, turn, 2).await;
    assert_eq!(wait_row(app, turn).await.0, "pending", "the job still runs");
    assert_eq!(app.wait_turn(turn, &["waiting"]).await, "waiting");
    let shown = shown_waits(app, &main, turn).await;
    assert_eq!(shown.as_array().map(Vec::len), Some(1), "{shown}");
    assert_eq!(shown[0]["mcp_id"], "t3-code", "{shown}");
    let waiting = app.waiting_on(&main).await;
    assert_eq!(
        (&waiting["bots"], &waiting["jobs"]),
        (&json!([]), &json!(1)),
        "the bot list says it waits on its MCP job: {waiting}"
    );

    std::fs::write(&state, "result_available").unwrap();
    check_now(app).await;
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(shown_waits(app, &main, turn).await, json!([]));
    assert_eq!(attempts(app, turn).await, 2, "the same request continued");
    assert_eq!(app.waiting_on(&main).await, Value::Null, "the job ended");
    let (status, delivered, checks) = wait_row(app, turn).await;
    assert_eq!((status.as_str(), delivered), ("finished", Some(2)));
    assert!(checks >= 3, "{checks}");
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("[else.bot MCP waits]"), "{packet}");
    assert!(packet.contains("Opened PR #775"), "{packet}");
    assert!(packet.contains("status=\"finished\""), "{packet}");

    let (_, token) = h.running(&main, GATE).await;
    let done = tool(app, &token, "mcp_wait", wait_args()).await.result;
    assert_eq!(done["status"], "finished", "{done}");
    assert_eq!(done["result"]["content"][0]["type"], "text");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_status_that_cannot_be_read_never_counts_as_the_job_finishing() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    installed.ok();

    let mut misspelled = wait_args();
    misspelled["status_pointer"] = json!("/work_state");
    let refused = tool(app, &token, "mcp_wait", misspelled).await;
    assert_eq!(refused.status, StatusCode::OK, "{}", refused.body);
    assert!(refused.error, "{}", refused.body);
    assert_eq!(
        refused.result["error"]["code"], "unreadable_status",
        "a refusal is the JSON error envelope: {}",
        refused.body
    );
    assert!(
        refused.result.to_string().contains("/work_state"),
        "{}",
        refused.body
    );

    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    std::fs::write(&state, "unreadable").unwrap();
    check_now(app).await;
    wait_for_check(app, turn, 2).await;
    let (status, _, checks) = wait_row(app, turn).await;
    assert_eq!(
        (status.as_str(), checks),
        ("pending", 2),
        "one unreadable status keeps waiting"
    );
    assert_eq!(attempts(app, turn).await, 1);

    for check in [3, 4] {
        check_now(app).await;
        wait_for_check(app, turn, check).await;
    }
    app.wait_turn(turn, &["completed"]).await;
    let (status, delivered, checks) = wait_row(app, turn).await;
    assert_eq!((status.as_str(), delivered, checks), ("failed", Some(2), 4));
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("status=\"failed\""), "{packet}");
    assert!(packet.contains("UNREADABLE_STATUS"), "{packet}");
    assert!(packet.contains("upstream schema changed"), "{packet}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_status_tool_not_declared_repeatable_is_refused_and_a_deadline_ends_a_wait() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    installed.ok();
    let mut poke = wait_args();
    poke["tool"] = json!("task_poke");
    let refused = tool(app, &token, "mcp_wait", poke).await;
    assert_eq!(refused.status, StatusCode::OK, "{}", refused.body);
    assert!(refused.error, "{}", refused.body);
    assert_eq!(
        refused.result["error"]["code"], "not_repeatable",
        "{}",
        refused.body
    );
    assert!(!else_api::app::tools::parks(
        "mcp_wait",
        &refused.body["result"]
    ));
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM turn_holds WHERE turn_id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(held, 0, "a refused tool neither waits nor asks the owner");
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;

    let (next, token) = h.running(&main, GATE).await;
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(next).await;
    app.wait_turn(next, &["waiting"]).await;
    sqlx::query("UPDATE mcp_waits SET deadline_at = '2000-01-01T00:00:00.000+00:00', next_check_at = '2999-01-01T00:00:00.000+00:00' WHERE turn_id = ?1")
        .bind(next)
        .execute(&app.db.pool)
        .await
        .unwrap();
    app.wait_turn(next, &["completed"]).await;
    let (status, delivered, checks) = wait_row(app, next).await;
    assert_eq!(
        (status.as_str(), delivered, checks),
        ("expired", Some(2), 1),
        "expired without another call"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_package_with_many_due_waits_does_not_hide_another_package() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    installed.ok();
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 70)
         INSERT INTO mcp_waits (id, bot_id, turn_id, attempt, mcp_id, tool, arguments,
             status_pointer, running_values, purpose, interval_seconds, deadline_at, next_check_at)
         SELECT randomblob(16), bot_id, turn_id, attempt + i, CASE WHEN i = 70 THEN 'linear' ELSE mcp_id END,
             tool, arguments, status_pointer, running_values, purpose, interval_seconds,
             '2999-01-01T00:00:00.000+00:00', '2000-01-01T00:00:00.000+00:00'
         FROM mcp_waits, n",
    )
    .execute(&app.db.pool)
    .await
    .unwrap();
    let packages = |due: Vec<else_api::db::mcp_waits::Due>| {
        let mut ids: Vec<String> = due.into_iter().map(|w| w.mcp_id).collect();
        ids.sort();
        ids
    };
    let busy = else_api::db::mcp_waits::due(&app.db.pool, &["t3-code".into()], 3)
        .await
        .unwrap();
    assert_eq!(packages(busy), ["linear"]);
    let idle = else_api::db::mcp_waits::due(&app.db.pool, &[], 3)
        .await
        .unwrap();
    assert_eq!(packages(idle), ["linear", "t3-code"], "one per package");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_external_first_call_waits_for_the_owner_and_a_tool_that_stops_qualifying_is_not_repeated()
 {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    installed.ok();
    let mut external = wait_args();
    external["effect"] = json!("external");
    let asked = tool(app, &token, "mcp_wait", external).await.result;
    assert_eq!(asked["parked"], true, "{asked}");
    assert_eq!(calls(&state), 0, "nothing runs before the owner approves");
    let waits: i64 = sqlx::query_scalar("SELECT count(*) FROM mcp_waits")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(waits, 0);
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;

    follow(app, &main, turn).await;
    let token = h.continued(turn, &token).await;
    assert_eq!(attempts(app, turn).await, 2);
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(turn).await;
    rests_after(app, turn, 2, "waiting", "the reply parks on its wait").await;
    assert_eq!(calls(&state), 1);
    std::fs::write(state.with_extension("unsafe"), "").unwrap();
    check_now(app).await;
    h.continued(turn, &token).await;
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await.0, "failed");
    assert_eq!(
        calls(&state),
        1,
        "a tool that no longer qualifies is not called again"
    );
}
