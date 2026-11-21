//! Failed runs: retries, pausing, and the owner's retry.

use super::*;

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
