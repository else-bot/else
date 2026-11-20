use crate::common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::TestApp;
use common::recorder::{Harness, harness, tool};
use else_api::providers::{grok::Grok, host::Host};
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
out = lambda e: print(json.dumps(e), flush=True)
def once(name):
    path = os.path.join(here, name)
    if os.path.exists(path):
        return False
    open(path, "w").close()
    return True
if "[signout]" in prompt and once("signed-out"):
    open(os.path.join(os.environ["GROK_HOME"], "auth.json"), "w").write("{}")
    out({"type": "system", "subtype": "init", "session_id": "", "model": "unknown", "tools": [], "mcp_servers": []})
    out({"type": "result", "subtype": "error_during_execution", "is_error": True, "stop_reason": None,
         "errors": ["Not signed in. To authenticate without a browser, run:\n  grok login --device-code"]})
    sys.exit(1)
if "[revoked]" in prompt and once("revoked"):
    open(os.path.join(os.environ["GROK_HOME"], "auth.json"), "w").write("{}")
    out({"type": "system", "subtype": "init", "session_id": session, "model": arg("--model"),
         "tools": tools, "mcp_servers": [{"name": "elsebot", "status": "connected"}]})
    out({"type": "result", "subtype": "error_during_execution", "is_error": True, "stop_reason": None,
         "errors": ["Internal error: {\n  \"message\": \"Unauthorized (401) from https://cli-chat-proxy.grok.com/v1/responses: Invalid or expired credentials\",\n  \"http_status\": 401\n}"]})
    sys.exit(1)
with open(os.path.join(here, "launches.jsonl"), "a") as f:
    f.write(json.dumps({"session": session, "resumed": resumed, "prompt": prompt}) + "\n")
out({"type": "system", "subtype": "init", "session_id": session, "model": arg("--model"),
     "tools": tools, "mcp_servers": [{"name": "elsebot", "status": "connected"}]})
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
    with open(os.path.join(here, "grant.tmp"), "w") as f:
        f.write(os.environ["ELSE_TASK_GRANT"])
    os.rename(os.path.join(here, "grant.tmp"), os.path.join(here, "grant"))
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

const SIGNED_IN: &str = r#"{"https://auth.x.ai::id": {"user_id": "u", "team_id": "t"}}"#;

struct Fixture {
    _dir: tempfile::TempDir,
    fake: PathBuf,
    login: PathBuf,
    host: Host,
    h: Harness,
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
    std::fs::write(login.join("auth.json"), SIGNED_IN).unwrap();
    let host = Host::new(&root.join("data"), &root.join("projects"));
    let grok = Grok::detect(Some(host.clone()), Some(program), Some(login.clone()))
        .await
        .with_tools("http://127.0.0.1:9/tools/v1/mcp".into());
    let h = harness(
        TestApp::builder()
            .max_active(2)
            .providers(vec![Arc::new(grok)]),
    )
    .await;
    let saved = h.app
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
        login,
        host,
        h,
    }
}

async fn send_grok(app: &TestApp, bot: &str, text: &str) -> Uuid {
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
    let bot = fx.h.app.main_bot().await;
    let turn = send_grok(&fx.h.app, &bot, "[stubborn] keep working").await;
    fx.h.app.wait_turn(&turn.to_string(), &["running"]).await;
    let pid_file = fx.fake.join("worker.pid");
    until("the detached worker", Duration::from_secs(10), || {
        pid_file.is_file()
    })
    .await;
    let worker: i32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    assert!(alive(worker));
    let stop =
        fx.h.app
            .owner(
                "POST",
                &format!("/v1/bots/{bot}/cancel"),
                Some(json!({ "request_id": Uuid::new_v4() })),
            )
            .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    let status =
        fx.h.app
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
    let school = fx.h.app.create_bot("School").await;
    let warm = fx.h.send(&school, "hello").await;
    fx.h.app.wait_turn(&warm.to_string(), &["completed"]).await;
    let main = fx.h.app.main_bot().await;
    let root = send_grok(&fx.h.app, &main, "[park] ask School for the form").await;
    fx.h.app.wait_turn(&root.to_string(), &["running"]).await;
    let grant = fx.fake.join("grant");
    until("Grok's task grant", common::SETTLE, || grant.is_file()).await;
    let token = std::fs::read_to_string(grant).unwrap();
    let reply = tool(
        &fx.h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": school, "task": "Collect the form." }),
    )
    .await;
    let pending = &reply.result;
    assert_eq!(pending["status"], "pending", "{}", reply.body);
    let task: Uuid = pending["task_id"].as_str().unwrap().parse().unwrap();

    let status =
        fx.h.app
            .wait_turn_for(
                &root.to_string(),
                &["completed", "failed", "uncertain"],
                Duration::from_secs(120),
            )
            .await;
    assert_eq!(status, "completed");
    let delegation: String = sqlx::query_scalar("SELECT status FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&fx.h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(delegation, "completed", "the forced stop kept the child");
    let attempts: i32 = sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(root)
        .fetch_one(&fx.h.app.db.pool)
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

async fn grok_capability(app: &TestApp) -> Value {
    let caps = app.owner("GET", "/v1/capabilities", None).await;
    caps.body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "grok")
        .unwrap()
        .clone()
}

/// Grok's two ways of failing for want of a login, as Grok 1.0.89 prints
/// them: an expired login stops before opening a session, and a revoked one
/// opens a session and gets a 401. Both clear the host `auth.json`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_grok_that_signs_out_mid_turn_is_unavailable_until_the_host_signs_in_again() {
    let signed_out = "Grok isn't signed in on this server. Run `grok login --device-code` there, \
                      or switch this bot to another model.";
    let fx = fixture().await;
    let bot = fx.h.app.main_bot().await;
    assert_eq!(grok_capability(&fx.h.app).await["available"], true);

    for marker in ["[signout]", "[revoked]"] {
        let turn = send_grok(&fx.h.app, &bot, &format!("{marker} hello")).await;
        let status =
            fx.h.app
                .wait_turn(&turn.to_string(), &["failed", "completed", "uncertain"])
                .await;
        let (code, message): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT error_code, error_message FROM turns WHERE id = ?1")
                .bind(turn)
                .fetch_one(&fx.h.app.db.pool)
                .await
                .unwrap();
        assert_eq!(status, "failed", "{marker}: {message:?}");
        assert_eq!(code.as_deref(), Some("provider_unavailable"), "{marker}");
        assert_eq!(
            message.as_deref(),
            Some(signed_out),
            "{marker}: not the CLI's raw login text"
        );

        let grok = grok_capability(&fx.h.app).await;
        assert_eq!(grok["available"], false);
        assert_eq!(grok["unavailable_reason"], signed_out);
        let rejected =
            fx.h.app
                .send(&bot, Uuid::new_v4(), "again", Some("grok"))
                .await;
        assert_eq!(rejected.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(rejected.body["error"]["code"], "provider_unavailable");
        assert_eq!(rejected.body["error"]["message"], signed_out);

        std::fs::write(fx.login.join("auth.json"), SIGNED_IN).unwrap();
        assert_eq!(
            grok_capability(&fx.h.app).await["available"],
            true,
            "a host sign-in is picked up without a restart"
        );
    }
    let turn = send_grok(&fx.h.app, &bot, "hello again").await;
    let status =
        fx.h.app
            .wait_turn(&turn.to_string(), &["failed", "completed", "uncertain"])
            .await;
    let error: Option<String> = sqlx::query_scalar("SELECT error_message FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&fx.h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "completed", "{error:?}");
}
