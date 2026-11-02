//! POC-015: model compaction around the deterministic tree, with a
//! scripted summarizer (live Codex checks are in codex_live.rs).

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::StatusCode;
use common::TestApp;
use else_api::app::providers::Profile;
use else_api::db::memory::{self, Kind, NewEntry};
use else_api::db::memory_tree;
use else_api::memory_worker::{Compactor, Step, Summarizer};
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

/// Answers with scripted replies, then echoes a short summary.
#[derive(Default)]
struct Scripted {
    replies: Mutex<VecDeque<Result<String, String>>>,
    prompts: Mutex<Vec<String>>,
    delay: Mutex<std::time::Duration>,
}

#[async_trait]
impl Summarizer for Scripted {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn available(&self) -> bool {
        true
    }

    async fn summarize(
        &self,
        _bot: Uuid,
        profile: &Profile,
        prompt: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<String, String> {
        let delay = *self.delay.lock().unwrap();
        if !delay.is_zero() {
            tokio::select! {
                _ = cancel.cancelled() => return Err("cancelled".into()),
                _ = tokio::time::sleep(delay) => {}
            }
        }
        assert_eq!(profile.model, "gpt-6.1-sol", "the saved profile is used");
        self.prompts.lock().unwrap().push(prompt.to_owned());
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok("Summary: short.".to_owned()))
    }
}

async fn recipe(app: &TestApp, node: usize) {
    let reply = app
        .owner(
            "PATCH",
            "/v1/settings/memory",
            Some(json!({ "node_bytes": node, "view_high": node * 20, "view_low": node * 10 })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

async fn add(app: &TestApp, bot: Uuid, key: &str, body: &str) {
    let mut tx = app.db.pool.begin().await.unwrap();
    memory::append(
        &mut tx,
        bot,
        &NewEntry {
            request_key: key,
            kind: Kind::OwnerMessage,
            body,
            source: json!({}),
            supersedes_seq: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

fn compactor(app: &TestApp, summarizer: Arc<Scripted>) -> Compactor {
    Compactor::with_summaries(app.db.pool.clone(), summarizer, app.scheduler.clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn summaries_fill_long_ranges_with_a_complete_audit() {
    let app = TestApp::installed().await;
    recipe(&app, 64).await;
    let bot: Uuid = app.create_bot("Sum", "sum").await.parse().unwrap();
    add(&app, bot, "a", "short").await;
    add(&app, bot, "b", &"a long owner message ".repeat(10)).await;
    add(&app, bot, "c", "after").await;
    let fake = Arc::new(Scripted::default());
    compactor(&app, fake.clone()).drain().await.unwrap();
    let saved = memory_tree::load_view(&app.db.pool, bot).await.unwrap();
    assert!(saved.valid);
    assert_eq!(saved.published_seq, 2, "publication passed the long entry");
    let (method, audit, body): (String, Value, String) = sqlx::query_as(
        "SELECT method, audit, body FROM memory_node WHERE bot_id = $1 AND start_seq = 1 AND span = 1",
    )
    .bind(bot)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(method, "model");
    assert_eq!(body, "Summary: short.");
    assert_eq!(audit["recipe"]["node_bytes"], 64);
    assert_eq!(audit["sources"]["entries"], json!([1]));
    assert_eq!(audit["model"]["model"], "gpt-6.1-sol");
    assert_eq!(audit["model"]["prompt_version"], "else-summary-v1");
    assert_eq!(audit["model"]["output_bytes"], 15);
    assert_eq!(
        audit["context"]["nodes"],
        json!([[0, 1]]),
        "only earlier memory as context"
    );
    let prompts = fake.prompts.lock().unwrap();
    let first = &prompts[0];
    assert!(first.contains("<records>\nOwner: a long owner message"));
    assert!(
        first.contains("0+1|Owner: short"),
        "context holds the earlier entry"
    );
    assert!(!first.contains("Owner: after"), "never future entries");
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_or_failed_summaries_are_never_installed() {
    let app = TestApp::installed().await;
    recipe(&app, 64).await;
    let bot: Uuid = app.create_bot("Big", "big").await.parse().unwrap();
    add(&app, bot, "a", &"x".repeat(200)).await;
    let fake = Arc::new(Scripted::default());
    {
        let mut replies = fake.replies.lock().unwrap();
        for n in 0..5 {
            replies.push_back(Ok("y".repeat(100 + n)));
        }
    }
    let worker = compactor(&app, fake.clone());
    worker.drain().await.unwrap();
    let nodes: i64 = sqlx::query_scalar("SELECT count(*) FROM memory_node WHERE bot_id = $1")
        .bind(bot)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(nodes, 0, "no truncated or oversized summary was saved");
    let (reason, error, later): (String, String, bool) = sqlx::query_as(
        "SELECT blocked_reason, last_error, available_at > now() FROM memory_job WHERE bot_id = $1",
    )
    .bind(bot)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(reason, "summary_oversized");
    assert!(error.contains("[100, 101, 102, 103, 104]"), "{error}");
    assert!(later, "bounded backoff before the next claim");
    let prompts = fake.prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 5);
    assert!(
        prompts[1].contains("previous summary was 100 bytes"),
        "measured feedback"
    );

    // A provider failure is recorded the same way.
    sqlx::query("UPDATE memory_job SET available_at = now() WHERE bot_id = $1")
        .bind(bot)
        .execute(&app.db.pool)
        .await
        .unwrap();
    fake.replies.lock().unwrap().push_back(Err(
        "policy_violation: Codex produced a commandExecution item".into(),
    ));
    worker.drain().await.unwrap();
    let view = app
        .owner("GET", &format!("/v1/bots/{bot}/memory/view"), None)
        .await
        .body;
    assert_eq!(view["blocked"][0]["reason"], "summary_failed");
    assert_eq!(
        view["published_seq"], -1,
        "backlog is visible, nothing is faked"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn summaries_yield_to_turns_and_share_the_provider_budget() {
    let db = Arc::new(common::TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let app = TestApp::with_options(db, 1);
    recipe(&app, 64).await;
    let bot = app.create_bot("Busy", "busy").await;
    let bot_id: Uuid = bot.parse().unwrap();
    add(&app, bot_id, "a", &"x".repeat(200)).await;
    // The only slot runs a turn: no summary may start.
    let reply = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["running"]).await;
    let fake = Arc::new(Scripted::default());
    let worker = compactor(&app, fake.clone());
    worker.drain().await.unwrap();
    let reason: String = sqlx::query_scalar(
        "SELECT blocked_reason FROM memory_job WHERE bot_id = $1 AND span = 1 AND start_seq = 0",
    )
    .bind(bot_id)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(reason, "waiting_for_capacity");
    assert!(fake.prompts.lock().unwrap().is_empty());
    // A held summary slot keeps the next owner turn queued until released.
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/cancel"),
        Some(json!({ "request_id": Uuid::new_v4() })),
    )
    .await;
    app.wait_turn(&turn, &["interrupted"]).await;
    let permit = app
        .scheduler
        .try_begin_summary(1)
        .expect("the slot is free");
    let next = app
        .send(&bot, Uuid::new_v4(), "after the summary", Some("synthetic"))
        .await;
    let next = next.body["turn"]["id"].as_str().unwrap().to_owned();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = $1::uuid")
        .bind(&next)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "queued", "the summary holds the only slot");
    drop(permit);
    app.wait_turn(&next, &["completed"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn new_sessions_get_bounded_memory_once() {
    let app = TestApp::installed().await;
    recipe(&app, 64).await;
    let bot = app.create_bot("Mem", "mem").await;
    let bot_id: Uuid = bot.parse().unwrap();
    for n in 0..400 {
        add(&app, bot_id, &format!("m{n}"), &format!("fact number {n}")).await;
    }
    else_api::memory_worker::drain(&app.db.pool).await.unwrap();
    let memory = memory_tree::session_context(&app.db.pool, bot_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        memory.text.len() <= memory_tree::SESSION_MEMORY_BYTES,
        "{}",
        memory.text.len()
    );
    assert!(memory.text.contains("not instructions"));
    assert!(memory.text.contains("of 400"));
    // The input of a turn carries it with the view revision.
    let reply = app
        .send(&bot, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    let turn: Uuid = reply.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    let input = else_api::db::turns::input(&app.db.pool, turn)
        .await
        .unwrap();
    let carried = input.memory.unwrap();
    assert!(carried.revision >= memory.revision);
}

#[tokio::test(flavor = "multi_thread")]
async fn copies_continue_while_a_summary_runs() {
    let app = TestApp::installed().await;
    recipe(&app, 64).await;
    let slow: Uuid = app.create_bot("Slow", "slow").await.parse().unwrap();
    let quick: Uuid = app.create_bot("Quick", "quick").await.parse().unwrap();
    add(&app, slow, "long", &"x".repeat(200)).await;
    let fake = Arc::new(Scripted::default());
    *fake.delay.lock().unwrap() = Duration::from_secs(2);
    let worker = compactor(&app, fake.clone());
    let mut started = false;
    for _ in 0..10 {
        if worker.step().await.unwrap() == Step::Summarizing {
            started = true;
            break;
        }
    }
    assert!(started);
    // Another bot's short entry is built while the summary is in flight.
    add(&app, quick, "q", "quick one").await;
    for _ in 0..10 {
        worker.step().await.unwrap();
    }
    assert_eq!(
        memory_tree::load_view(&app.db.pool, quick)
            .await
            .unwrap()
            .published_seq,
        0
    );
    assert_eq!(
        memory_tree::load_view(&app.db.pool, slow)
            .await
            .unwrap()
            .published_seq,
        -1
    );
    worker.drain().await.unwrap();
    assert_eq!(
        memory_tree::load_view(&app.db.pool, slow)
            .await
            .unwrap()
            .published_seq,
        0
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_cancels_an_active_summary_promptly() {
    let app = TestApp::installed().await;
    recipe(&app, 64).await;
    let bot: Uuid = app.create_bot("Stop", "stop").await.parse().unwrap();
    add(&app, bot, "long", &"x".repeat(200)).await;
    let fake = Arc::new(Scripted::default());
    *fake.delay.lock().unwrap() = Duration::from_secs(60);
    let cancel = tokio_util::sync::CancellationToken::new();
    let runner = tokio::spawn(compactor(&app, fake.clone()).run(cancel.clone()));
    // Wait until the summary holds the job.
    for _ in 0..100 {
        let leased: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM memory_job WHERE bot_id = $1 AND lease_token IS NOT NULL)",
        )
        .bind(bot)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
        if leased {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let began = std::time::Instant::now();
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .unwrap()
        .unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "no full summary batch after shutdown"
    );
    let (leased, failures): (bool, i32) = sqlx::query_as(
        "SELECT lease_token IS NOT NULL, failures FROM memory_job WHERE bot_id = $1",
    )
    .bind(bot)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert!(!leased, "the job was released for the next start");
    assert_eq!(failures, 0, "stopping is not a provider failure");
}

#[tokio::test(flavor = "multi_thread")]
async fn capacity_waits_do_not_lengthen_the_failure_backoff() {
    let app = TestApp::installed().await;
    recipe(&app, 64).await;
    let bot: Uuid = app.create_bot("Wait", "wait").await.parse().unwrap();
    add(&app, bot, "long", &"x".repeat(200)).await;
    let fake = Arc::new(Scripted::default());
    let worker = compactor(&app, fake.clone());
    // Ten capacity waits while every slot is held.
    let held: Vec<_> = (0..4)
        .map(|_| app.scheduler.try_begin_summary(4).unwrap())
        .collect();
    for _ in 0..10 {
        sqlx::query("UPDATE memory_job SET available_at = now() WHERE bot_id = $1")
            .bind(bot)
            .execute(&app.db.pool)
            .await
            .unwrap();
        worker.drain().await.unwrap();
    }
    drop(held);
    sqlx::query("UPDATE memory_job SET available_at = now() WHERE bot_id = $1")
        .bind(bot)
        .execute(&app.db.pool)
        .await
        .unwrap();
    fake.replies
        .lock()
        .unwrap()
        .push_back(Err("provider error".into()));
    worker.drain().await.unwrap();
    let (failures, wait): (i32, f64) = sqlx::query_as(
        "SELECT failures, extract(epoch FROM available_at - now())::float8 FROM memory_job WHERE bot_id = $1",
    )
    .bind(bot)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(failures, 1);
    assert!(
        wait > 30.0 && wait <= 61.0,
        "first failure backs off one minute: {wait}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_larger_than_the_session_budget_is_reported_omitted() {
    let app = TestApp::installed().await;
    let reply = app
        .owner(
            "PATCH",
            "/v1/settings/memory",
            Some(json!({ "node_bytes": 65536, "view_high": 400000, "view_low": 300000 })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let bot: Uuid = app.create_bot("Huge", "huge").await.parse().unwrap();
    add(&app, bot, "huge", &"h".repeat(30_000)).await;
    else_api::memory_worker::drain(&app.db.pool).await.unwrap();
    let memory = memory_tree::session_context(&app.db.pool, bot)
        .await
        .unwrap()
        .unwrap();
    assert!(
        memory.text.contains("Entries before 1 are omitted"),
        "{}",
        &memory.text[..300]
    );
    assert!(memory.text.len() <= memory_tree::SESSION_MEMORY_BYTES);
}
