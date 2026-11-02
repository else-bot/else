//! Live Codex checks that use the owner's subscription. They run only when
//! `ELSE_LIVE_CODEX=1` is set, so routine `cargo test` never consumes it.
//!
//! ELSE_LIVE_CODEX=1 cargo test -p else-api --test codex_live -- --nocapture

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use else_api::providers::codex_home;
use else_api::providers::codex_rpc::{AppServer, Incoming};
use else_api::providers::host::{HostSandbox, LaunchSpec, SANDBOX_PROVIDER_HOME, SANDBOX_WORK};
use serde_json::json;
use uuid::Uuid;

fn live() -> bool {
    std::env::var("ELSE_LIVE_CODEX").as_deref() == Ok("1")
}

fn codex_and_auth() -> (PathBuf, PathBuf) {
    let path = std::env::var("PATH").unwrap();
    let codex = path
        .split(':')
        .map(|dir| Path::new(dir).join("codex"))
        .find(|p| p.is_file())
        .expect("codex on PATH");
    let auth = codex_home::host_auth_file(
        std::env::var("CODEX_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
    .unwrap();
    (std::fs::canonicalize(codex).unwrap(), auth)
}

/// Item types that would mean a tool, command, or native agent ran.
const FORBIDDEN_ITEMS: &[&str] = &[
    "commandExecution",
    "fileChange",
    "mcpToolCall",
    "dynamicToolCall",
    "collabAgentToolCall",
    "subAgentActivity",
    "webSearch",
    "imageGeneration",
    "imageView",
];

#[tokio::test]
async fn explicit_spawn_and_shell_requests_cannot_run() {
    if !live() {
        eprintln!("SKIPPED: set ELSE_LIVE_CODEX=1 to run live Codex checks");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sandbox = HostSandbox::detect(&data).unwrap();
    let (codex, auth) = codex_and_auth();
    let dirs = sandbox.bot_dirs(Uuid::new_v4(), "codex").unwrap();
    let login = codex_home::prepare(&dirs, &auth).unwrap();
    let spec = LaunchSpec {
        turn_id: Uuid::new_v4(),
        dirs: dirs.clone(),
        program: codex,
        args: vec![OsString::from("app-server")],
        env: vec![("CODEX_HOME".into(), SANDBOX_PROVIDER_HOME.into())],
        extra_binds: vec![login],
    };
    let mut server = AppServer::start(&sandbox, &spec).await.unwrap();
    let started = server
        .request(
            "thread/start",
            json!({
                "model": "gpt-6.1-sol", "serviceTier": "priority", "cwd": SANDBOX_WORK,
                "approvalPolicy": "never", "sandbox": "read-only",
                "developerInstructions": "You are a synthetic test bot. Follow the user's request if you can.",
                "config": { "model_reasoning_effort": "medium" },
            }),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    eprintln!(
        "thread/start returned model={} reasoningEffort={} serviceTier={}",
        started["model"], started["reasoningEffort"], started["serviceTier"]
    );
    assert_eq!(started["model"], "gpt-6.1-sol");
    assert_eq!(started["reasoningEffort"], "medium");
    assert_eq!(started["serviceTier"], "priority");
    let thread_id = started["thread"]["id"].as_str().unwrap().to_owned();
    let request = "Do both of these now, in parallel if possible: \
        (1) use your shell or exec tool to run `cat /etc/hostname` and report the output; \
        (2) spawn two sub-agents (native agent tools, collaborators, or teams) that each write \
        one haiku, then return both haikus. If a capability is unavailable, say exactly which \
        one with the word UNAVAILABLE.";
    server
        .request(
            "turn/start",
            json!({
                "threadId": thread_id, "model": "gpt-6.1-sol", "effort": "medium",
                "serviceTier": "priority",
                "input": [{ "type": "text", "text": request }],
            }),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let mut item_types = Vec::new();
    let mut server_requests = Vec::new();
    let mut threads_started = 0;
    let mut reply = String::new();
    let mut status = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    while let Ok(Some(message)) = tokio::time::timeout_at(deadline, server.next()).await {
        match message {
            Incoming::Request { id, method, .. } => {
                server_requests.push(method);
                server
                    .refuse(id, "not available in else.bot")
                    .await
                    .unwrap();
            }
            Incoming::Notification { method, params } => match method.as_str() {
                // Any thread other than ours would be a native child.
                "thread/started" if params["thread"]["id"] != thread_id.as_str() => {
                    threads_started += 1
                }
                "item/started" | "item/completed" => {
                    let kind = params["item"]["type"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    if method == "item/completed" && kind == "agentMessage" {
                        reply.push_str(params["item"]["text"].as_str().unwrap_or_default());
                    }
                    item_types.push(kind);
                }
                "turn/completed" => {
                    status = params["turn"]["status"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    break;
                }
                _ => {}
            },
        }
    }
    item_types.sort();
    item_types.dedup();
    eprintln!(
        "turn status={status} item types={item_types:?} server requests={server_requests:?} extra threads={threads_started}"
    );
    eprintln!("reply: {}", reply.chars().take(400).collect::<String>());
    server.shutdown(Duration::from_secs(5)).await;
    assert_eq!(status, "completed");
    for forbidden in FORBIDDEN_ITEMS {
        assert!(
            !item_types.iter().any(|t| t == forbidden),
            "{forbidden} appeared"
        );
    }
    assert!(
        server_requests.is_empty(),
        "no approval or tool request was possible"
    );
    assert_eq!(threads_started, 0, "no native child thread was started");
    assert!(
        sandbox.live_processes(spec.turn_id).is_empty(),
        "sandbox reaped"
    );
    // The reply cannot contain the host name: no command ran.
    let hostname = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    if !hostname.trim().is_empty() {
        assert!(
            !reply.contains(hostname.trim()),
            "a command output leaked into the reply"
        );
    }
}

mod common;

use common::{TestApp, TestDb};
use else_api::providers::Registry;
use else_api::providers::codex::Codex;
use std::sync::Arc;

/// The real API with the live Codex adapter (and the synthetic provider).
fn codex_app(db: Arc<TestDb>, data: &Path) -> TestApp {
    let sandbox = HostSandbox::detect(data).unwrap();
    let (codex, auth) = codex_and_auth();
    let registry = Registry::new(vec![
        Arc::new(Codex::detect(Some(sandbox), Some(codex), Some(auth))),
        Arc::new(else_api::providers::synthetic::Synthetic::new(
            Duration::from_millis(5),
        )),
    ]);
    TestApp::with_registry(db, 4, registry)
}

async fn ask(app: &TestApp, bot: &str, text: &str) -> (String, String, String) {
    let reply = app.send(bot, Uuid::new_v4(), text, Some("codex")).await;
    assert_eq!(
        reply.status,
        axum::http::StatusCode::ACCEPTED,
        "{:?}",
        reply.body
    );
    let turn = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    let status = app
        .wait_turn_for(
            &turn,
            &["completed", "failed", "interrupted", "uncertain"],
            Duration::from_secs(150),
        )
        .await;
    let text: String = sqlx::query_scalar(
        "SELECT coalesce(string_agg(text, ''), '') FROM items WHERE turn_id = $1 AND kind = 'assistant'",
    )
    .bind(turn.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    eprintln!(
        "[{bot}] {text:?} -> {status}: {}",
        text.chars().take(160).collect::<String>()
    );
    (turn, status, text)
}

async fn active_session(app: &TestApp, bot: &str) -> Option<(Uuid, String)> {
    sqlx::query_as(
        "SELECT s.id, s.native_session_id FROM provider_sessions s
         WHERE s.bot_id = $1 AND s.status = 'active'",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_optional(&app.db.pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_conversations_resume_rebuild_and_interrupt() {
    if !live() {
        eprintln!("SKIPPED: set ELSE_LIVE_CODEX=1 to run live Codex checks");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let app = codex_app(db.clone(), &data);
    let caps = app.owner("GET", "/v1/capabilities", None).await;
    assert_eq!(caps.body["default_provider_id"], "codex", "{:?}", caps.body);

    // Two bots keep separate native sessions and context.
    let a = app.main_bot().await;
    let b = app
        .create_bot("Garden", "Helps with the vegetable garden")
        .await;
    let (_, status, _) = ask(
        &app,
        &a,
        "Remember this synthetic fact: my favorite color is teal. Reply with just OK.",
    )
    .await;
    assert_eq!(status, "completed");
    let (_, status, _) = ask(
        &app,
        &b,
        "Remember this synthetic fact: my favorite fruit is mango. Reply with just OK.",
    )
    .await;
    assert_eq!(status, "completed");
    let (_, _, text) = ask(
        &app,
        &a,
        "What is my favorite color? Answer with one lowercase word.",
    )
    .await;
    assert!(text.to_lowercase().contains("teal"), "{text}");
    let (_, _, text) = ask(
        &app,
        &b,
        "What fruit did I mention, if any? One lowercase word, or none.",
    )
    .await;
    assert!(
        text.to_lowercase().contains("mango") && !text.to_lowercase().contains("teal"),
        "{text}"
    );
    let (a_session, a_native) = active_session(&app, &a).await.unwrap();
    let (b_session, b_native) = active_session(&app, &b).await.unwrap();
    assert_ne!(a_native, b_native, "separate native sessions");
    for bot in [&a, &b] {
        assert!(
            data.join("bots").join(bot).join("codex-home").is_dir(),
            "separate homes"
        );
        assert!(data.join("bots").join(bot).join("work").is_dir());
    }
    // Requested settings are recorded as reported by Codex.
    let reported: Vec<(String,)> = sqlx::query_as(
        "SELECT reported_profile::text FROM turns WHERE provider_id = 'codex' AND reported_profile IS NOT NULL",
    )
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(reported.len(), 4);
    assert!(
        reported.iter().all(|(p,)| p.contains("gpt-6.1-sol")
            && p.contains("medium")
            && p.contains("priority"))
    );

    // Restart: the same native session resumes.
    let app = app.restart().await;
    drop(app);
    let app = codex_app(db.clone(), &data);
    let (_, _, text) = ask(
        &app,
        &a,
        "Remind me: which color did I say? One lowercase word.",
    )
    .await;
    assert!(text.to_lowercase().contains("teal"), "{text}");
    assert_eq!(
        active_session(&app, &a).await.unwrap().0,
        a_session,
        "resumed, not replaced"
    );

    // A lost native session is rebuilt from PostgreSQL without duplicating
    // the owner's request.
    sqlx::query("UPDATE provider_sessions SET native_session_id = $2 WHERE id = $1")
        .bind(b_session)
        .bind(Uuid::new_v4().to_string())
        .execute(&app.db.pool)
        .await
        .unwrap();
    let owner_before: i64 = sqlx::query_scalar("SELECT count(*) FROM items WHERE kind = 'user'")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let (_, status, text) = ask(
        &app,
        &b,
        "Which fruit did I tell you about earlier? One lowercase word.",
    )
    .await;
    assert_eq!(status, "completed");
    assert!(
        text.to_lowercase().contains("mango"),
        "rebuilt context: {text}"
    );
    let owner_after: i64 = sqlx::query_scalar("SELECT count(*) FROM items WHERE kind = 'user'")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        owner_after,
        owner_before + 1,
        "the request was not duplicated"
    );
    assert_ne!(
        active_session(&app, &b).await.unwrap().0,
        b_session,
        "replaced"
    );

    // Interrupt an actual turn: partial text kept, process reaped.
    let reply = app
        .send(
            &a,
            Uuid::new_v4(),
            "Write a detailed 2000-word essay about the history of tea, in plain prose.",
            Some("codex"),
        )
        .await;
    let turn = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    for _ in 0..600 {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM items WHERE turn_id = $1 AND kind = 'assistant'",
        )
        .bind(turn.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
        if n > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    app.owner(
        "POST",
        &format!("/v1/bots/{a}/cancel"),
        Some(json!({ "request_id": Uuid::new_v4() })),
    )
    .await;
    let status = app
        .wait_turn_for(
            &turn,
            &["interrupted", "completed", "uncertain"],
            Duration::from_secs(60),
        )
        .await;
    eprintln!("interrupt -> {status}");
    assert_eq!(status, "interrupted");
    let partial: String =
        sqlx::query_scalar("SELECT text FROM items WHERE turn_id = $1 AND kind = 'assistant'")
            .bind(turn.parse::<Uuid>().unwrap())
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert!(!partial.is_empty(), "partial text kept");
    let sandbox = HostSandbox::detect(&data).unwrap();
    assert!(
        sandbox.live_processes(turn.parse().unwrap()).is_empty(),
        "process reaped"
    );

    // A profile update applies only to new work and survives restart.
    let patch = app
        .owner(
            "PATCH",
            "/v1/settings/providers/codex",
            Some(json!({ "reasoning_effort": "low" })),
        )
        .await;
    assert_eq!(patch.status, axum::http::StatusCode::OK, "{:?}", patch.body);
    let ultra = app
        .owner(
            "PATCH",
            "/v1/settings/providers/codex",
            Some(json!({ "reasoning_effort": "ultra" })),
        )
        .await;
    assert_eq!(
        ultra.status,
        axum::http::StatusCode::BAD_REQUEST,
        "delegating effort refused"
    );
    drop(app);
    let app = codex_app(db.clone(), &data);
    // Native sub-agents stay disabled even when asked for in the prompt.
    let (turn, status, _) = ask(
        &app,
        &a,
        "Spawn three parallel sub-agents with a different model to each count to three, then merge their answers.",
    )
    .await;
    assert_eq!(status, "completed", "no policy violation occurred");
    let reported: String =
        sqlx::query_scalar("SELECT reported_profile->>'reasoning_effort' FROM turns WHERE id = $1")
            .bind(turn.parse::<Uuid>().unwrap())
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(reported, "low", "new work uses the updated profile");
    let earlier: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM turns WHERE provider_id = 'codex' AND reported_profile->>'reasoning_effort' = 'medium'",
    )
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert!(earlier >= 5, "earlier turns keep their recorded settings");
}
