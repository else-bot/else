//! POC-026: task secrets and script execution against SQLite, the real
//! router and MCP endpoint, and a fake runtime that records what reaches
//! the guest (the live checks are in `scripts_live.rs`).

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use common::TestApp;
use else_api::computers::{
    ComputerConfig, ComputerManager, ComputerRuntime, ExecOutput, RuntimeError, RuntimeState, Spec,
};
use else_api::grants::{DEFAULT_TTL, Tool};
use serde_json::{Value, json};
use uuid::Uuid;

const PASSWORD: &str = "s3cr3t-Pa55word";

/// Records every guest command (program, args, stdin). A foreground command
/// containing `HANG` blocks until a kill for it arrives.
#[derive(Default)]
struct Guest {
    state: Mutex<std::collections::HashMap<String, RuntimeState>>,
    execs: Mutex<Vec<(String, Vec<String>, Vec<u8>)>>,
    /// Per computer: a kill for it arrived.
    killed: Mutex<std::collections::HashMap<String, Arc<tokio::sync::Notify>>>,
    started: tokio::sync::Notify,
    /// Computers whose kill never answers.
    stalled: Mutex<Vec<String>>,
}

impl Guest {
    fn killed(&self, name: &str) -> Arc<tokio::sync::Notify> {
        self.killed
            .lock()
            .unwrap()
            .entry(name.to_owned())
            .or_default()
            .clone()
    }

    fn execs(&self) -> Vec<(String, Vec<String>, Vec<u8>)> {
        self.execs.lock().unwrap().clone()
    }

    /// A RUN reply: the command's stdout echoes the secret variable the way
    /// a careless script would.
    fn run_reply(&self, args: &[&str], input: &[u8]) -> String {
        use base64::Engine;
        let mut out = String::from("no secret");
        for line in String::from_utf8_lossy(input).lines() {
            if let Some((name, value)) = line.split_once(' ') {
                let value = base64::engine::general_purpose::STANDARD
                    .decode(value)
                    .unwrap();
                out = format!("{name}={}", String::from_utf8(value).unwrap());
            }
        }
        let _ = args;
        format!("0\n{}\n0\n{out}\n\0\n", out.len())
    }
}

#[async_trait]
impl ComputerRuntime for Guest {
    async fn list(&self) -> Result<Vec<(String, RuntimeState)>, RuntimeError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .iter()
            .map(|(n, s)| (n.clone(), *s))
            .collect())
    }
    async fn create(&self, spec: &Spec) -> Result<(), RuntimeError> {
        self.state
            .lock()
            .unwrap()
            .insert(spec.name.clone(), RuntimeState::Running);
        Ok(())
    }
    async fn start(&self, name: &str) -> Result<(), RuntimeError> {
        self.state
            .lock()
            .unwrap()
            .insert(name.into(), RuntimeState::Running);
        Ok(())
    }
    async fn stop(&self, name: &str) -> Result<(), RuntimeError> {
        self.state
            .lock()
            .unwrap()
            .insert(name.into(), RuntimeState::Stopped);
        Ok(())
    }
    async fn pause(&self, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn resume(&self, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn copy_out(&self, _: &str, _: &str, _: &std::path::Path) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn exec(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        self.exec_input(name, program, args, &[], timeout).await
    }
    async fn exec_input(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        input: &[u8],
        _: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        self.execs.lock().unwrap().push((
            program.into(),
            args.iter().map(|a| (*a).to_owned()).collect(),
            input.to_vec(),
        ));
        // `runuser -u elsebot -- /bin/sh -c SCRIPT sh ARGS...`
        let at = args.iter().position(|a| *a == "-c").map_or(0, |i| i + 1);
        let script = args.get(at).copied().unwrap_or_default();
        let stdout = if script.contains("wait $pg; c=$?") {
            // A foreground command (RUN): its user command follows `sh`.
            if args.get(at + 2).is_some_and(|c| c.contains("HANG")) {
                self.started.notify_one();
                // Bounded, so a broken kill fails the test instead of hanging.
                let killed = self.killed(name);
                let _ = tokio::time::timeout(Duration::from_secs(20), killed.notified()).await;
                "143\n0\n0\n\n\0\n".to_owned()
            } else {
                self.run_reply(args, input)
            }
        } else if script.contains("alive()") {
            // The kill script.
            if self.stalled.lock().unwrap().iter().any(|n| n == name) {
                tokio::time::sleep(Duration::from_secs(12)).await;
            }
            self.killed(name).notify_one();
            "stopped".into()
        } else if script.contains("echo launched") {
            "launched".into()
        } else {
            String::new()
        };
        Ok(ExecOutput {
            status: Some(0),
            stdout,
            stderr: String::new(),
            truncated: false,
        })
    }
}

struct Setup {
    app: TestApp,
    guest: Arc<Guest>,
    manager: Arc<ComputerManager>,
}

async fn setup() -> Setup {
    let db = Arc::new(common::TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let guest = Arc::new(Guest::default());
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        guest.clone(),
        ComputerConfig {
            desktop: true,
            idle_after: Duration::from_secs(3600),
            ..ComputerConfig::default()
        },
    ));
    manager.reconcile_startup().await.unwrap();
    let registry = else_api::providers::Registry::new(vec![Arc::new(
        else_api::providers::synthetic::Synthetic::new(Duration::from_millis(5)),
    )]);
    let app = TestApp::with_parts(db, 4, registry, Some(manager.clone()));
    Setup {
        app,
        guest,
        manager,
    }
}

/// A running turn of `bot` and a computer grant for it.
async fn grant(app: &TestApp, bot: &str) -> (Uuid, String) {
    let reply = app
        .send(bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn: Uuid = reply.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&turn.to_string(), &["running"]).await;
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let token = app
        .scheduler
        .grants()
        .issue(
            bot.parse().unwrap(),
            conversation,
            turn,
            vec![Tool::Computer],
            DEFAULT_TTL,
        )
        .await;
    (turn, token)
}

async fn tool(app: &TestApp, token: &str, name: &str, arguments: Value) -> Value {
    let reply = app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": name, "arguments": arguments } })),
            Some(token),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    let result = reply.body["result"].clone();
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    if result["isError"] == true {
        json!({ "error": text })
    } else {
        serde_json::from_str(text).unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn secrets_are_write_only_encrypted_and_scoped_to_their_bot() {
    let Setup { app, .. } = setup().await;
    let bot = app.create_bot("Mailer").await;
    let other = app.create_bot("Other").await;
    let path = format!("/v1/bots/{bot}/task-secrets/smtp");
    let body = json!({ "env": { "SMTP_USER": "bot", "SMTP_PASSWORD": PASSWORD } });
    let created = app.owner("PUT", &path, Some(body.clone())).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    assert_eq!(
        app.owner("PUT", &path, Some(body)).await.status,
        StatusCode::OK
    );
    let listed = app
        .owner("GET", &format!("/v1/bots/{bot}/task-secrets"), None)
        .await;
    assert_eq!(listed.body["secrets"][0]["name"], "smtp");
    assert_eq!(
        listed.body["secrets"][0]["env"],
        json!(["SMTP_PASSWORD", "SMTP_USER"])
    );
    assert!(!listed.body.to_string().contains(PASSWORD));
    // At rest: ciphertext only.
    let stored: Vec<u8> = sqlx::query_scalar("SELECT ciphertext FROM task_secrets")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&stored).contains(PASSWORD));
    assert!(stored.starts_with(b"age-encryption.org/v1"));
    // Another bot sees none; invalid variables are refused.
    let none = app
        .owner("GET", &format!("/v1/bots/{other}/task-secrets"), None)
        .await;
    assert_eq!(none.body["secrets"], json!([]));
    for env in [
        json!({ "PATH": "/tmp" }),
        json!({ "LD_PRELOAD": "x" }),
        json!({}),
    ] {
        let refused = app
            .owner(
                "PUT",
                &format!("/v1/bots/{bot}/task-secrets/bad"),
                Some(json!({ "env": env })),
            )
            .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{env}");
    }
    // Task grants never reach owner routes.
    let (_, token) = grant(&app, &bot).await;
    let as_task = app
        .call(
            "GET",
            &format!("/v1/bots/{bot}/task-secrets"),
            None,
            Some(&token),
        )
        .await;
    assert_eq!(as_task.status, StatusCode::FORBIDDEN);
    assert_eq!(
        app.owner("DELETE", &path, None).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.owner("DELETE", &path, None).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn secrets_reach_the_guest_only_on_stdin_and_are_redacted_from_output() {
    let Setup { app, guest, .. } = setup().await;
    let bot = app.create_bot("Mailer").await;
    let other = app.create_bot("Other").await;
    for (who, name) in [(&bot, "smtp"), (&other, "theirs")] {
        let put = app
            .owner(
                "PUT",
                &format!("/v1/bots/{who}/task-secrets/{name}"),
                Some(json!({ "env": { "SMTP_PASSWORD": PASSWORD } })),
            )
            .await;
        assert_eq!(put.status, StatusCode::CREATED);
    }
    let (_, token) = grant(&app, &bot).await;

    let listed = tool(&app, &token, "computer_secrets", json!({})).await;
    assert_eq!(
        listed,
        json!({ "secrets": [{ "name": "smtp", "env": ["SMTP_PASSWORD"] }] })
    );
    let unknown = tool(
        &app,
        &token,
        "computer_run",
        json!({ "command": "true", "secrets": ["theirs"], "effect": "local" }),
    )
    .await;
    assert!(
        unknown["error"]
            .as_str()
            .unwrap()
            .starts_with("SECRET_UNKNOWN"),
        "{unknown}"
    );

    let ran = tool(
        &app,
        &token,
        "computer_run",
        json!({ "command": "./send.sh", "secrets": ["smtp"], "effect": "local" }),
    )
    .await;
    assert_eq!(
        ran["stdout"], "SMTP_PASSWORD=[secret SMTP_PASSWORD]",
        "{ran}"
    );
    let execs = guest.execs();
    let (program, args, input) = execs
        .iter()
        .find(|(_, args, _)| args.iter().any(|a| a == "./send.sh"))
        .unwrap();
    assert_eq!(program, "/usr/sbin/runuser");
    assert!(
        args.iter().all(|a| !a.contains(PASSWORD)),
        "never an argument"
    );
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(PASSWORD);
    assert_eq!(
        String::from_utf8_lossy(input),
        format!("SMTP_PASSWORD {encoded}\n")
    );

    // A job records its secret names and a redaction set, not the value.
    let job = tool(
        &app,
        &token,
        "computer_job_start",
        json!({ "command": "./send.sh", "secrets": ["smtp"], "effect": "local" }),
    )
    .await;
    let id: Uuid = job["job_id"].as_str().unwrap().parse().unwrap();
    let row: Value = sqlx::query_scalar("SELECT json_object('command', command, 'output_tail', output_tail, 'error', error, 'secrets', json(secrets)) FROM guest_jobs WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(row["secrets"], json!(["smtp"]));
    assert!(!row.to_string().contains(PASSWORD), "names only");
    let launch = guest
        .execs()
        .into_iter()
        .find(|(_, args, _)| args.iter().any(|a| a.contains("echo launched")))
        .unwrap();
    assert!(launch.1.iter().all(|a| !a.contains(PASSWORD)));
    assert!(String::from_utf8_lossy(&launch.2).contains(&encoded));
    // Tool activity names the secret, never its value.
    let logged: Vec<String> = sqlx::query_scalar("SELECT body FROM memory_entry WHERE bot_id = ?1")
        .bind(bot.parse::<Uuid>().unwrap())
        .fetch_all(&app.db.pool)
        .await
        .unwrap();
    assert!(logged.iter().any(|l| l.contains("\"secrets\":[\"smtp\"]")));
    assert!(logged.iter().all(|l| !l.contains(PASSWORD)));
}

/// Stop work ends a foreground command that is still running.
#[tokio::test(flavor = "multi_thread")]
async fn stop_work_ends_a_running_foreground_command() {
    let Setup {
        app,
        guest,
        manager,
    } = setup().await;
    let bot = app.create_bot("Worker").await;
    let (_, token) = grant(&app, &bot).await;
    let app = Arc::new(app);
    let call = {
        let app = app.clone();
        let token = token.clone();
        tokio::spawn(async move {
            tool(
                &app,
                &token,
                "computer_run",
                json!({ "command": "HANG for a long time", "timeout_secs": 300, "effect": "local" }),
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), guest.started.notified())
        .await
        .expect("the foreground command started");
    // While its turn runs, nothing ends it.
    manager.end_orphaned_commands().await.unwrap();
    assert!(!call.is_finished());
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{:?}", stop.body);
    for _ in 0..100 {
        let live: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM turns WHERE bot_id = ?1 AND status = 'running')",
        )
        .bind(bot.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
        if !live {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Until the command ended, the stop stays pending.
    else_api::db::stop::settle_all(&app.db.pool).await.unwrap();
    let receipt = |state: &'static str| {
        let pool = app.db.pool.clone();
        async move {
            let states: Vec<String> =
                sqlx::query_scalar("SELECT state FROM request_receipts WHERE operation = 'cancel'")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert_eq!(states, vec![state.to_owned()]);
        }
    };
    receipt("pending").await;
    manager.end_orphaned_commands().await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .expect("the command ended")
        .unwrap();
    assert_eq!(result["exit_code"], 143, "{result}");
    let kill = guest
        .execs()
        .into_iter()
        .find(|(_, args, _)| args.iter().any(|a| a.contains("alive()")))
        .unwrap();
    assert!(
        kill.1.last().unwrap().starts_with("/var/lib/elsebot/runs/"),
        "{:?}",
        kill.1
    );
    let ended: bool = sqlx::query_scalar("SELECT min(ended_at IS NOT NULL) FROM guest_runs")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert!(ended, "the run is recorded as ended");
    // Once the turn also finished stopping, the stop completes.
    for _ in 0..100 {
        else_api::db::stop::settle_all(&app.db.pool).await.unwrap();
        let done: bool = sqlx::query_scalar(
            "SELECT min(state = 'done') FROM request_receipts WHERE operation = 'cancel'",
        )
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
        if done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    receipt("done").await;
}

/// A computer whose kill never answers does not hold up another bot's.
#[tokio::test(flavor = "multi_thread")]
async fn one_stalled_kill_does_not_delay_another_bots_stop() {
    let Setup {
        app,
        guest,
        manager,
    } = setup().await;
    let slow = app.create_bot("Slow").await;
    let fast = app.create_bot("Fast").await;
    let app = Arc::new(app);
    let mut calls = Vec::new();
    for bot in [&slow, &fast] {
        let (_, token) = grant(&app, bot).await;
        let app = app.clone();
        calls.push(tokio::spawn(async move {
            tool(
                &app,
                &token,
                "computer_run",
                json!({ "command": "HANG", "timeout_secs": 300, "effect": "local" }),
            )
            .await
        }));
        tokio::time::timeout(Duration::from_secs(10), guest.started.notified())
            .await
            .expect("started");
    }
    let slow_name = ComputerManager::runtime_name(slow.parse().unwrap());
    guest.stalled.lock().unwrap().push(slow_name);
    for bot in [&slow, &fast] {
        let stop = app
            .owner(
                "POST",
                &format!("/v1/bots/{bot}/cancel"),
                Some(json!({ "request_id": Uuid::new_v4() })),
            )
            .await;
        assert!(stop.status.is_success());
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    let pass = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.end_orphaned_commands().await })
    };
    let fast_call = calls.pop().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), fast_call)
        .await
        .expect("the other bot's command ended while one kill stalls")
        .unwrap();
    assert_eq!(result["exit_code"], 143);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    // The pass itself returns within its bound, leaving the slow kill.
    tokio::time::timeout(Duration::from_secs(15), pass)
        .await
        .expect("the pass is bounded")
        .unwrap()
        .unwrap();
    // The stalled kill finishes on its own; its command then ends too.
    let slow_call = calls.pop().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(30), slow_call)
        .await
        .expect("the stalled command ended once its kill answered")
        .unwrap();
    assert_eq!(result["exit_code"], 143);
}

/// POC-029 review: foreground work ends with an owner stop (without
/// waiting for the operation lock it holds) and when its request is
/// dropped; a command queued behind it runs nothing once its turn stopped.
#[tokio::test(flavor = "multi_thread")]
async fn owner_stop_dropped_requests_and_stopped_turns_end_or_refuse_commands() {
    let Setup {
        app,
        guest,
        manager,
    } = setup().await;
    let bot = app.create_bot("Clicker").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let (_, token) = grant(&app, &bot).await;
    let app = Arc::new(app);
    let call = |command: &'static str| {
        let app = app.clone();
        let token = token.clone();
        tokio::spawn(async move {
            tool(
                &app,
                &token,
                "computer_run",
                json!({ "command": command, "timeout_secs": 300, "effect": "local" }),
            )
            .await
        })
    };
    let kills = || {
        guest
            .execs()
            .iter()
            .filter(|(_, args, _)| args.iter().any(|a| a.contains("alive()")))
            .count()
    };

    // An owner stop ends the running command at once.
    let first = call("HANG one");
    tokio::time::timeout(Duration::from_secs(10), guest.started.notified())
        .await
        .expect("started");
    let stopped_at = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(10), manager.owner_stop(bot_id))
        .await
        .expect("the owner stop did not wait behind the command")
        .unwrap();
    assert!(stopped_at.elapsed() < Duration::from_secs(5));
    let result = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["exit_code"], 143, "{result}");
    manager.owner_resume(bot_id).await.unwrap();

    // A dropped request ends its command.
    let before = kills();
    let second = call("HANG two");
    tokio::time::timeout(Duration::from_secs(10), guest.started.notified())
        .await
        .expect("started");
    second.abort();
    let started = std::time::Instant::now();
    while kills() == before {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the dropped request's command was not ended"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // A command queued behind a running one, for a turn stopped meanwhile,
    // runs nothing.
    let third = call("HANG three");
    tokio::time::timeout(Duration::from_secs(10), guest.started.notified())
        .await
        .expect("started");
    let queued = call("echo should-not-run");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success());
    for _ in 0..100 {
        let live: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM turns WHERE bot_id = ?1 AND status = 'running')",
        )
        .bind(bot_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
        if !live {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    manager.end_orphaned_commands().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), third)
        .await
        .unwrap();
    let refused = tokio::time::timeout(Duration::from_secs(5), queued)
        .await
        .unwrap()
        .unwrap();
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or_default()
            .contains("STOPPED"),
        "{refused}"
    );
    assert!(
        guest
            .execs()
            .iter()
            .all(|(_, args, _)| args.iter().all(|a| !a.contains("should-not-run"))),
        "the queued command never reached the guest"
    );
}

/// POC-030: a category change touches no computer state: a running guest
/// job keeps its status and lease while its bot is assigned, moved, and
/// its category deleted.
#[tokio::test(flavor = "multi_thread")]
async fn category_changes_leave_guest_jobs_running() {
    let Setup { app, manager, .. } = setup().await;
    let bot = app.create_bot("Jobs").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let else_api::computers::jobs::Started::Job(job) = manager
        .start_job(bot_id, None, "sleep 600", Duration::from_secs(900))
        .await
        .unwrap()
    else {
        panic!("not started")
    };
    let state = || async {
        sqlx::query_as::<_, (String, Option<chrono::DateTime<chrono::Utc>>)>(
            "SELECT j.status, l.released_at FROM guest_jobs j JOIN computer_leases l ON l.id = j.lease_id
             WHERE j.id = ?1",
        )
        .bind(job)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
    };
    assert_eq!(state().await, ("running".to_owned(), None));
    let made = app
        .owner(
            "POST",
            "/v1/categories",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "Batch" })),
        )
        .await;
    let id = made.body["category"]["id"].clone();
    for category in [id.clone(), Value::Null, id.clone()] {
        let reply = app
            .owner(
                "PATCH",
                &format!("/v1/bots/{bot}/category"),
                Some(json!({ "category_id": category })),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
    }
    app.owner(
        "DELETE",
        &format!("/v1/categories/{}", id.as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(
        state().await,
        ("running".to_owned(), None),
        "the job and its lease are untouched"
    );
}

/// Retiring old input counters must never renew a live turn's allowance.
#[tokio::test(flavor = "multi_thread")]
async fn input_allowances_survive_counter_cleanup() {
    let Setup { app, manager, .. } = setup().await;
    let bot = app.create_bot("Input").await;
    let (turn, _) = grant(&app, &bot).await;
    for expected in 1..=41 {
        assert_eq!(manager.count_input(turn).await.unwrap(), expected);
    }
    // These IDs stand for settled turns whose counters have accumulated.
    for _ in 0..10_001 {
        manager.count_input(Uuid::new_v4()).await.unwrap();
    }
    assert_eq!(manager.count_input(turn).await.unwrap(), 42);
}
