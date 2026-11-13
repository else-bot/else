use crate::common;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use common::guest::Guest;
use common::recorder::tool;
use common::{TestApp, TestDb};
use else_api::computers::{ComputerConfig, ComputerManager};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::synthetic::Synthetic;
use serde_json::{Value, json};
use uuid::Uuid;

const SLOW: &str = "[synthetic:slow] Watch for new leads every morning.";
const PROMPT: &str = "Qualify each new lead and report the good ones.";
const YEARLY: &str = "0 9 1 1 *";

struct Setup {
    app: TestApp,
    guest: Arc<Guest>,
    manager: Arc<ComputerManager>,
    bot: String,
    routine: Uuid,
}

async fn setup(check: &str) -> Setup {
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
    let registry = else_api::providers::Registry::new(vec![synthetic]);
    let app = TestApp::with_parts(db, 4, registry, Some(manager.clone()));
    let bot = app.create_bot("Scout").await;
    let token = owner_turn(&app, &bot).await;
    let created = tool(
        &app,
        &token,
        "routine_create",
        json!({ "name": "Leads", "prompt": PROMPT, "check": check, "cron": YEARLY,
                "timezone": "UTC", "active_hours": "always" }),
    )
    .await;
    assert!(!created.error, "{}", created.body);
    assert_eq!(created.result["routine"]["check"], check);
    let routine = created.result["routine"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    Setup {
        app,
        guest,
        manager,
        bot,
        routine,
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
    app.scheduler
        .grants()
        .issue(
            bot.parse().unwrap(),
            conversation,
            turn,
            vec![Tool::Routines, Tool::Computer],
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

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
struct Run {
    id: Uuid,
    turn_id: Option<Uuid>,
    skip_reason: Option<String>,
    check_job_id: Option<Uuid>,
}

async fn runs(app: &TestApp, routine: Uuid) -> Vec<Run> {
    sqlx::query_as(
        "SELECT id, turn_id, skip_reason, check_job_id FROM routine_runs
         WHERE routine_id = ?1 ORDER BY due_at",
    )
    .bind(routine)
    .fetch_all(&app.db.pool)
    .await
    .unwrap()
}

async fn make_due(app: &TestApp, routine: Uuid) {
    let due = (Utc::now() - chrono::Duration::seconds(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, false);
    sqlx::query("UPDATE routines SET next_due_at = ?2 WHERE id = ?1")
        .bind(routine)
        .bind(due)
        .execute(&app.db.pool)
        .await
        .unwrap();
    app.scheduler.wake();
}

async fn checking(s: &Setup) -> (Uuid, Uuid) {
    make_due(&s.app, s.routine).await;
    let run = until("admitted the run", || async {
        runs(&s.app, s.routine).await.into_iter().last()
    })
    .await;
    assert_eq!((run.turn_id, run.skip_reason.as_deref()), (None, None));
    let job = run.check_job_id.expect("the run has its check job");
    until("launched the check", || async {
        let status: String = sqlx::query_scalar("SELECT status FROM guest_jobs WHERE id = ?1")
            .bind(job)
            .fetch_one(&s.app.db.pool)
            .await
            .unwrap();
        (status == "running").then_some(())
    })
    .await;
    assert_eq!(last_run(&s.app, &s.bot).await["status"], "checking");
    (run.id, job)
}

async fn check_ends(s: &Setup, job: Uuid, probe: &str) {
    s.guest.report(probe);
    s.manager.job(s.bot.parse().unwrap(), job).await.unwrap();
    s.app.scheduler.wake();
}

async fn settled(app: &TestApp, run: Uuid) -> Run {
    until("settled the run", || async {
        let row: Run = sqlx::query_as(
            "SELECT id, turn_id, skip_reason, check_job_id FROM routine_runs WHERE id = ?1",
        )
        .bind(run)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
        (row.turn_id.is_some() || row.skip_reason.is_some()).then_some(row)
    })
    .await
}

async fn last_run(app: &TestApp, bot: &str) -> Value {
    let reply = app
        .owner("GET", &format!("/v1/bots/{bot}/routines"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.body["routines"][0]["last_run"].clone()
}

async fn task_text(app: &TestApp, turn: Uuid) -> String {
    sqlx::query_scalar("SELECT text FROM items WHERE turn_id = ?1 AND kind = 'routine'")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn bot_turns(app: &TestApp, bot: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM turns WHERE bot_id = ?1")
        .bind(bot.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_check_that_prints_nothing_skips_the_run_without_waking_the_model() {
    let s = setup("./new-leads.sh").await;
    let (run, job) = checking(&s).await;
    let command: String = sqlx::query_scalar("SELECT command FROM guest_jobs WHERE id = ?1")
        .bind(job)
        .fetch_one(&s.app.db.pool)
        .await
        .unwrap();
    assert_eq!(command, "./new-leads.sh");

    check_ends(&s, job, "done 0\n").await;
    let run = settled(&s.app, run).await;
    assert_eq!(run.skip_reason.as_deref(), Some("nothing_new"));
    assert_eq!(run.turn_id, None, "no turn, so no provider call");
    assert_eq!(bot_turns(&s.app, &s.bot).await, 1, "only the owner's turn");
    let shown = last_run(&s.app, &s.bot).await;
    assert_eq!(
        (shown["status"].as_str(), shown["skip_reason"].as_str()),
        (Some("skipped"), Some("nothing_new"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_check_with_output_starts_the_turn_with_it() {
    let s = setup("./new-leads.sh").await;
    let (run, job) = checking(&s).await;

    check_ends(&s, job, "done 0\n2 new leads: ACME, Globex\n").await;
    let run = settled(&s.app, run).await;
    assert_eq!(run.skip_reason, None);
    let turn = run.turn_id.expect("the check's output wakes the model");
    let text = task_text(&s.app, turn).await;
    assert!(text.starts_with(PROMPT), "{text}");
    assert!(text.contains("2 new leads: ACME, Globex"), "{text}");
    let revision: Option<i64> =
        sqlx::query_scalar("SELECT prompt_revision FROM routine_runs WHERE id = ?1")
            .bind(run.id)
            .fetch_one(&s.app.db.pool)
            .await
            .unwrap();
    assert_eq!(revision, Some(1), "the turn runs the current prompt");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_check_wakes_the_run_with_its_failure_never_nothing_new() {
    let s = setup("./new-leads.sh").await;
    let (run, job) = checking(&s).await;

    check_ends(&s, job, "done 6\ncurl: (6) Could not resolve host\n").await;
    let run = settled(&s.app, run).await;
    let turn = run.turn_id.expect("a failed check wakes the model");
    let text = task_text(&s.app, turn).await;
    assert!(text.contains("failed with exit code 6"), "{text}");
    assert!(text.contains("Could not resolve host"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_check_wakes_the_run_and_a_due_occurrence_meanwhile_overlaps() {
    let s = setup("./new-leads.sh").await;
    let (run, job) = checking(&s).await;

    make_due(&s.app, s.routine).await;
    let overlap = until("skipped the next occurrence", || async {
        let all = runs(&s.app, s.routine).await;
        (all.len() == 2).then(|| all[1].clone())
    })
    .await;
    assert_eq!(
        (overlap.skip_reason.as_deref(), overlap.check_job_id),
        (Some("overlap"), None),
        "a second check never starts while one runs"
    );

    s.manager
        .cancel_job(s.bot.parse().unwrap(), job)
        .await
        .unwrap();
    s.app.scheduler.wake();
    let run = settled(&s.app, run).await;
    let turn = run
        .turn_id
        .expect("a check cancelled outside Stop wakes the run");
    let text = task_text(&s.app, turn).await;
    assert!(text.contains("stopped before it finished"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_computers_a_check_is_skipped_as_unavailable_and_update_can_remove_it() {
    let s = setup("./new-leads.sh").await;
    let due: DateTime<Utc> = sqlx::query_scalar("SELECT next_due_at FROM routines WHERE id = ?1")
        .bind(s.routine)
        .fetch_one(&s.app.db.pool)
        .await
        .unwrap();
    let admitted =
        else_api::db::routines::admit_due(&s.app.db.pool, s.app.scheduler.registry(), None, due, 8)
            .await
            .unwrap();
    assert_eq!((admitted.queued, admitted.checks.len()), (0, 0));
    let all = runs(&s.app, s.routine).await;
    assert_eq!(
        (all[0].skip_reason.as_deref(), all[0].turn_id),
        (Some("check_unavailable"), None)
    );

    let token = owner_turn(&s.app, &s.bot).await;
    let updated = tool(
        &s.app,
        &token,
        "routine_update",
        json!({ "routine_id": s.routine, "check": null }),
    )
    .await;
    assert!(!updated.error, "{}", updated.body);
    assert_eq!(updated.result["routine"]["check"], Value::Null);
    assert_eq!(
        updated.result["routine"]["prompt_revision"], 2,
        "a new check is a new revision, so old grants end"
    );
    let refused = tool(
        &s.app,
        &token,
        "routine_update",
        json!({ "routine_id": s.routine, "check": "   " }),
    )
    .await;
    assert!(refused.error, "{}", refused.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_check_whose_exit_status_cannot_be_read_wakes_the_run_as_unknown() {
    let s = setup("./new-leads.sh").await;
    let (run, job) = checking(&s).await;

    check_ends(&s, job, "done \nnew lead\n").await;
    let run = settled(&s.app, run).await;
    let turn = run
        .turn_id
        .expect("a check that ran with an unread exit status is unknown, not unstarted");
    let text = task_text(&s.app, turn).await;
    assert!(text.contains("unknown outcome"), "{text}");
    assert!(text.contains("new lead"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_work_settles_a_checking_run_before_its_check_can_report() {
    let s = setup("./new-leads.sh").await;
    let (run, job) = checking(&s).await;

    let stopped = s
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{}/cancel", s.bot),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stopped.status.is_success(), "{}", stopped.body);
    let row = runs(&s.app, s.routine).await.remove(0);
    assert_eq!(
        (row.id, row.skip_reason.as_deref(), row.turn_id),
        (run, Some("check_cancelled"), None),
        "Stop skips the run in its own transaction"
    );

    check_ends(&s, job, "done 0\n2 new leads\n").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let row = runs(&s.app, s.routine).await.remove(0);
    assert_eq!(
        (row.skip_reason.as_deref(), row.turn_id),
        (Some("check_cancelled"), None),
        "a check that reports after Stop never queues its turn"
    );
}
