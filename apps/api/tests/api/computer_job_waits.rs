use crate::common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::guest::Guest;
use common::{TestApp, TestDb};
use else_api::computers::{ComputerConfig, ComputerManager};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::synthetic::Synthetic;
use serde_json::{Value, json};
use uuid::Uuid;

const SLOW: &str = "[synthetic:slow] Run the nightly crawl and report what it found.";

struct Setup {
    app: TestApp,
    synthetic: Arc<Synthetic>,
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
    let synthetic = Arc::new(Synthetic::new(Duration::from_millis(5)));
    let registry = else_api::providers::Registry::new(vec![synthetic.clone()]);
    let app = TestApp::with_parts(db, 4, registry, Some(manager.clone()));
    Setup {
        app,
        synthetic,
        guest,
        manager,
    }
}

async fn running(app: &TestApp, bot: &str) -> (Uuid, String) {
    let sent = app.send(bot, Uuid::new_v4(), SLOW, Some("synthetic")).await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let turn: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
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
    reply.body["result"].clone()
}

fn text(result: &Value) -> Value {
    assert_ne!(result["isError"], true, "{result}");
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

async fn start_job(app: &TestApp, token: &str) -> Uuid {
    let started = text(
        &tool(
            app,
            token,
            "computer_job_start",
            json!({ "command": "./crawl.sh", "effect": "local" }),
        )
        .await,
    );
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

async fn settle(app: &TestApp, turn: Uuid, status: &str) {
    app.wait_turn_for(&turn.to_string(), &[status], common::SETTLE)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_parked_on_a_job_continues_with_its_exit_status_when_it_fails() {
    let Setup {
        app,
        synthetic,
        guest,
        manager,
    } = setup().await;
    let bot = app.create_bot("Scout").await;
    let (turn, token) = running(&app, &bot).await;
    let job = start_job(&app, &token).await;

    let parked = tool(
        &app,
        &token,
        "computer_job",
        json!({ "job_id": job, "wait": true }),
    )
    .await;
    assert!(
        else_api::app::tools::parks("computer_job", &parked),
        "{parked}"
    );
    assert_eq!(text(&parked)["status"], "waiting");
    settle(&app, turn, "waiting").await;
    let parked_at: i64 = sqlx::query_scalar("SELECT max(seq) FROM bot_events WHERE bot_id = ?1")
        .bind(bot.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let shown = shown_waits(&app, &bot, turn).await;
    assert_eq!(shown[0]["job_id"], job.to_string(), "{shown}");

    manager.job(bot.parse().unwrap(), job).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        (
            attempts(&app, turn).await,
            wait_row(&app, turn).await.0.as_str()
        ),
        (1, "pending"),
        "a job that still runs keeps the request parked"
    );

    guest.report("done 3\nTimeoutError: page.goto exceeded 30000 ms\n");
    manager.job(bot.parse().unwrap(), job).await.unwrap();
    settle(&app, turn, "completed").await;
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
    assert_eq!(attempts(&app, turn).await, 2, "the same request continued");
    assert_eq!(wait_row(&app, turn).await, ("settled".into(), Some(2)));
    assert_eq!(shown_waits(&app, &bot, turn).await, json!([]));
    let packet = synthetic.packets().await.last().unwrap().clone();
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
    let Setup {
        app,
        synthetic,
        manager,
        ..
    } = setup().await;
    let bot = app.create_bot("Scout").await;
    let (turn, token) = running(&app, &bot).await;
    let job = start_job(&app, &token).await;
    text(
        &tool(
            &app,
            &token,
            "computer_job",
            json!({ "job_id": job, "wait": true }),
        )
        .await,
    );
    settle(&app, turn, "waiting").await;

    manager.cancel_job(bot.parse().unwrap(), job).await.unwrap();
    settle(&app, turn, "completed").await;
    assert_eq!(wait_row(&app, turn).await, ("settled".into(), Some(2)));
    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("status=\"cancelled\" exit_code=\"130\""),
        "{packet}"
    );

    let (next, token) = running(&app, &bot).await;
    let ended = tool(
        &app,
        &token,
        "computer_job",
        json!({ "job_id": job, "wait": true }),
    )
    .await;
    assert!(!else_api::app::tools::parks("computer_job", &ended));
    assert_eq!(text(&ended)["status"], "cancelled", "{ended}");
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM turn_holds WHERE turn_id = ?1")
        .bind(next)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(held, 0, "nothing waits on a job that already ended");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_wait_is_refused_and_a_follow_up_withdraws_the_wait() {
    let Setup {
        app,
        guest,
        manager,
        ..
    } = setup().await;
    let bot = app.create_bot("Scout").await;
    let (turn, token) = running(&app, &bot).await;
    let job = start_job(&app, &token).await;
    let other = start_job(&app, &token).await;
    let unknown = tool(
        &app,
        &token,
        "computer_job",
        json!({ "job_id": Uuid::new_v4(), "wait": true }),
    )
    .await;
    assert!(unknown.to_string().contains("NO_SUCH_JOB"), "{unknown}");
    let first = text(
        &tool(
            &app,
            &token,
            "computer_job",
            json!({ "job_id": job, "wait": true }),
        )
        .await,
    );
    let parked = app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": "computer_job",
                                     "arguments": { "job_id": other, "wait": true } } })),
            Some(&token),
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
    settle(&app, turn, "waiting").await;

    let follow_up = app
        .send(
            &bot,
            Uuid::new_v4(),
            "Never mind, skip the crawl.",
            Some("synthetic"),
        )
        .await;
    assert_eq!(follow_up.status, StatusCode::ACCEPTED, "{}", follow_up.body);
    settle(&app, turn, "interrupted").await;
    assert_eq!(wait_row(&app, turn).await, ("cancelled".into(), None));

    guest.report("done 0\n");
    manager.job(bot.parse().unwrap(), job).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        app.wait_turn(&turn.to_string(), &["interrupted"]).await,
        "interrupted",
        "a withdrawn wait does not revive its request"
    );
    assert_eq!(attempts(&app, turn).await, 1);
}
