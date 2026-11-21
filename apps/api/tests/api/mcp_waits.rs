use crate::common;

use std::path::Path;
use std::time::Duration;

use axum::http::StatusCode;
use common::recorder::{GATE, GATE_EVERY_ATTEMPT, harness, tool};
use common::{TestApp, TestDb};
use serde_json::{Value, json};
use uuid::Uuid;

mod checks;
mod delegated;
mod replies;
mod stops;

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

async fn carried(app: &TestApp, turn: Uuid) -> Vec<(i32, String)> {
    sqlx::query_as("SELECT attempt, status FROM mcp_waits WHERE turn_id = ?1 ORDER BY created_at")
        .bind(turn)
        .fetch_all(&app.db.pool)
        .await
        .unwrap()
}

fn calls(state: &Path) -> usize {
    std::fs::read_to_string(state.with_extension("calls"))
        .unwrap_or_default()
        .lines()
        .count()
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
