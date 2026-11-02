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
use uuid::Uuid;

/// Answers with scripted replies, then echoes a short summary.
#[derive(Default)]
struct Scripted {
    replies: Mutex<VecDeque<Result<String, String>>>,
    prompts: Mutex<Vec<String>>,
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
    ) -> Result<String, String> {
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
    assert_eq!(worker.step().await.unwrap(), Step::Deferred);
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
    assert_eq!(worker.step().await.unwrap(), Step::Deferred);
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
