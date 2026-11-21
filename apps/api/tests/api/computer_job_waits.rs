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

async fn start_job(app: &TestApp, token: &str) -> Uuid {
    let started = tool(
        app,
        token,
        "computer_job_start",
        json!({ "command": "./crawl.sh", "effect": "local",
                     "purpose": "Nightly crawl" }),
    )
    .await
    .result;
    started["job_id"].as_str().unwrap().parse().unwrap()
}

async fn wait_row(app: &TestApp, turn: Uuid) -> (String, Option<i32>) {
    sqlx::query_as("SELECT status, delivered_attempt FROM computer_job_waits WHERE turn_id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
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
        .unwrap()["computer_job_waits"]
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_parked_on_a_job_continues_with_its_exit_status_when_it_fails() {
    let Setup { h, guest, manager } = setup().await;
    let app = &h.app;
    let bot = app.create_bot("Scout").await;
    let (turn, token) = h.running(&bot, GATE).await;
    let job = start_job(app, &token).await;

    let parked = tool(
        app,
        &token,
        "computer_job",
        json!({ "job_id": job, "wait": true }),
    )
    .await;
    assert!(
        else_api::app::tools::parks("computer_job", &parked.body["result"]),
        "{}",
        parked.body
    );
    assert_eq!(parked.result["status"], "waiting");
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let parked_at: i64 = sqlx::query_scalar("SELECT max(seq) FROM bot_events WHERE bot_id = ?1")
        .bind(bot.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let shown = shown_waits(app, &bot, turn).await;
    assert_eq!(shown[0]["job_id"], job.to_string(), "{shown}");
    let waiting = app.waiting_on(&bot).await;
    assert_eq!(
        (&waiting["bots"], &waiting["jobs"]),
        (&json!([]), &json!(1)),
        "the bot list says it waits on its job: {waiting}"
    );
    assert!(waiting["since"].is_string(), "{waiting}");

    manager.job(bot.parse().unwrap(), job).await.unwrap();
    common::never_polled(
        "the request leaves its wait while the job still runs",
        Duration::from_secs(2),
        || async {
            let attempt = attempts(app, turn).await;
            let status = wait_row(app, turn).await.0;
            (attempt != 1 || status != "pending").then_some((attempt, status))
        },
    )
    .await;

    guest.report("done 3\nTimeoutError: page.goto exceeded 30000 ms\n");
    manager.job(bot.parse().unwrap(), job).await.unwrap();
    app.wait_turn(turn, &["completed"]).await;
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM bot_events WHERE kind = 'turn.updated'
           AND json_extract(data, '$.turn.id') = ?1
           AND json_extract(data, '$.turn.status') = 'queued' AND seq > ?2",
    )
    .bind(turn.to_string())
    .bind(parked_at)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert!(queued >= 1, "the owner sees the request queued again");
    assert_eq!(attempts(app, turn).await, 2, "the same request continued");
    assert_eq!(wait_row(app, turn).await, ("settled".into(), Some(2)));
    assert_eq!(shown_waits(app, &bot, turn).await, json!([]));
    assert_eq!(app.waiting_on(&bot).await, Value::Null, "the job ended");
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("[else.bot computer job results]"),
        "{packet}"
    );
    assert!(
        packet.contains(&format!(
            "<computer_job id=\"{job}\" status=\"failed\" exit_code=\"3\">"
        )),
        "{packet}"
    );
    assert!(packet.contains("page.goto exceeded"), "{packet}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_job_also_continues_the_request_and_an_ended_job_returns_at_once() {
    let Setup { h, manager, .. } = setup().await;
    let app = &h.app;
    let bot = app.create_bot("Scout").await;
    let (turn, token) = h.running(&bot, GATE).await;
    let job = start_job(app, &token).await;
    wait_job(app, &token, job).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;

    manager.cancel_job(bot.parse().unwrap(), job).await.unwrap();
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await, ("settled".into(), Some(2)));
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("status=\"cancelled\" exit_code=\"130\""),
        "{packet}"
    );

    let (next, token) = h.running(&bot, GATE).await;
    let ended = tool(
        app,
        &token,
        "computer_job",
        json!({ "job_id": job, "wait": true }),
    )
    .await;
    assert!(!else_api::app::tools::parks(
        "computer_job",
        &ended.body["result"]
    ));
    assert_eq!(ended.result["status"], "cancelled", "{}", ended.body);
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM turn_holds WHERE turn_id = ?1")
        .bind(next)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(held, 0, "nothing waits on a job that already ended");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_wait_is_refused_and_a_follow_up_continues_the_waiting_reply() {
    let Setup {
        h, guest, manager, ..
    } = setup().await;
    let app = &h.app;
    let bot = app.create_bot("Scout").await;
    let (turn, token) = h.running(&bot, GATE).await;
    let job = start_job(app, &token).await;
    let other = start_job(app, &token).await;
    let unknown = tool(
        app,
        &token,
        "computer_job",
        json!({ "job_id": Uuid::new_v4(), "wait": true }),
    )
    .await;
    assert_eq!(unknown.code(), "NO_SUCH_JOB", "{}", unknown.body);
    let first = tool(
        app,
        &token,
        "computer_job",
        json!({ "job_id": job, "wait": true }),
    )
    .await
    .result;
    let parked = tool(
        app,
        &token,
        "computer_job",
        json!({ "job_id": other, "wait": true }),
    )
    .await;
    assert_eq!(parked.status, StatusCode::CONFLICT, "{}", parked.body);
    use else_api::db::computer_job_waits::{CreateError, Created, create};
    let id: Uuid = first["wait_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        create(&app.db.pool, turn, 1, job).await,
        Ok(Created::Waiting(id)),
        "a retry is the same wait"
    );
    assert_eq!(
        create(&app.db.pool, turn, 1, other).await,
        Err(CreateError::AttemptParked)
    );
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;

    let next = joined(app, &bot, turn, "Also summarize the crawl when it ends.", 2).await;
    app.wait_turn(turn, &["waiting"]).await;
    assert_eq!(wait_row(app, turn).await, ("pending".into(), None));
    assert_eq!(turn_status(app, next).await, "queued");

    guest.report("done 0\n");
    manager.job(bot.parse().unwrap(), job).await.unwrap();
    app.wait_turn(turn, &["completed"]).await;
    app.wait_turn(next, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await, ("settled".into(), Some(3)));
    assert_eq!(
        attempts(app, turn).await,
        3,
        "the message and the job's end continued the same reply"
    );
}

async fn job_status(app: &TestApp, job: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM guest_jobs WHERE id = ?1")
        .bind(job)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_keeps_a_waited_job_and_the_owner_stops_that_wait_and_its_job() {
    let Setup { h, manager, .. } = setup().await;
    let app = &h.app;
    let bot = app.create_bot("Scout").await;
    let (turn, token) = h.running(&bot, GATE).await;
    let job = start_job(app, &token).await;
    let loose = start_job(app, &token).await;
    wait_job(app, &token, job).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;

    let stopped = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stopped.status.is_success(), "{}", stopped.body);
    assert_eq!(job_status(app, loose).await, "cancelling");
    manager
        .cancel_job(bot.parse().unwrap(), loose)
        .await
        .unwrap();
    else_api::db::stop::settle_all(&app.db.pool).await.unwrap();
    assert_eq!(
        (
            job_status(app, job).await.as_str(),
            wait_row(app, turn).await.0.as_str()
        ),
        ("running", "pending"),
        "Stop leaves the waited job to its wait"
    );
    assert_eq!(app.wait_turn(turn, &["waiting"]).await, "waiting");

    let shown = shown_waits(app, &bot, turn).await;
    assert_eq!(shown[0]["command"], "./crawl.sh", "{shown}");
    assert_eq!(shown[0]["purpose"], "Nightly crawl", "{shown}");
    let wait = shown[0]["id"].as_str().unwrap();
    let cancel = app
        .owner("POST", &format!("/v1/bots/{bot}/waits/{wait}/cancel"), None)
        .await;
    assert_eq!(cancel.status, StatusCode::OK, "{}", cancel.body);
    assert_eq!(cancel.body["kind"], "computer_job", "{}", cancel.body);
    assert_eq!(job_status(app, job).await, "cancelling");
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await, ("stopped".into(), Some(2)));
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("status=\"stopped\""), "{packet}");
    assert!(packet.contains("Cancelled by owner"), "{packet}");
}

/// Send `text` while `into` waits: it joins that reply, which continues
/// with it as attempt `attempt`. Returns the message's own turn.
async fn joined(app: &TestApp, bot: &str, into: Uuid, text: &str, attempt: i32) -> Uuid {
    let sent = app.send(bot, Uuid::new_v4(), text, Some("synthetic")).await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["steered_into"], into.to_string());
    common::until(
        &format!("the message to continue the reply as attempt {attempt}"),
        common::poll(|| async { (attempts(app, into).await >= attempt).then_some(()) }),
    )
    .await;
    sent.body["turn"]["id"].as_str().unwrap().parse().unwrap()
}

async fn turn_status(app: &TestApp, turn: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn waits(app: &TestApp, turn: Uuid) -> Vec<(i32, String, Option<i32>)> {
    sqlx::query_as(
        "SELECT attempt, status, delivered_attempt FROM computer_job_waits
         WHERE turn_id = ?1 ORDER BY created_at",
    )
    .bind(turn)
    .fetch_all(&app.db.pool)
    .await
    .unwrap()
}

async fn wait_job(app: &TestApp, token: &str, job: Uuid) {
    let waited = tool(
        app,
        token,
        "computer_job",
        json!({ "job_id": job, "wait": true }),
    )
    .await;
    assert_eq!(waited.status, StatusCode::OK, "{}", waited.body);
    assert!(!waited.error, "{}", waited.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_joined_reply_keeps_its_job_and_a_question_it_then_asks_still_blocks() {
    let Setup { h, manager, .. } = setup().await;
    let app = &h.app;
    let bot = app.create_bot("Scout").await;
    let (turn, token) = h.running(&bot, GATE_EVERY_ATTEMPT).await;
    let crawl = start_job(app, &token).await;
    wait_job(app, &token, crawl).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;

    let next = joined(app, &bot, turn, "Index the archive too.", 2).await;
    let token = h.continued(turn, &token).await;
    let asked = tool(
        app,
        &token,
        "ask_owner",
        json!({ "question": "Index the archive too?", "choices": ["Yes", "No"] }),
    )
    .await;
    let question: Uuid = asked.result["question_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    manager
        .cancel_job(bot.parse().unwrap(), crawl)
        .await
        .unwrap();
    common::never_polled(
        "the turn continues while a question is pending",
        Duration::from_secs(1),
        || async {
            let attempt = attempts(app, turn).await;
            (attempt != 2).then_some(attempt)
        },
    )
    .await;
    assert_eq!(waits(app, turn).await, vec![(1, "settled".into(), None)]);
    assert_eq!(turn_status(app, next).await, "queued");

    let chosen = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/questions/{question}/answer"),
            Some(json!({ "choice": "a" })),
        )
        .await;
    assert_eq!(chosen.status, StatusCode::OK, "{}", chosen.body);
    h.continued(turn, &token).await;
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    app.wait_turn(next, &["completed"]).await;
    assert_eq!(attempts(app, turn).await, 3);
    assert_eq!(waits(app, turn).await, vec![(1, "settled".into(), Some(3))]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_joined_reply_that_waits_again_on_its_job_keeps_the_one_wait() {
    let Setup { h, manager, .. } = setup().await;
    let app = &h.app;
    let bot = app.create_bot("Scout").await;
    let (turn, token) = h.running(&bot, GATE_EVERY_ATTEMPT).await;
    let crawl = start_job(app, &token).await;
    let first = tool(
        app,
        &token,
        "computer_job",
        json!({ "job_id": crawl, "wait": true }),
    )
    .await
    .result;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;

    let next = joined(app, &bot, turn, "Is the crawl done yet?", 2).await;
    let token = h.continued(turn, &token).await;
    let again = tool(
        app,
        &token,
        "computer_job",
        json!({ "job_id": crawl, "wait": true }),
    )
    .await;
    assert!(!again.error, "{}", again.body);
    assert_eq!(again.result["wait_id"], first["wait_id"], "{}", again.body);
    assert_eq!(again.result["status"], "already_waiting", "{}", again.body);
    assert_eq!(again.result["parked"], false, "{}", again.body);
    let asked = tool(
        app,
        &token,
        "ask_owner",
        json!({ "question": "Keep crawling?", "choices": ["Yes", "No"] }),
    )
    .await;
    assert!(
        !asked.error,
        "the attempt holds nothing yet: {}",
        asked.body
    );
    let question = asked.result["question_id"].as_str().unwrap().to_owned();
    use else_api::db::computer_job_waits::{CreateError, create};
    assert_eq!(
        create(&app.db.pool, turn, 2, crawl).await,
        Err(CreateError::AttemptParked),
        "an attempt that asked the owner cannot claim the carried wait"
    );
    assert_eq!(waits(app, turn).await, vec![(1, "pending".into(), None)]);
    assert_eq!(
        shown_waits(app, &bot, turn).await.as_array().unwrap().len(),
        1
    );
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;

    manager
        .cancel_job(bot.parse().unwrap(), crawl)
        .await
        .unwrap();
    let chosen = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/questions/{question}/answer"),
            Some(json!({ "choice": "a" })),
        )
        .await;
    assert_eq!(chosen.status, StatusCode::OK, "{}", chosen.body);
    h.continued(turn, &token).await;
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    app.wait_turn(next, &["completed"]).await;
    assert_eq!(waits(app, turn).await, vec![(1, "settled".into(), Some(3))]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_joined_reply_can_wait_on_another_job_beside_its_first() {
    let Setup { h, .. } = setup().await;
    let app = &h.app;
    let bot = app.create_bot("Scout").await;
    let (turn, token) = h.running(&bot, GATE_EVERY_ATTEMPT).await;
    let crawl = start_job(app, &token).await;
    let index = start_job(app, &token).await;
    wait_job(app, &token, crawl).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;

    joined(app, &bot, turn, "Index the archive too.", 2).await;
    let token = h.continued(turn, &token).await;
    let waited = tool(
        app,
        &token,
        "computer_job",
        json!({ "job_id": index, "wait": true }),
    )
    .await;
    assert_eq!(waited.result["status"], "waiting", "{}", waited.body);
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    assert_eq!(
        waits(app, turn).await,
        vec![(1, "pending".into(), None), (2, "pending".into(), None)]
    );
}
