use crate::common;

use axum::http::StatusCode;
use chrono::DateTime;
use common::TestApp;
use common::recorder::{self, GATE, GATE_EVERY_ATTEMPT, Harness, tool};
use serde_json::{Value, json};
use uuid::Uuid;

async fn shown_bot(app: &TestApp, bot: &str) -> Value {
    let reply = app.owner("GET", &format!("/v1/bots/{bot}"), None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.body["bot"].clone()
}

async fn routine(app: &TestApp, bot: &str) -> Value {
    let reply = app
        .owner("GET", &format!("/v1/bots/{bot}/routines"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.body["routines"][0].clone()
}

async fn shown_turn(app: &TestApp, bot: &str, turn: Uuid) -> Value {
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body;
    snapshot["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .unwrap()
        .clone()
}

async fn no_news(app: &TestApp, token: &str) -> recorder::Called {
    tool(
        app,
        token,
        "use_tool",
        json!({ "name": "no_news", "arguments": {} }),
    )
    .await
}

const UPTIME: &str = "[gate] Check that example.com answers.";

/// Save a routine from an owner turn, read everything so far, and start
/// its first run, held running. Returns the routine and the run's turn
/// and grant.
async fn start_run(h: &Harness, bot: &str, quiet: bool, task: &str) -> (Value, Uuid, String) {
    let (setup, token) = h.running(bot, GATE).await;
    let created = tool(
        &h.app,
        &token,
        "routine_create",
        json!({ "name": "Uptime", "prompt": task,
                "cron": "0 9 1 1 *", "quiet": quiet }),
    )
    .await;
    assert!(!created.error, "{}", created.body);
    let saved = created.result["routine"].clone();
    assert_eq!(saved["quiet"], quiet);
    h.release(setup).await;
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;
    let latest = shown_bot(&h.app, bot).await["read_state"]["latest"]["position"].clone();
    let read = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/read"),
            Some(json!({ "action": "mark_read", "position": latest })),
        )
        .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);

    let due = DateTime::parse_from_rfc3339(saved["next_run_at"].as_str().unwrap())
        .unwrap()
        .to_utc();
    let queued =
        else_api::db::routines::admit_due(&h.app.db.pool, h.app.scheduler.registry(), None, due, 8)
            .await
            .unwrap()
            .queued;
    assert_eq!(queued, 1);
    h.app.scheduler.wake();
    let run: Uuid = sqlx::query_scalar("SELECT turn_id FROM routine_runs WHERE routine_id = ?1")
        .bind(saved["id"].as_str().unwrap().parse::<Uuid>().unwrap())
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    let (_, token) = h.started(run).await;
    (saved, run, token)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_run_with_no_news_alerts_no_one() {
    let h = recorder::harness(4).await;
    let main = h.app.main_bot().await;
    let (_, run, token) = start_run(&h, &main, true, UPTIME).await;
    let before = shown_bot(&h.app, &main).await;
    let packet = h.packets().await.into_iter().last().unwrap();
    assert!(
        packet.contains("The owner marked this routine quiet"),
        "{packet}"
    );

    let first = no_news(&h.app, &token).await;
    assert!(!first.error, "{}", first.body);
    assert_eq!(first.result["report"], "no_news");
    let again = no_news(&h.app, &token).await;
    assert_eq!(
        again.result["report"], "no_news",
        "repeating changes nothing"
    );
    h.release(run).await;
    h.app.wait_turn(&run.to_string(), &["completed"]).await;

    let after = shown_bot(&h.app, &main).await;
    assert_eq!(after["read_state"]["unread"], false, "{after}");
    assert_eq!(
        after["read_state"]["latest"],
        before["read_state"]["latest"]
    );
    assert_eq!(after["last_message_at"], before["last_message_at"]);
    assert_eq!(
        routine(&h.app, &main).await["last_run"]["status"],
        "no_news"
    );
    let turn = shown_turn(&h.app, &main, run).await;
    assert_eq!(turn["routine"]["no_news"], true, "{turn}");
    assert_eq!(turn["routine"]["quiet"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_run_that_asks_the_owner_still_reports() {
    let h = recorder::harness(4).await;
    let main = h.app.main_bot().await;
    let (_, run, token) = start_run(&h, &main, true, UPTIME).await;
    assert!(!no_news(&h.app, &token).await.error);
    let asked = tool(
        &h.app,
        &token,
        "ask_owner",
        json!({ "question": "Restart the server?", "choices": ["Yes", "No"] }),
    )
    .await;
    assert_eq!(asked.result["status"], "pending", "{}", asked.body);
    let question = asked.result["question_id"].as_str().unwrap().to_owned();
    h.release(run).await;
    h.app.wait_turn(&run.to_string(), &["waiting"]).await;
    assert_eq!(
        shown_bot(&h.app, &main).await["sidebar_status"]["kind"],
        "waiting_for_answer"
    );

    let answered = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/questions/{question}/answer"),
            Some(json!({ "choice": "b" })),
        )
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    h.app.wait_turn(&run.to_string(), &["completed"]).await;

    let after = shown_bot(&h.app, &main).await;
    assert_eq!(after["read_state"]["unread"], true, "{after}");
    assert_eq!(after["read_state"]["latest"]["turn_id"], run.to_string());
    assert_eq!(
        routine(&h.app, &main).await["last_run"]["status"],
        "completed"
    );
    assert_eq!(
        shown_turn(&h.app, &main, run).await["routine"]["no_news"],
        false
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_run_of_a_quiet_routine_can_end_with_no_news() {
    let h = recorder::harness(4).await;
    let main = h.app.main_bot().await;
    let (saved, run, token) = start_run(&h, &main, false, UPTIME).await;
    let refused = no_news(&h.app, &token).await;
    assert!(refused.error, "{}", refused.body);
    assert!(
        refused.result["error"]
            .as_str()
            .unwrap()
            .contains("not quiet"),
        "{}",
        refused.body
    );

    let marked = h
        .app
        .owner(
            "PATCH",
            &format!("/v1/bots/{main}/routines/{}", saved["id"].as_str().unwrap()),
            Some(json!({ "quiet": true })),
        )
        .await;
    assert_eq!(marked.status, StatusCode::OK, "{}", marked.body);
    assert_eq!(marked.body["routine"]["quiet"], true);
    assert!(!no_news(&h.app, &token).await.error);
    h.release(run).await;
    h.app.wait_turn(&run.to_string(), &["completed"]).await;
    assert_eq!(
        routine(&h.app, &main).await["last_run"]["status"],
        "no_news"
    );

    let (owner_turn, owner_token) = h.running(&main, GATE).await;
    let owner = no_news(&h.app, &owner_token).await;
    assert!(
        owner.result["error"]
            .as_str()
            .unwrap()
            .contains("only a scheduled routine run"),
        "{}",
        owner.body
    );
    h.release(owner_turn).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_run_that_fails_still_reports() {
    let h = recorder::harness(4).await;
    let main = h.app.main_bot().await;
    let task = format!("{GATE} [synthetic:fail] Check that example.com answers.");
    let (_, run, token) = start_run(&h, &main, true, &task).await;
    assert!(!no_news(&h.app, &token).await.error);
    h.release(run).await;
    h.app.wait_turn(&run.to_string(), &["failed"]).await;

    let after = shown_bot(&h.app, &main).await;
    assert_eq!(after["read_state"]["unread"], true, "{after}");
    assert_eq!(after["read_state"]["latest"]["turn_id"], run.to_string());
    assert_eq!(
        shown_turn(&h.app, &main, run).await["routine"]["no_news"],
        false
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_ran_an_allowed_action_cannot_end_with_no_news() {
    let h = recorder::harness(4).await;
    let main = h.app.main_bot().await;
    let task = format!("{GATE_EVERY_ATTEMPT} Delete merged branches on origin.");
    let (_, run, token) = start_run(&h, &main, true, &task).await;
    let delete = |branch: &str| {
        json!({ "command": format!("git push origin --delete {branch}"),
                "effect": "destructive", "purpose": "Delete a merged branch" })
    };
    let asked = tool(&h.app, &token, "computer_run", delete("feature-a")).await;
    assert_eq!(asked.result["status"], "pending", "{}", asked.body);
    let question = asked.result["question_id"].as_str().unwrap().to_owned();
    h.release(run).await;
    h.app.wait_turn(&run.to_string(), &["waiting"]).await;
    let answered = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/questions/{question}/answer"),
            Some(json!({ "choice": "approve_routine" })),
        )
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    let token = h.continued(run, &token).await;

    let ran = tool(&h.app, &token, "computer_run", delete("feature-b")).await;
    assert_ne!(
        ran.result["status"], "pending",
        "the grant runs it: {}",
        ran.body
    );
    let refused = no_news(&h.app, &token).await;
    assert!(refused.error, "{}", refused.body);
    assert!(
        refused.result["error"]
            .as_str()
            .unwrap()
            .contains("approved or allowed action ran"),
        "{}",
        refused.body
    );
    h.release(run).await;
    h.app.wait_turn(&run.to_string(), &["completed"]).await;
    assert_eq!(
        routine(&h.app, &main).await["last_run"]["status"],
        "completed"
    );
}
