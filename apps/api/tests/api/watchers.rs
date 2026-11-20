use crate::common;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::guest::Guest;
use common::recorder::tool;
use common::{TestApp, TestDb};
use else_api::computers::{ComputerConfig, ComputerManager, ComputerRuntime, RuntimeState};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::synthetic::Synthetic;
use serde_json::{Value, json};
use uuid::Uuid;

const SLOW: &str = "[synthetic:slow] Tell me when example.com is free.";
const CHECK: &str = "./domain-free.sh example.com";
const PROMPT: &str = "[synthetic:slow] Register example.com, then tell the owner.";

struct Setup {
    app: TestApp,
    guest: Arc<Guest>,
    manager: Arc<ComputerManager>,
    bot: String,
    token: String,
}

async fn setup() -> Setup {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let guest = Arc::new(Guest::new());
    let key = Arc::new(else_api::credentials::SecretKey::parse(common::SECRET_KEY).unwrap());
    let manager = Arc::new(
        ComputerManager::new(
            db.pool.clone(),
            guest.clone(),
            ComputerConfig {
                idle_after: Duration::from_secs(3600),
                ..ComputerConfig::default()
            },
        )
        .with_secret_key(key),
    );
    manager.reconcile_startup().await.unwrap();
    let synthetic = Arc::new(Synthetic::new(Duration::from_millis(5)));
    let registry: common::Providers = vec![synthetic];
    let app = TestApp::builder()
        .db(db)
        .providers(registry)
        .computers(manager.clone())
        .start()
        .await;
    let bot = app.create_bot("Scout").await;
    let token = owner_turn(&app, &bot).await;
    Setup {
        app,
        guest,
        manager,
        bot,
        token,
    }
}

async fn owner_turn(app: &TestApp, bot: &str) -> String {
    let sent = app.send(bot, Uuid::new_v4(), SLOW, Some("synthetic")).await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let turn: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&turn.to_string(), &["running"]).await;
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    app.state
        .grants
        .issue(
            bot.parse().unwrap(),
            conversation,
            turn,
            vec![Tool::Computer],
            DEFAULT_TTL,
        )
        .await
}

async fn until<T, F: Future<Output = Option<T>>>(what: &str, mut probe: impl FnMut() -> F) -> T {
    let deadline = tokio::time::Instant::now() + common::SETTLE;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(tokio::time::Instant::now() < deadline, "never {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn start(s: &Setup, check: &str, hours: i64) -> common::recorder::Called {
    tool(
        &s.app,
        &s.token,
        "computer_watch_start",
        json!({ "check": check, "hours": hours, "every_minutes": 15,
                "purpose": "example.com becomes available", "prompt": PROMPT }),
    )
    .await
}

async fn started(s: &Setup) -> Uuid {
    let called = start(s, CHECK, 240).await;
    assert!(!called.error, "{}", called.body);
    assert_eq!(called.result.get("parked"), None, "the turn is not parked");
    called.result["watch_id"].as_str().unwrap().parse().unwrap()
}

const TOKEN: &str = "rgx-7f3a9c21-token";

async fn save_secret(s: &Setup, name: &str, env: Value) {
    let saved = s
        .app
        .owner(
            "PUT",
            &format!("/v1/task-secrets/{name}"),
            Some(json!({ "env": env })),
        )
        .await;
    assert!(saved.status.is_success(), "{}", saved.body);
}

async fn start_with_secrets(s: &Setup, secrets: Value) -> common::recorder::Called {
    tool(
        &s.app,
        &s.token,
        "computer_watch_start",
        json!({ "check": CHECK, "hours": 240, "purpose": "example.com becomes available",
                "prompt": PROMPT, "secrets": secrets }),
    )
    .await
}

/// Save the Vault entry `registrar` and start a watcher whose checks get it.
async fn started_with_token(s: &Setup) -> Uuid {
    save_secret(s, "registrar", json!({ "REGISTRAR_TOKEN": TOKEN })).await;
    let called = start_with_secrets(s, json!(["registrar"])).await;
    assert!(!called.error, "{}", called.body);
    assert_eq!(called.result["secrets"], json!(["registrar"]));
    called.result["watch_id"].as_str().unwrap().parse().unwrap()
}

#[derive(Debug, sqlx::FromRow)]
struct Row {
    status: String,
    outcome: Option<String>,
    check_job_id: Option<Uuid>,
    last_status: Option<String>,
    checks: i64,
    wake_turn_id: Option<Uuid>,
}

async fn row(app: &TestApp, id: Uuid) -> Row {
    sqlx::query_as(
        "SELECT status, outcome, check_job_id, last_status, checks, wake_turn_id
         FROM watchers WHERE id = ?1",
    )
    .bind(id)
    .fetch_one(&app.db.pool)
    .await
    .unwrap()
}

async fn job_status(app: &TestApp, job: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM guest_jobs WHERE id = ?1")
        .bind(job)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

/// The watcher's current check job, once it runs.
async fn running_check(s: &Setup, id: Uuid) -> Uuid {
    let job = until("admitted the check", || async {
        row(&s.app, id).await.check_job_id
    })
    .await;
    until("launched the check", || async {
        (job_status(&s.app, job).await == "running").then_some(())
    })
    .await;
    job
}

/// Make the watcher's next check due, and return its running check job.
async fn checking(s: &Setup, id: Uuid) -> Uuid {
    s.guest.report("running\n");
    sqlx::query(
        "UPDATE watchers SET next_check_at = '2000-01-01T00:00:00.000+00:00'
         WHERE id = ?1 AND status = 'waiting'",
    )
    .bind(id)
    .execute(&s.app.db.pool)
    .await
    .unwrap();
    s.app.scheduler.wake();
    running_check(s, id).await
}

async fn check_ends(s: &Setup, job: Uuid, probe: &str) {
    s.guest.report(probe);
    s.manager.job(s.bot.parse().unwrap(), job).await.unwrap();
    s.app.scheduler.wake();
}

async fn settled(s: &Setup, id: Uuid, status: &str) -> Row {
    until("settled the watcher", || async {
        let row = row(&s.app, id).await;
        (row.status == status).then_some(row)
    })
    .await
}

async fn snapshot_watchers(s: &Setup) -> Vec<Value> {
    let reply = s
        .app
        .owner("GET", &format!("/v1/bots/{}/conversation", s.bot), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.body["watchers"].as_array().unwrap().clone()
}

async fn watching(s: &Setup) -> i64 {
    let reply = s.app.owner("GET", "/v1/bots", None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.body["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|bot| bot["id"] == s.bot.as_str())
        .unwrap()["watching"]
        .as_i64()
        .unwrap()
}

async fn wake_text(app: &TestApp, turn: Uuid) -> String {
    sqlx::query_scalar("SELECT text FROM items WHERE turn_id = ?1 AND kind = 'routine'")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn bot_turns(s: &Setup) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM turns WHERE bot_id = ?1")
        .bind(s.bot.parse::<Uuid>().unwrap())
        .fetch_one(&s.app.db.pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_checks_until_found_then_wakes_its_bot_once() {
    let s = setup().await;
    let id = started(&s).await;
    let first = running_check(&s, id).await;
    let command: String = sqlx::query_scalar("SELECT command FROM guest_jobs WHERE id = ?1")
        .bind(first)
        .fetch_one(&s.app.db.pool)
        .await
        .unwrap();
    assert_eq!(command, CHECK);

    check_ends(&s, first, "done 0\nSTATUS: example.com is taken\n").await;
    let waiting = settled(&s, id, "waiting").await;
    assert_eq!(
        (
            waiting.last_status.as_deref(),
            waiting.checks,
            waiting.outcome
        ),
        (Some("example.com is taken"), 1, None)
    );
    let shown = snapshot_watchers(&s).await;
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0]["last_status"], "example.com is taken");
    assert_eq!(shown[0]["purpose"], "example.com becomes available");
    assert_eq!(watching(&s).await, 1);
    let turns = bot_turns(&s).await;

    let second = checking(&s, id).await;
    check_ends(
        &s,
        second,
        "done 0\nSTATUS: free\nexample.com is available\n",
    )
    .await;
    let ended = settled(&s, id, "ended").await;
    assert_eq!(ended.outcome.as_deref(), Some("met"));
    let wake = ended.wake_turn_id.expect("the bot is woken");
    let text = wake_text(&s.app, wake).await;
    assert!(text.starts_with(PROMPT), "{text}");
    assert!(text.contains("example.com is available"), "{text}");
    assert!(!text.contains("STATUS:"), "{text}");
    assert_eq!(bot_turns(&s).await, turns + 1);
    assert_eq!(watching(&s).await, 0);
    assert!(snapshot_watchers(&s).await.is_empty());

    s.app.scheduler.wake();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(bot_turns(&s).await, turns + 1, "woken once");

    s.app.wait_turn(&wake.to_string(), &["running"]).await;
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(wake)
        .fetch_one(&s.app.db.pool)
        .await
        .unwrap();
    let woken = s
        .app
        .state
        .grants
        .issue(
            s.bot.parse().unwrap(),
            conversation,
            wake,
            vec![Tool::Computer],
            DEFAULT_TTL,
        )
        .await;
    let again = tool(
        &s.app,
        &woken,
        "computer_watch_start",
        json!({ "check": CHECK, "hours": 24, "purpose": "example.com becomes available",
                "prompt": PROMPT }),
    )
    .await;
    assert_eq!(
        again.code(),
        "NOT_ALLOWED",
        "a wake cannot start the next watcher itself: {}",
        again.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn three_failed_checks_in_a_row_end_the_watcher_as_failed() {
    let s = setup().await;
    let id = started(&s).await;
    let mut job = running_check(&s, id).await;
    for n in 1..=3 {
        check_ends(&s, job, "done 6\ncurl: (6) Could not resolve host\n").await;
        if n < 3 {
            let waiting = settled(&s, id, "waiting").await;
            assert_eq!(
                waiting.last_status.as_deref(),
                Some("check failed with exit code 6")
            );
            job = checking(&s, id).await;
        }
    }
    let ended = settled(&s, id, "ended").await;
    assert_eq!(ended.outcome.as_deref(), Some("failed"));
    assert!(ended.wake_turn_id.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_check_that_finds_it_after_the_deadline_expires_the_watcher() {
    let s = setup().await;
    let id = started(&s).await;
    let job = running_check(&s, id).await;
    let deadline = (chrono::Utc::now() + chrono::Duration::milliseconds(300))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, false);
    sqlx::query("UPDATE watchers SET watch_until = ?2 WHERE id = ?1")
        .bind(id)
        .bind(deadline)
        .execute(&s.app.db.pool)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    s.guest.report("done 0\nexample.com is available\n");
    s.manager.job(s.bot.parse().unwrap(), job).await.unwrap();
    s.app.scheduler.wake();
    let ended = settled(&s, id, "ended").await;
    assert_eq!(
        ended.outcome.as_deref(),
        Some("expired"),
        "a match after the deadline never tells the bot to act"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owners_stop_of_the_bot_leaves_a_watcher_check_running() {
    let s = setup().await;
    let id = started(&s).await;
    let job = running_check(&s, id).await;
    let stopped = s
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{}/cancel", s.bot),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stopped.status.is_success(), "{}", stopped.body);
    assert_eq!(job_status(&s.app, job).await, "running");
    assert_eq!(row(&s.app, id).await.status, "checking");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_survives_its_computer_restarting() {
    let s = setup().await;
    let id = started(&s).await;
    let first = running_check(&s, id).await;

    check_ends(&s, first, "lost\n").await;
    let waiting = settled(&s, id, "waiting").await;
    assert!(
        waiting.last_status.unwrap().contains("unknown"),
        "a check lost to a restart neither finds nor fails"
    );

    let bot: Uuid = s.bot.parse().unwrap();
    let name: String =
        sqlx::query_scalar("SELECT runtime_name FROM bot_computers WHERE bot_id = ?1")
            .bind(bot)
            .fetch_one(&s.app.db.pool)
            .await
            .unwrap();
    s.guest.stop(&name).await.unwrap();
    s.manager.reconcile_startup().await.unwrap();
    assert_eq!(
        s.guest.list().await.unwrap(),
        vec![(name.clone(), RuntimeState::Stopped)]
    );

    let next = checking(&s, id).await;
    assert_ne!(next, first);
    assert_eq!(
        s.guest.list().await.unwrap(),
        vec![(name, RuntimeState::Running)],
        "the next check starts the computer again"
    );
    check_ends(&s, next, "done 0\n").await;
    let waiting = settled(&s, id, "waiting").await;
    assert_eq!(
        (waiting.last_status.as_deref(), waiting.checks),
        (Some("not yet"), 2)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_past_its_deadline_expires_and_stops_its_check() {
    let s = setup().await;
    let id = started(&s).await;
    let job = running_check(&s, id).await;

    sqlx::query("UPDATE watchers SET watch_until = '2000-01-01T00:00:00.000+00:00' WHERE id = ?1")
        .bind(id)
        .execute(&s.app.db.pool)
        .await
        .unwrap();
    s.app.scheduler.wake();
    let ended = settled(&s, id, "ended").await;
    assert_eq!(ended.outcome.as_deref(), Some("expired"));
    assert_ne!(
        job_status(&s.app, job).await,
        "running",
        "its check is asked to stop"
    );
    let wake = ended.wake_turn_id.expect("the bot is woken");
    assert!(wake_text(&s.app, wake).await.starts_with(PROMPT));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owners_stop_wakes_the_bot_and_the_bots_own_stop_does_not() {
    let s = setup().await;
    let owned = started(&s).await;
    let path = format!("/v1/bots/{}/watchers/{owned}/stop", s.bot);
    let stopped = s.app.owner("POST", &path, None).await;
    assert_eq!(stopped.status, StatusCode::OK, "{}", stopped.body);
    assert_eq!(stopped.body["watcher"]["outcome"], "stopped");
    let ended = settled(&s, owned, "ended").await;
    assert!(
        ended.wake_turn_id.is_some(),
        "the owner's stop wakes the bot"
    );

    let again = s.app.owner("POST", &path, None).await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.body);
    assert_eq!(again.body["error"]["code"], "not_active");

    let own = start(&s, "./other-check.sh", 24).await;
    assert!(!own.error, "{}", own.body);
    let turns = bot_turns(&s).await;
    let dropped = tool(
        &s.app,
        &s.token,
        "computer_watch_stop",
        json!({ "watch_id": own.result["watch_id"] }),
    )
    .await;
    assert!(!dropped.error, "{}", dropped.body);
    assert_eq!(dropped.result["status"], "ended");
    assert_eq!(dropped.result["outcome"], "stopped");
    s.app.scheduler.wake();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        bot_turns(&s).await,
        turns,
        "the bot's own stop wakes nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_runs_at_most_four_watchers_and_a_retry_returns_the_same_one() {
    let s = setup().await;
    let first = started(&s).await;
    let retried = start(&s, CHECK, 240).await;
    assert_eq!(retried.result["watch_id"], first.to_string());
    let changed = tool(
        &s.app,
        &s.token,
        "computer_watch_start",
        json!({ "check": CHECK, "hours": 240, "every_minutes": 60,
                "purpose": "example.com becomes available", "prompt": PROMPT }),
    )
    .await;
    assert!(changed.error, "{}", changed.body);
    assert!(
        changed.result["error"]
            .as_str()
            .unwrap()
            .starts_with("CONFLICT")
    );
    for n in 0..3 {
        let more = start(&s, &format!("./check-{n}.sh"), 24).await;
        assert!(!more.error, "{}", more.body);
    }
    let refused = start(&s, "./check-5.sh", 24).await;
    assert!(refused.error, "{}", refused.body);
    assert_eq!(refused.result["watchers"].as_array().unwrap().len(), 4);
    assert!(
        refused.result["error"]
            .as_str()
            .unwrap()
            .starts_with("WATCHER_LIMIT")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_longer_than_six_hours_is_refused_with_the_watcher_tool_named() {
    let s = setup().await;
    let called = tool(
        &s.app,
        &s.token,
        "computer_job_start",
        json!({ "command": "./poll-domain.sh", "max_minutes": 20160, "effect": "local",
                "purpose": "example.com becomes available" }),
    )
    .await;
    assert!(called.error, "{}", called.body);
    assert_eq!(called.code(), "JOB_TOO_LONG");
    assert!(
        called
            .result
            .as_str()
            .unwrap()
            .contains("computer_watch_start")
    );
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM guest_jobs")
        .fetch_one(&s.app.db.pool)
        .await
        .unwrap();
    assert_eq!(jobs, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_check_gets_its_vault_entry_and_never_shows_its_value() {
    use base64::Engine;
    let s = setup().await;
    let id = started_with_token(&s).await;
    let encoded = base64::engine::general_purpose::STANDARD.encode(TOKEN);
    let mut job = running_check(&s, id).await;
    for n in 1..=2 {
        let (args, input) = s.guest.launches().pop().expect("the check launched");
        assert!(
            String::from_utf8_lossy(&input).contains(&format!("REGISTRAR_TOKEN {encoded}")),
            "check {n} gets the token"
        );
        assert!(args.iter().all(|a| !a.contains(TOKEN)), "never an argument");
        if n == 1 {
            check_ends(
                &s,
                job,
                &format!("done 0\nSTATUS: taken, asked with {TOKEN}\n"),
            )
            .await;
            let waiting = settled(&s, id, "waiting").await;
            assert_eq!(
                waiting.last_status.as_deref(),
                Some("taken, asked with [secret REGISTRAR_TOKEN]")
            );
            job = checking(&s, id).await;
        }
    }
    check_ends(
        &s,
        job,
        &format!("done 0\nexample.com is free; key {TOKEN}\n"),
    )
    .await;
    let ended = settled(&s, id, "ended").await;
    assert_eq!(ended.outcome.as_deref(), Some("met"));
    let text = wake_text(&s.app, ended.wake_turn_id.unwrap()).await;
    assert!(
        text.contains("example.com is free; key [secret REGISTRAR_TOKEN]"),
        "{text}"
    );
    let stored: String = sqlx::query_scalar(
        "SELECT json_object('w', (SELECT json_group_array(json_array(secrets, last_status,
                    last_output)) FROM watchers),
                'j', (SELECT json_group_array(json_array(output_tail, error)) FROM guest_jobs))",
    )
    .fetch_one(&s.app.db.pool)
    .await
    .unwrap();
    assert!(!stored.contains(TOKEN), "{stored}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_check_whose_vault_entry_was_deleted_fails_and_says_why() {
    let s = setup().await;
    let id = started_with_token(&s).await;
    let job = running_check(&s, id).await;
    check_ends(&s, job, "done 0\n").await;
    settled(&s, id, "waiting").await;
    let deleted = s
        .app
        .owner("DELETE", "/v1/task-secrets/registrar", None)
        .await;
    assert!(deleted.status.is_success(), "{}", deleted.body);
    let retried = start_with_secrets(&s, json!(["registrar"])).await;
    assert!(
        !retried.error,
        "a retry still finds its watcher: {}",
        retried.body
    );
    assert_eq!(retried.result["watch_id"], id.to_string());
    let launched = s.guest.launches().len();
    let jobs = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM guest_jobs")
            .fetch_one(&s.app.db.pool)
            .await
            .unwrap()
    };
    let reserved = jobs().await;

    for failures in 1..=3_i64 {
        sqlx::query(
            "UPDATE watchers SET next_check_at = '2000-01-01T00:00:00.000+00:00'
             WHERE id = ?1 AND status = 'waiting'",
        )
        .bind(id)
        .execute(&s.app.db.pool)
        .await
        .unwrap();
        s.app.scheduler.wake();
        let (status, outcome, last_status): (String, Option<String>, Option<String>) =
            until("counted the failed check", || async {
                let (status, outcome, last, counted): (
                    String,
                    Option<String>,
                    Option<String>,
                    i64,
                ) = sqlx::query_as(
                    "SELECT status, outcome, last_status, failures FROM watchers WHERE id = ?1",
                )
                .bind(id)
                .fetch_one(&s.app.db.pool)
                .await
                .unwrap();
                (counted == failures).then_some((status, outcome, last))
            })
            .await;
        assert_eq!(
            last_status.as_deref(),
            Some(
                "check failed: the Vault entry registrar was deleted; save it again or stop this \
                 watcher"
            )
        );
        if failures < 3 {
            assert_eq!((status.as_str(), outcome), ("waiting", None));
        } else {
            assert_ne!(status, "waiting");
            assert_eq!(outcome.as_deref(), Some("failed"));
        }
    }
    assert_eq!(
        s.guest.launches().len(),
        launched,
        "no check ran without it"
    );
    assert_eq!(jobs().await, reserved, "nor was one reserved");
    let ended = settled(&s, id, "ended").await;
    assert!(ended.wake_turn_id.is_some(), "the bot is woken to say why");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_check_whose_vault_entries_cannot_be_read_fails_and_says_why() {
    let s = setup().await;
    save_secret(&s, "registrar", json!({ "REGISTRAR_TOKEN": TOKEN })).await;
    save_secret(&s, "dns", json!({ "DNS_TOKEN": "dns-0123456789" })).await;
    let called = start_with_secrets(&s, json!(["registrar", "dns"])).await;
    assert!(!called.error, "{}", called.body);
    let id: Uuid = called.result["watch_id"].as_str().unwrap().parse().unwrap();
    let job = running_check(&s, id).await;
    check_ends(&s, job, "done 0\n").await;
    settled(&s, id, "waiting").await;
    save_secret(&s, "dns", json!({ "REGISTRAR_TOKEN": "other-0123456789" })).await;
    let launched = s.guest.launches().len();

    sqlx::query(
        "UPDATE watchers SET next_check_at = '2000-01-01T00:00:00.000+00:00' WHERE id = ?1",
    )
    .bind(id)
    .execute(&s.app.db.pool)
    .await
    .unwrap();
    s.app.scheduler.wake();
    let last_status: Option<String> = until("counted the failed check", || async {
        let (failures, last, status): (i64, Option<String>, String) =
            sqlx::query_as("SELECT failures, last_status, status FROM watchers WHERE id = ?1")
                .bind(id)
                .fetch_one(&s.app.db.pool)
                .await
                .unwrap();
        (failures == 1 && status == "waiting").then_some(last)
    })
    .await;
    assert_eq!(
        last_status.as_deref(),
        Some(
            "check failed: its Vault entries could not be read: two secrets set REGISTRAR_TOKEN; \
             name only one of them"
        )
    );
    assert_eq!(
        s.guest.launches().len(),
        launched,
        "the check never launched"
    );
}
