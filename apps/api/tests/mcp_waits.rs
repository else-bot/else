mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::{TestApp, TestDb};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::synthetic::Synthetic;
use serde_json::{Value, json};
use uuid::Uuid;

const SLOW: &str = "[synthetic:slow] Start WEB-284 in T3 and review its PR when it is ready.";

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
        work = open(state).read().strip()
        r = {'content':[{'type':'text','text':json.dumps({'taskId': m['params']['arguments']['taskId'], 'workState': work, 'summary': 'Opened PR #775' if work == 'result_available' else None})}]}
    else: r = {}
    print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':r}), flush=True)
"#;

async fn harness() -> (TestApp, Arc<Synthetic>) {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let synthetic = Arc::new(Synthetic::new(Duration::from_millis(5)));
    let registry = else_api::providers::Registry::new(vec![synthetic.clone()]);
    (TestApp::with_registry(db, 4, registry), synthetic)
}

async fn running(app: &TestApp, bot: &str) -> (Uuid, String) {
    let sent = app.send(bot, Uuid::new_v4(), SLOW, Some("synthetic")).await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["running"]).await;
    let turn: Uuid = turn.parse().unwrap();
    let (bot, conversation): (Uuid, Uuid) =
        sqlx::query_as("SELECT bot_id, conversation_id FROM turns WHERE id = ?1")
            .bind(turn)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    let token = app
        .scheduler
        .grants()
        .issue(
            bot,
            conversation,
            turn,
            vec![Tool::Mcp, Tool::ManageMcp],
            DEFAULT_TTL,
        )
        .await;
    (turn, token)
}

async fn tool(app: &TestApp, token: &str, name: &str, arguments: Value) -> Value {
    let reply = app.call("POST", "/tools/v1/mcp", Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})), Some(token)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.body["result"].clone()
}

fn text(result: &Value) -> Value {
    assert_ne!(result["isError"], true, "{result}");
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

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

async fn attempts(app: &TestApp, turn: Uuid) -> i32 {
    sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn settle(app: &TestApp, turn: Uuid, status: &str) {
    app.wait_turn_for(&turn.to_string(), &[status], Duration::from_secs(30))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_waiting_on_a_running_mcp_job_continues_by_itself_when_it_finishes() {
    let (app, synthetic) = harness().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    text(&tool(&app, &token, "mcp_install", install(&state)).await);

    let parked = tool(&app, &token, "mcp_wait", wait_args()).await;
    assert!(else_api::app::tools::parks("mcp_wait", &parked), "{parked}");
    assert_eq!(text(&parked)["status"], "waiting");
    settle(&app, turn, "waiting").await;
    assert_eq!(attempts(&app, turn).await, 1);

    check_now(&app).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        wait_row(&app, turn).await.0,
        "pending",
        "the job still runs"
    );
    assert_eq!(
        app.wait_turn(&turn.to_string(), &["waiting"]).await,
        "waiting"
    );

    std::fs::write(&state, "result_available").unwrap();
    check_now(&app).await;
    settle(&app, turn, "completed").await;
    assert_eq!(attempts(&app, turn).await, 2, "the same request continued");
    let (status, delivered, checks) = wait_row(&app, turn).await;
    assert_eq!((status.as_str(), delivered), ("finished", Some(2)));
    assert!(checks >= 3, "{checks}");
    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(packet.contains("[else.bot MCP waits]"), "{packet}");
    assert!(packet.contains("Opened PR #775"), "{packet}");
    assert!(packet.contains("status=\"finished\""), "{packet}");

    let (_, token) = running(&app, &main).await;
    let done = text(&tool(&app, &token, "mcp_wait", wait_args()).await);
    assert_eq!(done["status"], "finished", "{done}");
    assert_eq!(done["result"]["content"][0]["type"], "text");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wait_survives_a_restart_and_a_follow_up_withdraws_it() {
    let (app, _) = harness().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    text(&tool(&app, &token, "mcp_install", install(&state)).await);
    text(&tool(&app, &token, "mcp_wait", wait_args()).await);
    settle(&app, turn, "waiting").await;

    let app = app.restart().await;
    assert_eq!(
        app.wait_turn(&turn.to_string(), &["waiting"]).await,
        "waiting"
    );
    std::fs::write(&state, "result_available").unwrap();
    check_now(&app).await;
    settle(&app, turn, "completed").await;
    assert_eq!(wait_row(&app, turn).await.0, "finished");

    std::fs::write(&state, "working").unwrap();
    let (turn, token) = running(&app, &main).await;
    text(&tool(&app, &token, "mcp_wait", wait_args()).await);
    settle(&app, turn, "waiting").await;
    let follow_up = app
        .send(
            &main,
            Uuid::new_v4(),
            "Never mind, skip the review.",
            Some("synthetic"),
        )
        .await;
    assert_eq!(follow_up.status, StatusCode::ACCEPTED, "{}", follow_up.body);
    settle(&app, turn, "interrupted").await;
    assert_eq!(wait_row(&app, turn).await.0, "cancelled");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_status_tool_not_declared_repeatable_is_refused_and_a_deadline_ends_a_wait() {
    let (app, _) = harness().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    text(&tool(&app, &token, "mcp_install", install(&state)).await);
    let mut poke = wait_args();
    poke["tool"] = json!("task_poke");
    let refused = tool(&app, &token, "mcp_wait", poke).await;
    assert_eq!(refused["isError"], true, "{refused}");
    assert!(refused.to_string().contains("NOT_REPEATABLE"), "{refused}");
    assert!(!else_api::app::tools::parks("mcp_wait", &refused));
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM turn_holds WHERE turn_id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(held, 0, "a refused tool neither waits nor asks the owner");
    settle(&app, turn, "completed").await;

    let (next, token) = running(&app, &main).await;
    text(&tool(&app, &token, "mcp_wait", wait_args()).await);
    settle(&app, next, "waiting").await;
    sqlx::query("UPDATE mcp_waits SET deadline_at = '2000-01-01T00:00:00.000+00:00', next_check_at = '2999-01-01T00:00:00.000+00:00' WHERE turn_id = ?1")
        .bind(next)
        .execute(&app.db.pool)
        .await
        .unwrap();
    settle(&app, next, "completed").await;
    let (status, delivered, checks) = wait_row(&app, next).await;
    assert_eq!(
        (status.as_str(), delivered, checks),
        ("expired", Some(2), 1),
        "expired without another call"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_package_with_many_due_waits_does_not_hide_another_package() {
    let (app, _) = harness().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    text(&tool(&app, &token, "mcp_install", install(&state)).await);
    text(&tool(&app, &token, "mcp_wait", wait_args()).await);
    settle(&app, turn, "waiting").await;
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
    let (app, _) = harness().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    text(&tool(&app, &token, "mcp_install", install(&state)).await);
    let mut external = wait_args();
    external["effect"] = json!("external");
    let asked = text(&tool(&app, &token, "mcp_wait", external).await);
    assert_eq!(asked["parked"], true, "{asked}");
    assert_eq!(calls(&state), 0, "nothing runs before the owner approves");
    let waits: i64 = sqlx::query_scalar("SELECT count(*) FROM mcp_waits")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(waits, 0);
    settle(&app, turn, "waiting").await;

    let (next, token) = running(&app, &main).await;
    text(&tool(&app, &token, "mcp_wait", wait_args()).await);
    settle(&app, next, "waiting").await;
    assert_eq!(calls(&state), 1);
    std::fs::write(state.with_extension("unsafe"), "").unwrap();
    check_now(&app).await;
    settle(&app, next, "completed").await;
    assert_eq!(wait_row(&app, next).await.0, "failed");
    assert_eq!(
        calls(&state),
        1,
        "a tool that no longer qualifies is not called again"
    );
}
