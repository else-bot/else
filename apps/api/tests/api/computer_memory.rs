use crate::common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::guest::Guest;
use common::recorder::{GATE, GATE_EVERY_ATTEMPT, Harness, harness, tool};
use common::{TestApp, TestDb};
use else_api::computers::{ComputerConfig, ComputerManager};
use serde_json::{Value, json};
use uuid::Uuid;

struct Setup {
    h: Harness,
    guest: Arc<Guest>,
    manager: Arc<ComputerManager>,
}

/// A host whose computer budget fits one computer of up to 4 GiB: 1 GiB
/// computers by default, 4608 MiB in all, 256 MiB overhead each.
async fn setup() -> Setup {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let guest = Arc::new(Guest::new());
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        guest.clone(),
        ComputerConfig {
            idle_after: Duration::from_secs(3600),
            ..ComputerConfig::default()
        },
    ));
    manager.reconcile_startup().await.unwrap();
    let h = harness(TestApp::builder().db(db).computers(manager.clone())).await;
    Setup { h, guest, manager }
}

async fn answer(app: &TestApp, bot: &str, question: &Value, choice: &str) {
    let answered = app
        .owner(
            "POST",
            &format!(
                "/v1/bots/{bot}/questions/{}/answer",
                question.as_str().unwrap()
            ),
            Some(json!({ "choice": choice })),
        )
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
}

async fn memory(app: &TestApp, bot: &str) -> Value {
    app.owner("GET", &format!("/v1/bots/{bot}/computer"), None)
        .await
        .body["memory_mib"]
        .clone()
}

/// Start a job, let the guest report how it ended, and read it back.
async fn ended_job(setup: &Setup, bot: &str, token: &str, probe: &str) -> (Uuid, Value) {
    let started = tool(
        &setup.h.app,
        token,
        "computer_job_start",
        json!({ "command": "python3 build_index.py", "effect": "local",
                     "purpose": "Build the search index" }),
    )
    .await
    .result;
    let job: Uuid = started["job_id"].as_str().unwrap().parse().unwrap();
    setup.guest.report(probe);
    setup.manager.job(bot.parse().unwrap(), job).await.unwrap();
    let read = tool(
        &setup.h.app,
        token,
        "computer_job",
        json!({ "job_id": job }),
    )
    .await
    .result;
    (job, read)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approved_resize_restarts_the_computer_bigger_and_reruns_the_job_it_ran_out_of_memory_in()
 {
    let setup = setup().await;
    let h = &setup.h;
    let app = &h.app;
    let bot = app.create_bot("Indexer").await;
    let (turn, token) = h.running(&bot, GATE_EVERY_ATTEMPT).await;
    let (job, failed) = ended_job(&setup, &bot, &token, "done 137 1\nKilled\n").await;
    assert_eq!(failed["status"], "failed", "{failed}");
    assert_eq!(failed["out_of_memory"]["memory_mib"], 1024, "{failed}");
    assert_eq!(
        failed["out_of_memory"]["sizes_mib"],
        json!([2048, 4096]),
        "8 GiB is above the host's budget"
    );

    let args = json!({ "memory_mib": 4096, "retry_job": job,
                       "reason": "build_index.py was killed loading the 2 GiB corpus" });
    let too_big = tool(
        app,
        &token,
        "computer_resize",
        json!({ "memory_mib": 8192, "reason": "more" }),
    )
    .await;
    assert!(
        too_big
            .result
            .as_str()
            .is_some_and(|t| t.starts_with("SIZE_UNAVAILABLE")),
        "{}",
        too_big.body
    );
    let pending = tool(app, &token, "computer_resize", args.clone())
        .await
        .result;
    assert_eq!(pending["status"], "pending", "{pending}");
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body;
    let card = snapshot["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .unwrap()["questions"][0]
        .clone();
    assert_eq!(card["title"], "Raise this bot's computer memory to 4 GiB");
    let context = card["context"].as_str().unwrap();
    assert!(context.starts_with("Memory: 1 GiB → 4 GiB\n"), "{context}");
    assert!(context.contains("python3 build_index.py"), "{context}");
    assert_eq!(
        card["choices"],
        json!([{ "id": "approve", "label": "Approve once" },
               { "id": "deny", "label": "Deny" },
               { "id": "approve_task", "label": "Always allow" }])
    );
    assert_eq!(
        card["action"]["task_scope"],
        json!({ "task": "Raise this bot's computer memory", "target": "up to 4 GiB" })
    );

    answer(app, &bot, &pending["question_id"], "approve").await;
    let token = h.continued(turn, &token).await;
    setup.guest.report("running\n");
    let retried = tool(app, &token, "computer_resize", args).await;
    assert!(
        else_api::app::tools::parks("computer_resize", &retried.body["result"]),
        "{}",
        retried.body
    );
    let retried = retried.result;
    assert_eq!(
        retried["resized"],
        json!({ "status": "resized", "memory_mib": 4096, "restarted": true })
    );
    assert_eq!(setup.guest.configured(), [4096]);
    assert_eq!(memory(app, &bot).await, 4096);
    let rerun: Uuid = retried["job_id"].as_str().unwrap().parse().unwrap();
    assert_ne!(rerun, job);
    let command: String = sqlx::query_scalar("SELECT command FROM guest_jobs WHERE id = ?1")
        .bind(rerun)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(command, "python3 build_index.py");

    let wait = || async {
        sqlx::query_as::<_, (Uuid, String)>(
            "SELECT job_id, status FROM computer_job_waits WHERE turn_id = ?1",
        )
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
    };
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    assert_eq!(wait().await, (rerun, "pending".to_owned()));
    setup.guest.report("done 0\nindexed 1.2M documents\n");
    setup
        .manager
        .job(bot.parse().unwrap(), rerun)
        .await
        .unwrap();
    assert_eq!(
        wait().await,
        (rerun, "settled".to_owned()),
        "the continuation gets the re-run's result"
    );

    h.continued(turn, &token).await;
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;

    let stopped = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/computer/memory"),
            Some(json!({ "memory_mib": null })),
        )
        .await;
    assert_eq!(stopped.status, StatusCode::OK, "{}", stopped.body);
    assert_eq!(stopped.body["memory_mib"], 1024);
    assert_eq!(setup.guest.configured(), [4096, 1024]);
    assert_eq!(memory(app, &bot).await, 1024, "the owner lowered it again");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_resize_keeps_the_size_and_the_continuation_reports_the_oom() {
    let setup = setup().await;
    let h = &setup.h;
    let app = &h.app;
    let bot = app.create_bot("Indexer").await;
    let (turn, token) = h.running(&bot, GATE).await;
    let started = tool(
        app,
        &token,
        "computer_job_start",
        json!({ "command": "python3 build_index.py", "effect": "local",
                     "purpose": "Build the search index" }),
    )
    .await
    .result;
    let job = started["job_id"].clone();
    let parked = tool(
        app,
        &token,
        "computer_job",
        json!({ "job_id": job, "wait": true }),
    )
    .await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    setup.guest.report("done 137 1\nKilled\n");
    setup
        .manager
        .job(bot.parse().unwrap(), job.as_str().unwrap().parse().unwrap())
        .await
        .unwrap();
    app.wait_turn(turn, &["completed"]).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("Out of memory: it likely ran out of your computer's 1 GiB."),
        "{packet}"
    );

    let (turn, token) = h.running(&bot, GATE_EVERY_ATTEMPT).await;
    let args =
        json!({ "memory_mib": 2048, "retry_job": job, "reason": "the index build was killed" });
    let pending = tool(app, &token, "computer_resize", args.clone())
        .await
        .result;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    answer(app, &bot, &pending["question_id"], "deny").await;
    let token = h.continued(turn, &token).await;
    let denied = tool(app, &token, "computer_resize", args).await;
    assert!(denied.error, "{}", denied.body);
    assert!(
        denied
            .result
            .as_str()
            .is_some_and(|t| t.starts_with("DENIED: the owner kept the computer at 1 GiB")),
        "{}",
        denied.body
    );
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("status=\"answered\" answer=\"deny\""),
        "the continuation reports the denial: {packet}"
    );
    assert!(setup.guest.configured().is_empty(), "nothing was resized");
    assert_eq!(memory(app, &bot).await, 1024);
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM guest_jobs")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(jobs, 1, "nothing was re-run");
}

#[tokio::test(flavor = "multi_thread")]
async fn always_allow_covers_smaller_sizes_without_a_card_but_reruns_nothing_unseen() {
    let setup = setup().await;
    let h = &setup.h;
    let app = &h.app;
    let bot = app.create_bot("Indexer").await;
    let (turn, token) = h.running(&bot, GATE_EVERY_ATTEMPT).await;
    let args = json!({ "memory_mib": 4096, "reason": "the test suite was killed" });
    let pending = tool(app, &token, "computer_resize", args.clone())
        .await
        .result;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    answer(app, &bot, &pending["question_id"], "approve_task").await;
    let token = h.continued(turn, &token).await;
    let resized = tool(app, &token, "computer_resize", args).await.result;
    assert_eq!(resized["memory_mib"], 4096, "{resized}");
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    let lowered = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/computer/memory"),
            Some(json!({ "memory_mib": 1024 })),
        )
        .await;
    assert_eq!(lowered.status, StatusCode::OK, "{}", lowered.body);

    let (_, token) = h.running(&bot, GATE).await;
    let (job, _) = ended_job(&setup, &bot, &token, "done 1\nMemoryError\n").await;
    let granted = tool(
        app,
        &token,
        "computer_resize",
        json!({ "memory_mib": 2048, "retry_job": job, "reason": "MemoryError again" }),
    )
    .await
    .result;
    assert_eq!(granted["status"], "resized", "{granted}");
    assert_eq!(granted["memory_mib"], 2048);
    assert!(
        granted["next"]
            .as_str()
            .unwrap()
            .contains("does not re-run jobs"),
        "{granted}"
    );
    let questions: i64 = sqlx::query_scalar("SELECT count(*) FROM owner_questions")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(questions, 1, "the saved grant needed no second card");
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM guest_jobs")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(jobs, 1, "no job ran that the owner did not see");
    assert_eq!(
        setup.guest.configured(),
        [2048],
        "a computer not created yet takes its size at creation"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn always_allow_on_the_card_still_reruns_the_job_the_card_showed() {
    let setup = setup().await;
    let h = &setup.h;
    let app = &h.app;
    let bot = app.create_bot("Indexer").await;
    let (turn, token) = h.running(&bot, GATE_EVERY_ATTEMPT).await;
    let (job, _) = ended_job(&setup, &bot, &token, "done 137 1\nKilled\n").await;
    let args = json!({ "memory_mib": 2048, "retry_job": job, "reason": "the build was killed" });
    let pending = tool(app, &token, "computer_resize", args.clone())
        .await
        .result;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    answer(app, &bot, &pending["question_id"], "approve_task").await;
    let token = h.continued(turn, &token).await;
    setup.guest.report("running\n");
    let retried = tool(app, &token, "computer_resize", args).await.result;
    assert_eq!(retried["resized"]["memory_mib"], 2048, "{retried}");
    assert_eq!(retried["status"], "waiting", "{retried}");
    assert_ne!(
        retried["job_id"],
        json!(job),
        "the job re-runs as a new one"
    );
}
