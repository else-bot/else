//! Failed runs: retries, pausing, and the owner's retry.

use super::*;
use else_api::db::turns::{ItemKind, TurnStatus};

async fn wait_routine(app: &TestApp, bot: &str, done: impl Fn(&Value) -> bool) -> Value {
    let settled = common::poll(|| async {
        let listed = owner_routines(app, bot).await;
        done(&listed[0]).then(|| listed[0].clone())
    });
    match tokio::time::timeout(common::SETTLE, settled).await {
        Ok(routine) => routine,
        Err(_) => panic!(
            "routine never reached the state: {}",
            owner_routines(app, bot).await[0]
        ),
    }
}

async fn replies(app: &TestApp, turn: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT text FROM items WHERE turn_id = ?1 AND kind = 'assistant' ORDER BY seq",
    )
    .bind(turn)
    .fetch_all(&app.db.pool)
    .await
    .unwrap()
}

async fn sidebar(app: &TestApp, bot: &str) -> Value {
    app.owner("GET", &format!("/v1/bots/{bot}"), None)
        .await
        .body["bot"]["sidebar_status"]
        .clone()
}

async fn failing_routine(h: &Harness, bot: &str, prompt: &str) -> (String, DateTime<Utc>) {
    let (setup, token) = h.running(bot, GATE).await;
    let routine = create(
        &h.app,
        &token,
        json!({ "name": "Tickets", "prompt": prompt, "cron": "0 * * * *", "active_hours": "always" }),
    )
    .await;
    h.release(setup).await;
    h.app.wait_turn(setup, &["completed"]).await;
    (
        routine["id"].as_str().unwrap().to_owned(),
        at(&routine["next_run_at"]),
    )
}

async fn set_failed_runs(app: &TestApp, routine: &str, failed_runs: i64) {
    sqlx::query("UPDATE routines SET failed_runs = ?2 WHERE id = ?1")
        .bind(routine.parse::<Uuid>().unwrap())
        .bind(failed_runs)
        .execute(&app.db.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_transient_failure_is_retried_once_then_reported_until_dismissed() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (id, due) = failing_routine(&h, &main, "[synthetic:fail] update the tickets").await;
    fire(&h.app, due).await;
    let routine = wait_routine(&h.app, &main, |r| r["failed_runs"] == 1).await;
    assert_eq!(routine["enabled"], true);
    assert_eq!(routine["paused_reason"], Value::Null);
    assert_eq!(
        at(&routine["next_run_at"]),
        due + chrono::Duration::hours(1)
    );
    let all = runs(&h.app, &id).await;
    assert_eq!(all.len(), 2, "one automatic retry");
    let (first, retry) = (all[0].0.unwrap(), all[1].0.unwrap());
    let retry_of: Option<Uuid> =
        sqlx::query_scalar("SELECT retry_of FROM routine_runs WHERE turn_id = ?1")
            .bind(retry)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    let first_run: Uuid = sqlx::query_scalar("SELECT id FROM routine_runs WHERE turn_id = ?1")
        .bind(first)
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(retry_of, Some(first_run));
    assert_eq!(routine["last_run"]["turn_id"], retry.to_string());
    assert_eq!(routine["last_run"]["status"], "failed");
    assert_eq!(
        replies(&h.app, first).await,
        ["Partial output before failure. "],
        "the retried failure is not reported"
    );
    let notice = replies(&h.app, retry).await.pop().unwrap();
    assert!(
        notice.starts_with(
            "The routine **Tickets** failed because the AI provider reported an error. I retried \
             it right away and it failed again. I'll try again at the next scheduled run, "
        ),
        "{notice}"
    );
    assert!(notice.ends_with("Error: `provider_failed`: The synthetic provider failed."));

    assert_eq!(sidebar(&h.app, &main).await, json!({ "kind": "failed" }));
    let considered: i64 = sqlx::query_scalar(
        "SELECT max(considered_through_turn) FROM provider_sessions s
         JOIN turns t ON t.conversation_id = s.conversation_id WHERE t.id = ?1",
    )
    .bind(retry)
    .fetch_one(&h.app.db.pool)
    .await
    .unwrap();
    let retry_no: i64 = sqlx::query_scalar("SELECT turn_no FROM turns WHERE id = ?1")
        .bind(retry)
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert!(considered < retry_no, "warm sessions replay the notice");
    let (partial_seq, partial_bytes): (i64, i64) = sqlx::query_as(
        "SELECT seq, length(CAST(text AS BLOB)) FROM items
         WHERE turn_id = ?1 AND kind = 'assistant' ORDER BY seq LIMIT 1",
    )
    .bind(retry)
    .fetch_one(&h.app.db.pool)
    .await
    .unwrap();
    h.app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/read"),
            Some(json!({ "action": "through",
                         "position": { "item_seq": partial_seq, "text_bytes": partial_bytes } })),
        )
        .await;
    assert_eq!(
        sidebar(&h.app, &main).await,
        json!({ "kind": "failed" }),
        "reading partial output leaves the notice unseen"
    );
    let state = h
        .app
        .owner("GET", &format!("/v1/bots/{main}"), None)
        .await
        .body["bot"]["read_state"]
        .clone();
    assert_eq!(state["unread"], true, "the notice is a new reply");
    let read = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/read"),
            Some(json!({ "action": "through", "position": state["latest"]["position"] })),
        )
        .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    assert_eq!(
        sidebar(&h.app, &main).await,
        Value::Null,
        "read failures clear"
    );

    let dismissed = h
        .app
        .owner(
            "PATCH",
            &format!("/v1/bots/{main}/routines/{id}"),
            Some(json!({ "dismiss_failures": true })),
        )
        .await;
    assert_eq!(dismissed.status, StatusCode::OK, "{}", dismissed.body);
    assert_eq!(dismissed.body["routine"]["failed_runs"], 0);
    assert_eq!(dismissed.body["routine"]["enabled"], true);
}

#[tokio::test]
async fn a_failure_covered_by_a_stop_is_reported_but_never_retried() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (id, due) = failing_routine(&h, &main, "[synthetic:fail] update the tickets").await;
    sqlx::query(
        "INSERT INTO request_receipts
             (request_id, operation, bot_id, payload_hash, state, boundary_turn_no)
         SELECT randomblob(16), 'cancel', bot_id, randomblob(32), 'done', last_turn_no + 1
         FROM conversations WHERE bot_id = ?1 AND kind = 'owner'",
    )
    .bind(main.parse::<Uuid>().unwrap())
    .execute(&h.app.db.pool)
    .await
    .unwrap();
    fire(&h.app, due).await;
    let routine = wait_routine(&h.app, &main, |r| r["failed_runs"] == 1).await;
    assert_eq!(routine["enabled"], true);
    let all = runs(&h.app, &id).await;
    assert_eq!(all.len(), 1, "the owner's Stop is not undone by a retry");
    let notice = replies(&h.app, all[0].0.unwrap()).await.pop().unwrap();
    assert!(
        notice.contains("I couldn't retry it right away."),
        "{notice}"
    );
}

#[tokio::test]
async fn a_routine_that_keeps_failing_is_paused_until_turned_back_on() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (id, due) = failing_routine(&h, &main, "[synthetic:fail] update the tickets").await;
    let limit = else_api::app::routine_failures::MAX_FAILED_RUNS;
    set_failed_runs(&h.app, &id, limit - 1).await;
    fire(&h.app, due).await;
    let routine = wait_routine(&h.app, &main, |r| r["paused_reason"] == "repeated").await;
    assert_eq!(routine["enabled"], false);
    assert_eq!(routine["next_run_at"], Value::Null);
    assert_eq!(routine["failed_runs"], limit);
    assert_eq!(runs(&h.app, &id).await.len(), 2, "retried before pausing");
    let retry = runs(&h.app, &id).await[1].0.unwrap();
    let notice = replies(&h.app, retry).await.pop().unwrap();
    assert!(
        notice.starts_with(&format!(
            "I paused the routine **Tickets** after {limit} failed runs in a row. The last one \
             failed because the AI provider reported an error."
        )),
        "{notice}"
    );
    let far = Utc::now() + chrono::Duration::days(3650);
    assert_eq!(fire(&h.app, far).await, 0, "paused routines never run");

    let resumed = set_enabled(&h.app, &main, &id, true).await;
    assert_eq!(resumed.status, StatusCode::OK, "{}", resumed.body);
    assert_eq!(resumed.body["routine"]["failed_runs"], 0);
    assert_eq!(resumed.body["routine"]["paused_reason"], Value::Null);
    assert!(at(&resumed.body["routine"]["next_run_at"]) > Utc::now());
}

#[tokio::test]
async fn a_failure_retrying_cannot_fix_pauses_at_once_and_says_why() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (id, due) = failing_routine(&h, &main, "[synthetic:policy] check the servers").await;
    fire(&h.app, due).await;
    let routine = wait_routine(&h.app, &main, |r| r["paused_reason"] == "needs_action").await;
    assert_eq!(routine["enabled"], false);
    assert_eq!(routine["failed_runs"], 1);
    assert_eq!(routine["last_run"]["status"], "failed");
    assert_eq!(routine["last_run"]["error"]["code"], "policy_violation");
    let all = runs(&h.app, &id).await;
    assert_eq!(all.len(), 1, "never retried");
    let notice = replies(&h.app, all[0].0.unwrap()).await;
    assert_eq!(notice.len(), 1);
    assert!(
        notice[0].starts_with(
            "I paused the routine **Tickets** because it tried to use a tool it is not allowed \
             to use. Retrying won't fix this"
        ),
        "{}",
        notice[0]
    );
    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await
        .body;
    assert!(
        snapshot["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["kind"] == "assistant" && i["text"] == notice[0].as_str()),
        "the notice is in the transcript"
    );
}

async fn set_prompt(app: &TestApp, routine: &str, prompt: &str) {
    sqlx::query("UPDATE routines SET prompt = ?2 WHERE id = ?1")
        .bind(routine.parse::<Uuid>().unwrap())
        .bind(prompt)
        .execute(&app.db.pool)
        .await
        .unwrap();
}

async fn paused_routine(h: &Harness, bot: &str) -> String {
    let (id, due) = failing_routine(h, bot, "[synthetic:policy] check the servers").await;
    fire(&h.app, due).await;
    wait_routine(&h.app, bot, |r| r["paused_reason"] == "needs_action").await;
    id
}

#[tokio::test]
async fn the_owner_retries_a_paused_routine_and_a_success_switches_it_back_on() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let id = paused_routine(&h, &main).await;
    set_prompt(&h.app, &id, "Check the servers.").await;

    let request = Uuid::new_v4();
    let queued = retry(&h.app, &main, &id, request).await;
    assert_eq!(queued.status, StatusCode::OK, "{}", queued.body);
    assert_eq!(queued.body["queued"], true);
    assert_eq!(
        queued.body["routine"]["enabled"], false,
        "paused while it runs"
    );
    assert_eq!(queued.body["routine"]["retrying"], true);
    let again = retry(&h.app, &main, &id, request).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(
        again.body["queued"], false,
        "a repeated request returns its run"
    );

    let routine = wait_routine(&h.app, &main, |r| r["enabled"] == true).await;
    assert_eq!(routine["last_run"]["status"], "completed");
    assert_eq!(routine["paused_reason"], Value::Null);
    assert_eq!(routine["failed_runs"], 0);
    assert_eq!(routine["retrying"], false);
    assert!(at(&routine["next_run_at"]) > Utc::now());
    assert_eq!(runs(&h.app, &id).await.len(), 2);

    let late = retry(&h.app, &main, &id, Uuid::new_v4()).await;
    assert_eq!(late.status, StatusCode::CONFLICT, "{}", late.body);
    assert_eq!(late.body["error"]["code"], "nothing_to_retry");
    assert_eq!(runs(&h.app, &id).await.len(), 2);
}

#[tokio::test]
async fn the_owner_retries_an_enabled_routine_whose_last_run_failed() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (id, due) = failing_routine(&h, &main, "[synthetic:fail] update the tickets").await;
    fire(&h.app, due).await;
    wait_routine(&h.app, &main, |r| r["failed_runs"] == 1).await;
    set_prompt(&h.app, &id, "Update the tickets.").await;

    let queued = retry(&h.app, &main, &id, Uuid::new_v4()).await;
    assert_eq!(queued.status, StatusCode::OK, "{}", queued.body);
    assert_eq!(queued.body["queued"], true);
    assert_eq!(queued.body["routine"]["enabled"], true);
    assert_eq!(queued.body["routine"]["retrying"], true);

    let routine = wait_routine(&h.app, &main, |r| r["failed_runs"] == 0).await;
    assert_eq!(routine["last_run"]["status"], "completed");
    assert_eq!(routine["enabled"], true);
    assert_eq!(routine["retrying"], false);
    assert_eq!(
        at(&routine["next_run_at"]),
        due + chrono::Duration::hours(1),
        "the schedule stays as it was"
    );
    assert_eq!(runs(&h.app, &id).await.len(), 3);

    let late = retry(&h.app, &main, &id, Uuid::new_v4()).await;
    assert_eq!(late.status, StatusCode::CONFLICT, "{}", late.body);
    assert_eq!(late.body["error"]["code"], "nothing_to_retry");
    assert_eq!(runs(&h.app, &id).await.len(), 3);
}

#[tokio::test]
async fn a_failed_retry_of_an_enabled_routine_counts_and_keeps_its_schedule() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (id, due) = failing_routine(&h, &main, "[synthetic:fail] update the tickets").await;
    fire(&h.app, due).await;
    wait_routine(&h.app, &main, |r| r["failed_runs"] == 1).await;

    let queued = retry(&h.app, &main, &id, Uuid::new_v4()).await;
    assert_eq!(queued.status, StatusCode::OK, "{}", queued.body);
    let routine = wait_routine(&h.app, &main, |r| r["failed_runs"] == 2).await;
    assert_eq!(routine["retrying"], false);
    assert_eq!(routine["enabled"], true);
    assert_eq!(routine["paused_reason"], Value::Null);
    assert_eq!(
        at(&routine["next_run_at"]),
        due + chrono::Duration::hours(1)
    );
    assert_eq!(runs(&h.app, &id).await.len(), 3, "no automatic retry");
}

#[tokio::test]
async fn a_scheduled_run_skipped_during_an_owner_retry_keeps_it_retrying() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (id, due) = failing_routine(&h, &main, "[synthetic:fail] update the tickets").await;
    fire(&h.app, due).await;
    wait_routine(&h.app, &main, |r| r["failed_runs"] == 1).await;
    set_prompt(&h.app, &id, "[synthetic:hang] update the tickets").await;

    let queued = retry(&h.app, &main, &id, Uuid::new_v4()).await;
    assert_eq!(queued.status, StatusCode::OK, "{}", queued.body);
    let run = runs(&h.app, &id).await[2].0.unwrap();
    h.app.wait_turn(run, &["running"]).await;
    fire(&h.app, due + chrono::Duration::hours(1)).await;
    let routine = wait_routine(&h.app, &main, |r| r["last_run"]["status"] == "skipped").await;
    assert_eq!(routine["last_run"]["skip_reason"], "overlap");
    assert_eq!(routine["retrying"], true);
}

#[tokio::test]
async fn a_second_retry_waits_for_the_first_and_a_stopped_retry_stays_paused() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let id = paused_routine(&h, &main).await;
    set_prompt(&h.app, &id, "[synthetic:hang] check the servers").await;

    let first = retry(&h.app, &main, &id, Uuid::new_v4()).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let second = retry(&h.app, &main, &id, Uuid::new_v4()).await;
    assert_eq!(second.status, StatusCode::CONFLICT, "{}", second.body);
    assert_eq!(second.body["error"]["code"], "already_running");
    assert_eq!(runs(&h.app, &id).await.len(), 2, "one retry run");

    let run = runs(&h.app, &id).await[1].0.unwrap();
    h.app.wait_turn(run, &["running"]).await;
    let stop = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    let routine = wait_routine(&h.app, &main, |r| r["retrying"] == false).await;
    assert_eq!(routine["enabled"], false);
    assert_eq!(routine["paused_reason"], "needs_action");
    assert_eq!(routine["last_run"]["status"], "interrupted");
    let (_, update) = events(&h.app, &main)
        .await
        .into_iter()
        .rfind(|(kind, _)| kind == "bot.updated")
        .unwrap();
    assert_eq!(
        update["routines"][0]["retrying"], false,
        "clients learn the retry ended"
    );
}

#[tokio::test]
async fn a_retry_without_a_provider_says_so_and_queues_nothing() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let id = paused_routine(&h, &main).await;
    sqlx::query("UPDATE routines SET model = ?2 WHERE id = ?1")
        .bind(id.parse::<Uuid>().unwrap())
        .bind(
            json!({ "provider_id": "claude", "model": "claude-opus-5-5",
                    "reasoning_effort": "high", "service_tier": "default" })
            .to_string(),
        )
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    let refused = retry(&h.app, &main, &id, Uuid::new_v4()).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], "provider_unavailable");
    assert_eq!(runs(&h.app, &id).await.len(), 1);
    assert_eq!(owner_routines(&h.app, &main).await[0]["retrying"], false);
}

#[tokio::test]
async fn a_failed_owner_retry_keeps_the_routine_paused_with_the_new_reason() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let id = paused_routine(&h, &main).await;
    set_prompt(&h.app, &id, "[synthetic:fail] check the servers").await;

    let queued = retry(&h.app, &main, &id, Uuid::new_v4()).await;
    assert_eq!(queued.status, StatusCode::OK, "{}", queued.body);
    let routine = wait_routine(&h.app, &main, |r| r["failed_runs"] == 2).await;
    assert_eq!(routine["enabled"], false);
    assert_eq!(routine["next_run_at"], Value::Null);
    assert_eq!(routine["paused_reason"], "repeated");
    assert_eq!(routine["retrying"], false);
    assert_eq!(routine["last_run"]["error"]["code"], "provider_failed");
    let all = runs(&h.app, &id).await;
    assert_eq!(
        all.len(),
        2,
        "an owner retry is never retried automatically"
    );
    let notice = replies(&h.app, all[1].0.unwrap()).await.pop().unwrap();
    assert!(
        notice.starts_with(
            "Your retry of the routine **Tickets** failed because the AI provider reported an \
             error, so it stays paused."
        ),
        "{notice}"
    );

    let dismissed = h
        .app
        .owner(
            "PATCH",
            &format!("/v1/bots/{main}/routines/{id}"),
            Some(json!({ "dismiss_failures": true })),
        )
        .await;
    assert_eq!(dismissed.status, StatusCode::OK, "{}", dismissed.body);
    assert_eq!(dismissed.body["routine"]["paused_reason"], Value::Null);
    assert_eq!(dismissed.body["routine"]["enabled"], false);
}

#[tokio::test]
async fn a_successful_run_forgets_earlier_failures() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (id, due) = failing_routine(&h, &main, "Collect the build results.").await;
    set_failed_runs(&h.app, &id, 2).await;
    fire(&h.app, due).await;
    let routine = wait_routine(&h.app, &main, |r| r["failed_runs"] == 0).await;
    assert_eq!(routine["last_run"]["status"], "completed");
    assert_eq!(routine["enabled"], true);
    assert_eq!(runs(&h.app, &id).await.len(), 1);
}

/// Seed a run of a new routine `name` due at `due_at` in `bot`'s owner
/// conversation, not reviewed yet. Its turn failed with `failure`, or
/// completed without one. Returns the run's turn.
async fn unreviewed_run(
    conn: &mut sqlx::SqliteConnection,
    bot: Uuid,
    name: &str,
    due_at: &str,
    failure: Option<&str>,
) -> Uuid {
    let status = match failure {
        Some(_) => TurnStatus::Failed,
        None => TurnStatus::Completed,
    };
    let turn = common::seed::turn(bot)
        .message(ItemKind::Routine, "Update the tickets.")
        .status(status)
        .insert(&mut *conn)
        .await;
    if let Some(code) = failure {
        sqlx::query("UPDATE turns SET error_code = ?2, error_message = 'It failed.' WHERE id = ?1")
            .bind(turn)
            .bind(code)
            .execute(&mut *conn)
            .await
            .unwrap();
    }
    let routine = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO routines (id, bot_id, conversation_id, name, prompt, cron, timezone,
             created_turn_id, created_name, request_hash)
         SELECT ?1, bot_id, conversation_id, ?3, 'Update the tickets.', '0 * * * *', 'UTC',
             id, ?3, zeroblob(32) FROM turns WHERE id = ?2",
    )
    .bind(routine)
    .bind(turn)
    .bind(name)
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO routine_runs (id, routine_id, due_at, name, turn_id)
         VALUES (randomblob(16), ?1, ?2, ?3, ?4)",
    )
    .bind(routine)
    .bind(due_at)
    .bind(name)
    .bind(turn)
    .execute(&mut *conn)
    .await
    .unwrap();
    turn
}

const DUE: &str = "2026-01-01T10:00:00.000+00:00";

#[tokio::test]
async fn the_next_turn_waits_for_the_failure_notice_of_an_ended_run() {
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let pool = &db.pool;
    let bot = common::seed::bot(pool, "Scout").await;
    unreviewed_run(
        &mut pool.acquire().await.unwrap(),
        bot.id,
        "Tickets",
        DUE,
        Some("policy_violation"),
    )
    .await;
    let next = common::seed::turn(bot.id)
        .message(ItemKind::User, "Any news on the tickets?")
        .insert(pool)
        .await;
    let claim = || async {
        else_api::db::turns::claim_next(pool, &[], true)
            .await
            .unwrap()
            .map(|claimed| claimed.id)
    };

    assert_eq!(claim().await, None, "the run ended but is not reviewed");
    assert!(
        !else_api::db::turns::lifecycle::claim(&mut pool.acquire().await.unwrap(), next, bot.id)
            .await
            .unwrap(),
        "the claim itself refuses it"
    );
    else_api::db::routines::review_ended(
        pool,
        &else_api::providers::Registry::new(vec![]),
        Utc::now(),
    )
    .await
    .unwrap();
    assert_eq!(claim().await, Some(next), "the review lets it start");
    let input = else_api::db::turns::input(pool, next).await.unwrap();
    assert!(
        input.history.iter().any(|entry| entry.text.starts_with(
            "I paused the routine **Tickets** because it tried to use a tool it is not allowed \
             to use."
        )),
        "the next turn reads the notice: {:?}",
        input.history
    );
}

#[tokio::test]
async fn a_routine_retry_takes_the_last_queue_slot_before_a_watcher_wake() {
    // No run slots, so queued turns stay queued and the queue stays full.
    let app = TestApp::builder().max_active(0).start().await;
    let bot: Uuid = app.main_bot().await.parse().unwrap();
    // Seed between two pump passes, so both become ready in the same tick.
    let closed = app
        .state
        .gate
        .close(common::SETTLE, else_api::maintenance::Pause::backup(None))
        .await
        .unwrap()
        .unwrap();
    let mut tx = app.db.pool.begin().await.unwrap();
    // More ended runs than one review query reads come first.
    for n in 0..9 {
        let due = format!("2026-01-01T09:00:0{n}.000+00:00");
        unreviewed_run(&mut tx, bot, &format!("Digest {n}"), &due, None).await;
    }
    let run = unreviewed_run(&mut tx, bot, "Tickets", DUE, Some("provider_failed")).await;
    for _ in 0..3 {
        common::seed::turn(bot)
            .message(ItemKind::User, "Queued.")
            .insert(&mut *tx)
            .await;
    }
    let watcher = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO watchers (id, bot_id, conversation_id, created_turn_id, purpose,
             check_command, prompt, every_minutes, watch_until, status, outcome, ended_at)
         SELECT ?1, bot_id, conversation_id, id, 'Build', 'true', 'Report the build.', 5,
             '2100-01-01T00:00:00.000+00:00', 'waking', 'met',
             strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         FROM turns WHERE id = ?2",
    )
    .bind(watcher)
    .bind(run)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(closed);
    app.scheduler.wake();

    let settled = common::poll(|| async {
        sqlx::query_as::<_, (bool, String, bool)>(
            "SELECT rr.reviewed, w.status, w.wake_after IS NOT NULL
             FROM routine_runs rr, watchers w WHERE rr.turn_id = ?1 AND w.id = ?2",
        )
        .bind(run)
        .bind(watcher)
        .fetch_one(&app.db.pool)
        .await
        .ok()
        .filter(|(reviewed, status, refused)| *reviewed && (status == "ended" || *refused))
    });
    let (_, status, _) = tokio::time::timeout(common::SETTLE, settled)
        .await
        .expect("the run and the wake settle");
    let retried: Option<Uuid> = sqlx::query_scalar(
        "SELECT r.turn_id FROM routine_runs r JOIN routine_runs o ON o.id = r.retry_of
         WHERE o.turn_id = ?1",
    )
    .bind(run)
    .fetch_optional(&app.db.pool)
    .await
    .unwrap();
    assert!(retried.is_some(), "the retry took the last slot");
    assert_eq!(status, "waking", "the wake waits for a free slot");
}
