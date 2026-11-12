use crate::common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{TestApp, TestDb};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::{Registry, grok::Grok, host::Host, synthetic::Synthetic};
use serde_json::{Value, json};
use uuid::Uuid;

const FAKE_GROK: &str = r#"#!/usr/bin/env python3
import json, os, signal, sys, time
here = os.path.dirname(os.path.realpath(__file__))
a = sys.argv[1:]
if a[:1] == ["--version"]:
    print("grok 9.9.9")
    sys.exit()
arg = lambda flag: a[a.index(flag) + 1]
resumed = "--resume" in a
session = arg("--resume") if resumed else arg("--session-id")
names = {"run_terminal_cmd": "run_terminal_command", "get_task_output": "get_command_or_subagent_output",
         "kill_task": "kill_command_or_subagent"}
tools = [names.get(t, t) for t in arg("--tools").split(",")]
prompt = open(arg("--prompt-file")).read()
with open(os.path.join(here, "launches.jsonl"), "a") as f:
    f.write(json.dumps({"session": session, "resumed": resumed, "prompt": prompt}) + "\n")
out = lambda e: print(json.dumps(e), flush=True)
out({"type": "system", "subtype": "init", "session_id": session, "model": arg("--model"),
     "tools": tools, "mcp_servers": [{"name": "elsebot", "status": "connected"}]})
def once(name):
    path = os.path.join(here, name)
    if os.path.exists(path):
        return False
    open(path, "w").close()
    return True
if "[stubborn]" in prompt:
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    if os.fork() == 0:
        os.setsid()
        open(os.path.join(here, "worker.pid"), "w").write(str(os.getpid()))
        while True:
            time.sleep(1)
    out({"type": "stream_event", "event": {"type": "message_start"}})
    while True:
        time.sleep(1)
if "[park]" in prompt and once("parked"):
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    out({"type": "stream_event", "event": {"type": "message_start"}})
    out({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "c1", "name": "use_tool",
         "input": {"tool_name": "elsebot__delegate", "tool_input": {}}}]}})
    out({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "c1",
         "content": "{\"status\":\"pending\",\"parked\":true}"}]}})
    while True:
        time.sleep(1)
out({"type": "stream_event", "event": {"type": "message_start"}})
reply = "Continued." if "Continue the owner's request" in prompt else "Done."
out({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": reply}}})
out({"type": "stream_event", "event": {"type": "message_stop"}})
out({"type": "result", "subtype": "success", "is_error": False, "stop_reason": "end_turn"})
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    fake: PathBuf,
    host: Host,
    app: TestApp,
}

async fn fixture() -> Fixture {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let fake = root.join("fake");
    std::fs::create_dir(&fake).unwrap();
    let program = fake.join("grok");
    std::fs::write(&program, FAKE_GROK).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let login = root.join("login");
    std::fs::create_dir(&login).unwrap();
    std::fs::write(login.join("auth.json"), "{}").unwrap();
    let host = Host::new(&root.join("data"), &root.join("projects"));
    let grok = Grok::detect(Some(host.clone()), Some(program), Some(login))
        .with_tools("http://127.0.0.1:9/tools/v1/mcp".into());
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let app = TestApp::with_registry(
        db,
        2,
        Registry::new(vec![
            Arc::new(grok),
            Arc::new(Synthetic::new(Duration::from_millis(5))),
        ]),
    );
    let saved = app
        .owner(
            "PATCH",
            "/v1/settings/providers/grok",
            Some(json!({ "model": "grok-4.7", "reasoning_effort": "low", "service_tier": "default" })),
        )
        .await;
    assert!(saved.status.is_success(), "{}", saved.body);
    Fixture {
        _dir: dir,
        fake,
        host,
        app,
    }
}

async fn send(app: &TestApp, bot: &str, text: &str) -> Uuid {
    let sent = app.send(bot, Uuid::new_v4(), text, Some("grok")).await;
    assert!(sent.status.is_success(), "{}", sent.body);
    sent.body["turn"]["id"].as_str().unwrap().parse().unwrap()
}

async fn until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(timeout, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn launches(fake: &Path) -> Vec<Value> {
    std::fs::read_to_string(fake.join("launches.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread")]
async fn an_escalated_stop_reaps_a_stubborn_cli_and_its_detached_worker_before_the_turn_settles() {
    let fx = fixture().await;
    let bot = fx.app.main_bot().await;
    let turn = send(&fx.app, &bot, "[stubborn] keep working").await;
    fx.app.wait_turn(&turn.to_string(), &["running"]).await;
    let pid_file = fx.fake.join("worker.pid");
    until("the detached worker", Duration::from_secs(10), || {
        pid_file.is_file()
    })
    .await;
    let worker: i32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    assert!(alive(worker));
    let stop = fx
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    let status = fx
        .app
        .wait_turn_for(
            &turn.to_string(),
            &["uncertain", "interrupted", "failed", "completed"],
            Duration::from_secs(10),
        )
        .await;
    assert_eq!(status, "uncertain", "the CLI never confirmed its stop");
    assert!(
        !alive(worker),
        "a detached worker outlived the attempt that settled its turn"
    );
    assert!(fx.host.live_processes(turn).is_empty());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_parked_parent_without_a_result_keeps_its_child_and_continues_in_a_new_session() {
    let fx = fixture().await;
    let school = fx.app.create_bot("School").await;
    let warm = fx
        .app
        .send(&school, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    fx.app
        .wait_turn(warm.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let main = fx.app.main_bot().await;
    let root = send(&fx.app, &main, "[park] ask School for the form").await;
    fx.app.wait_turn(&root.to_string(), &["running"]).await;
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(root)
        .fetch_one(&fx.app.db.pool)
        .await
        .unwrap();
    let token = fx
        .app
        .scheduler
        .grants()
        .issue(
            main.parse().unwrap(),
            conversation,
            root,
            vec![Tool::ReadHistory, Tool::Delegate],
            DEFAULT_TTL,
        )
        .await;
    let reply = fx
        .app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": "delegate",
                                     "arguments": { "target_bot_id": school, "task": "Collect the form." } } })),
            Some(&token),
        )
        .await;
    let text = reply.body["result"]["content"][0]["text"].as_str().unwrap();
    let pending: Value = serde_json::from_str(text).unwrap();
    assert_eq!(pending["status"], "pending", "{}", reply.body);
    let task: Uuid = pending["task_id"].as_str().unwrap().parse().unwrap();

    let status = fx
        .app
        .wait_turn_for(
            &root.to_string(),
            &["completed", "failed", "uncertain"],
            Duration::from_secs(120),
        )
        .await;
    assert_eq!(status, "completed");
    let delegation: String = sqlx::query_scalar("SELECT status FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&fx.app.db.pool)
        .await
        .unwrap();
    assert_eq!(delegation, "completed", "the forced stop kept the child");
    let attempts: i32 = sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(root)
        .fetch_one(&fx.app.db.pool)
        .await
        .unwrap();
    assert_eq!(attempts, 2);
    let parent: Vec<Value> = launches(&fx.fake)
        .into_iter()
        .filter(|l| l["prompt"].as_str().unwrap().contains("[park]"))
        .collect();
    assert_eq!(parent.len(), 2, "{parent:?}");
    assert_eq!(
        parent[1]["resumed"], false,
        "the unfinished session is not resumed"
    );
    assert_ne!(parent[0]["session"], parent[1]["session"]);
    assert!(fx.host.live_processes(root).is_empty());
}
