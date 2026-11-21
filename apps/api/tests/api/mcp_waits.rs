use crate::common;

use std::path::Path;
use std::time::Duration;

use axum::http::StatusCode;
use common::recorder::{GATE, GATE_EVERY_ATTEMPT, harness, tool};
use common::{TestApp, TestDb};
use serde_json::{Value, json};
use uuid::Uuid;

const SERVER: &str = r#"
import json, os, sys
state = os.environ['JOB_STATE']
for line in sys.stdin:
    m = json.loads(line)
    if 'id' not in m: continue
    if m['method'] == 'initialize':
        r = {'protocolVersion': m['params']['protocolVersion'], 'serverInfo': {'name':'fixture','version':'1'}, 'capabilities':{'tools':{}}}
    elif m['method'] == 'tools/list' and not m['params'].get('cursor'):
        r = {'tools':[{'name':'task_poke','inputSchema':{'type':'object'},'annotations':{'idempotentHint':True}}], 'nextCursor':'2'}
    elif m['method'] == 'tools/list':
        hints = {} if os.path.exists(state + '.unsafe') else {'readOnlyHint':False,'destructiveHint':False,'idempotentHint':True}
        r = {'tools':[{'name':'task_status','inputSchema':{'type':'object'},'annotations':hints}]}
    elif m['method'] == 'tools/call':
        open(state + '.calls', 'a').write(m['params']['name'] + '\n')
        own = state + '.' + m['params']['arguments']['taskId']
        work = open(own if os.path.exists(own) else state).read().strip()
        body = {'taskId': m['params']['arguments']['taskId'], 'workState': work, 'summary': 'Opened PR #775' if work == 'result_available' else None}
        if work == 'unreadable': body = {'taskId': body['taskId'], 'error': 'upstream schema changed'}
        r = {'content':[{'type':'text','text':json.dumps(body)}]}
    else: r = {}
    print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':r}), flush=True)
"#;

fn install(state: &Path) -> Value {
    json!({"id":"t3-code","name":"T3 Code","description":"Delegate coding tasks","skill":"Check tasks with task_status.","documentation":"task_status {taskId}","config":{"type":"stdio","command":"python3","args":["-u","-c",SERVER],"env":{"JOB_STATE":state.to_str().unwrap()}}})
}

fn wait_args() -> Value {
    json!({"id":"t3-code","tool":"task_status","arguments":{"taskId":"WEB-284"},
           "status_pointer":"/workState","running_values":["working","waiting_for_children"],"effect":"local",
           "purpose":"Review the WEB-284 PR once T3 finishes.","interval_seconds":15})
}

async fn wait_row(app: &TestApp, turn: Uuid) -> (String, Option<i32>, i64) {
    sqlx::query_as("SELECT status, delivered_attempt, checks FROM mcp_waits WHERE turn_id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn check_now(app: &TestApp) {
    sqlx::query("UPDATE mcp_waits SET next_check_at = '2000-01-01T00:00:00.000+00:00' WHERE status = 'pending'")
        .execute(&app.db.pool)
        .await
        .unwrap();
}

async fn wait_for_check(app: &TestApp, turn: Uuid, check: i64) {
    common::until(
        &format!("MCP check {check} for turn {turn}"),
        common::poll(|| async { (wait_row(app, turn).await.2 >= check).then_some(()) }),
    )
    .await;
}

async fn attempts(app: &TestApp, turn: Uuid) -> i32 {
    sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn shown_waits(app: &TestApp, bot: &str, turn: Uuid) -> Value {
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body;
    snapshot["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .unwrap()["mcp_waits"]
        .clone()
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
    assert!(!installed.error, "{}", installed.body);

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
    assert!(!installed.error, "{}", installed.body);

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
async fn a_wait_survives_a_restart_during_and_after_the_owners_stop() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    assert!(!installed.error, "{}", installed.body);
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    // A restart lands while the owner's Stop unwinds a reply that carries
    // this wait: recovery parks the turn on it instead of cancelling it.
    sqlx::query("UPDATE turns SET status = 'cancelling', error_code = 'stopped' WHERE id = ?1")
        .bind(turn)
        .execute(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(wait_row(app, turn).await.0, "pending");

    let db = h.app.stop().await;
    let h = harness(TestApp::builder().db(db)).await;
    let app = &h.app;
    assert_eq!(app.wait_turn(turn, &["waiting"]).await, "waiting");
    assert_eq!(wait_row(app, turn).await.0, "pending");
    let stopped = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert_eq!(stopped.status, StatusCode::OK, "{}", stopped.body);
    common::never_polled(
        "Stop settles the MCP wait",
        Duration::from_secs(1),
        || async {
            let status = wait_row(app, turn).await.0;
            (status != "pending").then_some(status)
        },
    )
    .await;
    assert_eq!(
        app.wait_turn(turn, &["waiting"]).await,
        "waiting",
        "Stop ends generation only; the turn keeps waiting"
    );
    std::fs::write(&state, "result_available").unwrap();
    check_now(app).await;
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await.0, "finished");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owner_stops_one_wait_and_the_bot_learns_it_was_cancelled() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    assert!(!installed.error, "{}", installed.body);
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    std::fs::write(&state, "result_available").unwrap();
    check_now(app).await;
    app.wait_turn(turn, &["completed"]).await;
    let finished: Uuid = sqlx::query_scalar("SELECT id FROM mcp_waits WHERE turn_id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();

    std::fs::write(&state, "working").unwrap();
    let (turn, token) = h.running(&main, GATE).await;
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let shown = shown_waits(app, &main, turn).await;
    assert_eq!(
        shown[0]["purpose"], "Review the WEB-284 PR once T3 finishes.",
        "{shown}"
    );
    assert_eq!(shown[0]["last_status"], "working", "{shown}");
    let wait = shown[0]["id"].as_str().unwrap().to_owned();
    let cancel = |bot: String, id: String| async move {
        app.owner("POST", &format!("/v1/bots/{bot}/waits/{id}/cancel"), None)
            .await
    };
    let first = cancel(main.clone(), wait.clone()).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(
        (&first.body["kind"], &first.body["created"]),
        (&json!("mcp"), &json!(true))
    );
    app.wait_turn(turn, &["completed"]).await;
    let (status, delivered, _) = wait_row(app, turn).await;
    assert_eq!((status.as_str(), delivered), ("stopped", Some(2)));
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("status=\"stopped\""), "{packet}");
    assert!(packet.contains("cancelled by owner"), "{packet}");

    let again = cancel(main.clone(), wait).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(again.body["created"], false, "a retry returns the stop");
    let ended = cancel(main.clone(), finished.to_string()).await;
    assert_eq!(ended.status, StatusCode::CONFLICT, "{}", ended.body);
    assert_eq!(ended.body["error"]["code"], "wait_ended", "{}", ended.body);
    let other = app.create_bot("Scout").await;
    let foreign = cancel(other, finished.to_string()).await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND, "{}", foreign.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_continued_reply_keeps_its_waits() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    assert!(!installed.error, "{}", installed.body);
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;

    let next = follow(app, &main, first).await;
    let token = h.continued(first, &token).await;
    assert_eq!(attempts(app, first).await, 2);
    let stopped = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stopped.status.is_success(), "{}", stopped.body);
    app.wait_turn(first, &["waiting"]).await;
    assert_eq!(turn_status(app, next).await, "interrupted");
    assert_eq!(carried(app, first).await, vec![(1, "pending".into())]);

    std::fs::write(&state, "result_available").unwrap();
    check_now(app).await;
    h.continued(first, &token).await;
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;
    assert_eq!(attempts(app, first).await, 3);
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("Opened PR #775"), "{packet}");
}

/// Send a follow-up that joins the waiting turn. Returns the message's own turn.
async fn follow(app: &TestApp, bot: &str, into: Uuid) -> Uuid {
    let sent = app
        .send(
            bot,
            Uuid::new_v4(),
            "Any news on WEB-284?",
            Some("synthetic"),
        )
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["steered_into"], into.to_string());
    sent.body["turn"]["id"].as_str().unwrap().parse().unwrap()
}

async fn turn_status(app: &TestApp, turn: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

fn wait_on(task: &str, purpose: &str) -> Value {
    let mut args = wait_args();
    args["arguments"]["taskId"] = json!(task);
    args["purpose"] = json!(purpose);
    args
}

async fn carried(app: &TestApp, turn: Uuid) -> Vec<(i32, String)> {
    sqlx::query_as("SELECT attempt, status FROM mcp_waits WHERE turn_id = ?1 ORDER BY created_at")
        .bind(turn)
        .fetch_all(&app.db.pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn follow_ups_join_the_reply_and_each_of_its_waits_reports_once() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    assert!(!installed.error, "{}", installed.body);
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;

    let second = follow(app, &main, first).await;
    let token = h.continued(first, &token).await;
    assert_eq!(attempts(app, first).await, 2);
    let deploy = wait_on("WEB-286", "Check the WEB-286 deploy.");
    let parked = tool(app, &token, "mcp_wait", deploy).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(first).await;
    rests_after(app, first, 2, "waiting", "the reply parks on both waits").await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("Any news on WEB-284?"), "{packet}");

    let latest = follow(app, &main, first).await;
    let token = h.continued(first, &token).await;
    assert_eq!(attempts(app, first).await, 3);
    let merge = wait_on("WEB-285", "Merge WEB-285 once T3 finishes.");
    let parked = tool(app, &token, "mcp_wait", merge).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(first).await;
    rests_after(app, first, 3, "waiting", "the reply parks on three waits").await;
    let shown = shown_waits(app, &main, first).await;
    assert_eq!(shown.as_array().map(Vec::len), Some(3), "{shown}");

    std::fs::write(dir.path().join("job.WEB-285"), "result_available").unwrap();
    check_now(app).await;
    let token = h.continued(first, &token).await;
    h.release(first).await;
    rests_after(
        app,
        first,
        4,
        "waiting",
        "one result continues the reply while its other waits still run",
    )
    .await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("Merge WEB-285"), "{packet}");
    assert!(packet.contains("Opened PR #775"), "{packet}");
    assert_eq!(
        carried(app, first).await,
        vec![
            (1, "pending".into()),
            (2, "pending".into()),
            (3, "finished".into())
        ]
    );

    std::fs::write(dir.path().join("job.WEB-284"), "result_available").unwrap();
    check_now(app).await;
    let token = h.continued(first, &token).await;
    h.release(first).await;
    rests_after(app, first, 5, "waiting", "one wait is still pending").await;
    std::fs::write(dir.path().join("job.WEB-286"), "result_available").unwrap();
    check_now(app).await;
    h.continued(first, &token).await;
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;
    for message in [second, latest] {
        app.wait_turn(message, &["completed"]).await;
        assert_eq!(
            attempts(app, message).await,
            0,
            "a joined message never runs"
        );
    }
    let delivered: Vec<(String, Option<i32>)> = sqlx::query_as(
        "SELECT status, delivered_attempt FROM mcp_waits WHERE turn_id = ?1 ORDER BY created_at",
    )
    .bind(first)
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert!(
        delivered
            .iter()
            .all(|(status, at)| status == "finished" && at.is_some()),
        "{delivered:?}"
    );
    let packets = h.packets().await;
    for purpose in [
        "Review the WEB-284 PR",
        "Check the WEB-286 deploy",
        "Merge WEB-285",
    ] {
        assert_eq!(
            packets
                .iter()
                .filter(|p| p.contains(&format!("Purpose: {purpose}")))
                .count(),
            1,
            "each result arrives once: {purpose}"
        );
    }
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
    assert!(!installed.error, "{}", installed.body);
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
    assert!(!installed.error, "{}", installed.body);
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

fn calls(state: &Path) -> usize {
    std::fs::read_to_string(state.with_extension("calls"))
        .unwrap_or_default()
        .lines()
        .count()
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
    assert!(!installed.error, "{}", installed.body);
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

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_task_waits_on_a_job_and_its_delegator_resumes_only_when_it_finishes() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let dev = app.create_bot("Dev").await;
    sqlx::query("UPDATE bots SET selected_provider_id = 'synthetic' WHERE id = ?1")
        .bind(dev.parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
    let (root, token) = h.running(&main, GATE).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    assert!(!installed.error, "{}", installed.body);
    let task = format!("{GATE} Run WEB-284 in T3, review its PR, and report back.");
    let accepted = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": dev, "task": task }),
    )
    .await
    .result;
    assert_eq!(accepted["status"], "pending", "{accepted}");
    let (delegation, child): (Uuid, Uuid) =
        sqlx::query_as("SELECT id, child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    let (_, worker) = h.started(child).await;
    let parked = tool(app, &worker, "mcp_wait", wait_args()).await.result;
    assert_eq!(parked["parked"], true, "{parked}");
    assert!(
        parked["next"].as_str().unwrap().contains("without a reply"),
        "{parked}"
    );
    h.release(child).await;
    app.wait_turn(child, &["waiting"]).await;
    h.release(root).await;
    app.wait_turn(root, &["waiting"]).await;
    let pending: String = sqlx::query_scalar("SELECT status FROM delegations WHERE id = ?1")
        .bind(delegation)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(pending, "running", "parking does not settle the delegation");

    let owner = app
        .send(
            &dev,
            Uuid::new_v4(),
            "What is on your plate?",
            Some("synthetic"),
        )
        .await;
    assert_eq!(owner.status, StatusCode::ACCEPTED, "{}", owner.body);
    app.wait_turn(owner.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    assert_eq!(
        app.wait_turn(child, &["waiting"]).await,
        "waiting",
        "the owner's work ran while the task kept waiting"
    );
    assert_eq!(attempts(app, root).await, 1);

    std::fs::write(&state, "result_available").unwrap();
    check_now(app).await;
    app.wait_turn(child, &["completed"]).await;
    assert_eq!(attempts(app, child).await, 2, "the same task continued");
    assert_eq!(wait_row(app, child).await.0, "finished");
    app.wait_turn(root, &["completed"]).await;
    assert_eq!(attempts(app, root).await, 2, "the delegator resumed once");
    let (status, delivered): (String, Option<i32>) =
        sqlx::query_as("SELECT status, delivered_attempt FROM delegations WHERE id = ?1")
            .bind(delegation)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), delivered), ("completed", Some(2)));
    let packets = h.packets().await;
    assert!(
        packets
            .iter()
            .any(|p| p.contains("[else.bot MCP waits]") && p.contains("Opened PR #775")),
        "the task's continuation got the job's result"
    );
    assert!(
        packets
            .last()
            .unwrap()
            .contains("[else.bot delegation results]"),
        "{:?}",
        packets.last()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_settled_wait_continues_a_reply_whose_task_still_runs() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let dev = app.create_bot("Dev").await;
    sqlx::query("UPDATE bots SET selected_provider_id = 'synthetic' WHERE id = ?1")
        .bind(dev.parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
    let (root, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    assert!(!installed.error, "{}", installed.body);
    let task = json!({ "target_bot_id": dev, "task": "[synthetic:hang] Audit the billing code." });
    let accepted = tool(app, &token, "delegate", task).await;
    assert_eq!(accepted.result["status"], "pending", "{}", accepted.body);
    h.release(root).await;
    app.wait_turn(root, &["waiting"]).await;

    follow(app, &main, root).await;
    let token = h.continued(root, &token).await;
    assert_eq!(attempts(app, root).await, 2);
    let task_root: Uuid =
        sqlx::query_scalar("SELECT root_turn_id FROM delegations WHERE status = 'running'")
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(task_root, root, "the task stays with its reply");
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(root).await;
    rests_after(
        app,
        root,
        2,
        "waiting",
        "the reply parks on its task and wait",
    )
    .await;

    std::fs::write(&state, "result_available").unwrap();
    check_now(app).await;
    h.continued(root, &token).await;
    h.release(root).await;
    rests_after(
        app,
        root,
        3,
        "waiting",
        "the settled wait continues the reply while its task still runs",
    )
    .await;
    assert_eq!(wait_row(app, root).await.1, Some(3));
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("Opened PR #775"), "{packet}");
}

/// Waits until `turn` has run `attempt` attempts and rests in `status`.
async fn rests_after(app: &TestApp, turn: Uuid, attempt: i32, status: &str, why: &str) {
    let deadline = std::time::Instant::now() + common::SETTLE;
    loop {
        let now: (i32, String) = sqlx::query_as("SELECT attempt, status FROM turns WHERE id = ?1")
            .bind(turn)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
        if (now.0, now.1.as_str()) == (attempt, status) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{why}: {now:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn each_wait_that_settles_continues_the_reply_while_the_others_run() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (latest, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    assert!(!installed.error, "{}", installed.body);
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(latest).await;
    app.wait_turn(latest, &["waiting"]).await;
    follow(app, &main, latest).await;
    let token = h.continued(latest, &token).await;
    assert_eq!(attempts(app, latest).await, 2);
    let deploy = wait_on("WEB-286", "Check the WEB-286 deploy.");
    let parked = tool(app, &token, "mcp_wait", deploy).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(latest).await;
    rests_after(app, latest, 2, "waiting", "the reply parks on two waits").await;
    follow(app, &main, latest).await;
    let token = h.continued(latest, &token).await;
    assert_eq!(attempts(app, latest).await, 3);
    let merge = wait_on("WEB-285", "Merge WEB-285 once T3 finishes.");
    let parked = tool(app, &token, "mcp_wait", merge).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(latest).await;
    rests_after(app, latest, 3, "waiting", "the reply parks on three waits").await;

    std::fs::write(dir.path().join("job.WEB-286"), "result_available").unwrap();
    check_now(app).await;
    let token = h.continued(latest, &token).await;
    h.release(latest).await;
    rests_after(
        app,
        latest,
        4,
        "waiting",
        "a wait that settles continues the reply while the others run",
    )
    .await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("Purpose: Check the WEB-286 deploy"),
        "{packet}"
    );
    assert!(
        packet.contains("Merge WEB-285 once T3 finishes.")
            && packet.contains("Review the WEB-284 PR once T3 finishes."),
        "the continuation lists the waits still running: {packet}"
    );

    std::fs::write(dir.path().join("job.WEB-284"), "result_available").unwrap();
    check_now(app).await;
    let token = h.continued(latest, &token).await;
    h.release(latest).await;
    rests_after(
        app,
        latest,
        5,
        "waiting",
        "a continuation that parked nothing still wakes on the next wait",
    )
    .await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("Purpose: Review the WEB-284 PR"),
        "{packet}"
    );

    std::fs::write(dir.path().join("job.WEB-285"), "result_available").unwrap();
    check_now(app).await;
    h.continued(latest, &token).await;
    h.release(latest).await;
    rests_after(app, latest, 6, "completed", "the last wait ends the reply").await;
    assert_eq!(
        sqlx::query_as::<_, (i32, String, Option<i32>)>(
            "SELECT attempt, status, delivered_attempt FROM mcp_waits
             WHERE turn_id = ?1 ORDER BY created_at",
        )
        .bind(latest)
        .fetch_all(&app.db.pool)
        .await
        .unwrap(),
        vec![
            (1, "finished".into(), Some(5)),
            (2, "finished".into(), Some(4)),
            (3, "finished".into(), Some(6)),
        ]
    );
    let packets = h.packets().await;
    for purpose in [
        "Review the WEB-284 PR",
        "Check the WEB-286 deploy",
        "Merge WEB-285",
    ] {
        assert_eq!(
            packets
                .iter()
                .filter(|p| p.contains(&format!("Purpose: {purpose}")))
                .count(),
            1,
            "each result arrives once: {purpose}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn upgrading_continues_a_turn_whose_settled_carried_wait_its_own_wait_stranded() {
    let db = TestDb::empty().await;
    let before = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            else_api::db::MIGRATOR
                .migrations
                .iter()
                .filter(|m| m.version < 65)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    before.run(&db.pool).await.unwrap();
    let (bot, conversation) = (Uuid::new_v4(), Uuid::new_v4());
    let (stranded, waiting) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO bots (id, name) VALUES (?1, 'Old')")
        .bind(bot)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations (id, bot_id, last_turn_no) VALUES (?1, ?2, 2)")
        .bind(conversation)
        .bind(bot)
        .execute(&db.pool)
        .await
        .unwrap();
    for (no, turn) in [(1, waiting), (2, stranded)] {
        sqlx::query(
            "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
                 owner_item_id, provider_id, status, attempt)
             VALUES (?1, ?2, ?3, ?4, zeroblob(32), ?5, ?6, 'synthetic', 'waiting', 1)",
        )
        .bind(turn)
        .bind(bot)
        .bind(conversation)
        .bind(Uuid::new_v4())
        .bind(no)
        .bind(Uuid::new_v4())
        .execute(&db.pool)
        .await
        .unwrap();
    }
    for (turn, attempt, status) in [
        (waiting, 1, "pending"),
        (stranded, 1, "pending"),
        (stranded, 0, "finished"),
    ] {
        sqlx::query(
            "INSERT INTO mcp_waits (id, bot_id, turn_id, attempt, mcp_id, tool, arguments,
                 status_pointer, running_values, purpose, interval_seconds, deadline_at,
                 next_check_at, status)
             VALUES (?1, ?2, ?3, ?4, 't3-code', 'task_status', '{}', '/workState', '[]',
                 'Review the PR.', 15, '2999-01-01T00:00:00.000+00:00',
                 '2999-01-01T00:00:00.000+00:00', ?5)",
        )
        .bind(Uuid::new_v4())
        .bind(bot)
        .bind(turn)
        .bind(attempt)
        .bind(status)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    else_api::db::MIGRATOR.run(&db.pool).await.unwrap();
    let statuses: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, status FROM turns ORDER BY turn_no")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        statuses,
        vec![
            (waiting, "waiting".to_owned()),
            (stranded, "queued".to_owned())
        ],
        "only the turn with an undelivered result continues"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follow_up_that_waits_again_on_the_same_job_keeps_the_one_wait() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert!(!installed.error, "{}", installed.body);
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;

    follow(app, &main, first).await;
    let token = h.continued(first, &token).await;
    let again = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(again.result["status"], "already_waiting", "{}", again.body);
    assert_eq!(again.result["wait_id"], parked.result["wait_id"]);
    assert_eq!(again.result["parked"], false, "{}", again.body);
    h.release(first).await;
    rests_after(
        app,
        first,
        2,
        "waiting",
        "the reply parks on its first wait",
    )
    .await;
    assert_eq!(carried(app, first).await, vec![(1, "pending".into())]);
}
