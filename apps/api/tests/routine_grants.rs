mod common;

use std::time::Duration;

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use common::TestApp;
use common::recorder::{self, GATE, GATE_EVERY_ATTEMPT, tool};
use else_api::app::questions::Answer;
use else_api::app::routines::{RoutineName, RoutinePrompt};
use else_api::db::routines::{self, By, Patch};
use serde_json::{Value, json};
use uuid::Uuid;

const YEARLY: &str = "0 9 1 1 *";

fn delete_args(path: &str) -> Value {
    json!({ "command": format!("git push origin --delete {path}"), "effect": "destructive",
            "purpose": "Delete a merged branch" })
}

fn send_args() -> Value {
    json!({ "command": "node send.js --all", "effect": "external",
            "purpose": "Send the build report", "context": "To: team list" })
}

async fn create(
    app: &TestApp,
    token: &str,
    name: &str,
    prompt: &str,
    cron: &str,
) -> (Uuid, DateTime<Utc>) {
    let called = tool(
        app,
        token,
        "routine_create",
        json!({ "name": name, "prompt": prompt, "cron": cron }),
    )
    .await;
    assert!(!called.error, "{}", called.body);
    let routine = &called.result["routine"];
    (
        routine["id"].as_str().unwrap().parse().unwrap(),
        DateTime::parse_from_rfc3339(routine["next_run_at"].as_str().unwrap())
            .unwrap()
            .to_utc(),
    )
}

async fn fire(app: &TestApp, now: DateTime<Utc>) -> Vec<(Uuid, Uuid)> {
    for _ in 0..400 {
        let pending: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM routine_runs rr JOIN turns t ON t.id = rr.turn_id
                            WHERE NOT rr.reviewed
                              AND t.status IN ('completed', 'interrupted', 'failed', 'uncertain'))",
        )
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
        if !pending {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    routines::admit_due(&app.db.pool, app.scheduler.registry(), now, 8)
        .await
        .unwrap();
    app.scheduler.wake();
    sqlx::query_as(
        "SELECT routine_id, turn_id FROM routine_runs WHERE due_at = ?1 AND turn_id IS NOT NULL",
    )
    .bind(now.to_rfc3339_opts(chrono::SecondsFormat::Millis, false))
    .fetch_all(&app.db.pool)
    .await
    .unwrap()
}

fn run_of(runs: &[(Uuid, Uuid)], routine: Uuid) -> Uuid {
    runs.iter().find(|(r, _)| *r == routine).unwrap().1
}

async fn next_run(app: &TestApp, routine: Uuid) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT next_due_at FROM routines WHERE id = ?1")
        .bind(routine)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn wait(app: &TestApp, turn: Uuid, status: &str) {
    app.wait_turn_for(&turn.to_string(), &[status], Duration::from_secs(30))
        .await;
}

async fn asks(app: &TestApp, token: &str, args: Value) -> (Uuid, Vec<String>) {
    let called = tool(app, token, "computer_run", args).await;
    assert_eq!(called.result["status"], "pending", "{}", called.body);
    let question: Uuid = called.result["question_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let choices: String = sqlx::query_scalar("SELECT choices FROM owner_questions WHERE id = ?1")
        .bind(question)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let choices: Vec<Value> = serde_json::from_str(&choices).unwrap();
    let ids = choices
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect();
    (question, ids)
}

async fn runs_without_asking(app: &TestApp, token: &str, args: Value) {
    let called = tool(app, token, "computer_run", args).await;
    assert!(
        called
            .result
            .as_str()
            .is_some_and(|t| t.starts_with("COMPUTER_UNAVAILABLE")),
        "{}",
        called.body
    );
}

async fn answer(app: &TestApp, bot: &str, question: Uuid, choice: &str) {
    let reply = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/questions/{question}/answer"),
            Some(json!({ "choice": choice })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

async fn routine_grants(app: &TestApp) -> Vec<(Uuid, i64)> {
    sqlx::query_as("SELECT routine_id, prompt_revision FROM routine_grants ORDER BY routine_id")
        .fetch_all(&app.db.pool)
        .await
        .unwrap()
}

async fn set_prompt(app: &TestApp, bot: &str, routine: Uuid, prompt: &str) {
    let patch = Patch {
        prompt: Some(RoutinePrompt::parse(prompt).unwrap()),
        ..Patch::default()
    };
    routines::update(
        &app.db.pool,
        By::Owner {
            bot_id: bot.parse().unwrap(),
        },
        routine,
        patch,
        Utc::now(),
    )
    .await
    .unwrap();
}

const APPROVE_ROUTINE: [&str; 3] = ["approve", "deny", "approve_routine"];
const APPROVE_FOREVER: [&str; 3] = ["approve", "deny", "approve_forever"];

#[tokio::test(flavor = "multi_thread")]
async fn chat_can_preapprove_a_routine_before_its_first_run() {
    let h = recorder::harness(4).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let authorization = "You can already mark this routine as always approved.";
    let (setup, token) = h.running(&main, &format!("{GATE} {authorization}")).await;
    let (prune, due) = create(
        app,
        &token,
        "Prune branches",
        &format!("{GATE} Delete merged branches."),
        YEARLY,
    )
    .await;
    let args = json!({ "target": { "scope": "routine", "routine_id": prune, "prompt_revision": 1 }, "authorization": authorization });
    let approved = tool(app, &token, "confirm_permission", args.clone()).await;
    assert_eq!(approved.result["status"], "approved", "{}", approved.body);
    let retry = tool(app, &token, "confirm_permission", args).await;
    assert_eq!(retry.result["question_id"], approved.result["question_id"]);
    assert_eq!(routine_grants(app).await, [(prune, 1)]);
    let shown = else_api::db::turns::load_turn(&mut app.db.pool.acquire().await.unwrap(), setup)
        .await
        .unwrap();
    assert_eq!(shown.questions.len(), 1);
    assert_eq!(
        shown.questions[0].status,
        else_api::app::questions::Status::Answered
    );
    assert_eq!(shown.questions[0].context.as_deref(), Some(authorization));
    let event: String = sqlx::query_scalar("SELECT data FROM bot_events WHERE bot_id = ?1 AND kind = 'turn.updated' ORDER BY seq DESC LIMIT 1")
        .bind(main.parse::<Uuid>().unwrap()).fetch_one(&app.db.pool).await.unwrap();
    let event: Value = serde_json::from_str(&event).unwrap();
    assert_eq!(
        event["turn"]["questions"][0]["status"], "answered",
        "the live update must contain the settled approval"
    );
    assert!(
        else_api::db::owner_questions::permissions(&app.db.pool, main.parse().unwrap())
            .await
            .unwrap()
            .is_empty()
    );
    h.release(setup).await;
    wait(app, setup, "completed").await;

    let run = run_of(&fire(app, due).await, prune);
    let (_, token) = h.started(run).await;
    runs_without_asking(app, &token, delete_args("feature-a")).await;
    let (_, choices) = asks(app, &token, send_args()).await;
    assert_eq!(choices, ["approve", "deny"]);
    h.release(run).await;
    wait(app, run, "waiting").await;

    let (manual, token) = h
        .running(&main, &format!("{GATE} delete a branch for me"))
        .await;
    let (question, choices) = asks(app, &token, delete_args("feature-b")).await;
    assert_eq!(
        choices, APPROVE_FOREVER,
        "routine approval does not cover owner requests"
    );
    h.release(manual).await;
    wait(app, manual, "waiting").await;
    answer(app, &main, question, "deny").await;
    wait(app, manual, "completed").await;

    set_prompt(app, &main, prune, &format!("{GATE} Delete all branches.")).await;
    assert!(
        else_api::db::owner_questions::routine_grants(&app.db.pool, main.parse().unwrap())
            .await
            .unwrap()
            .is_empty()
    );
    let run = run_of(&fire(app, next_run(app, prune).await).await, prune);
    let (_, token) = h.started(run).await;
    let (_, choices) = asks(app, &token, delete_args("feature-c")).await;
    assert_eq!(
        choices, APPROVE_ROUTINE,
        "a changed task needs approval again"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_bot_approval_is_idempotent_and_a_retry_cannot_restore_it_after_revocation() {
    let h = recorder::harness(4).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let authorization = "Always approve destructive actions for this bot.";
    let (turn, token) = h.running(&main, &format!("{GATE} {authorization}")).await;
    let args = json!({ "target": { "scope": "bot" }, "authorization": authorization });
    let (first, retry) = tokio::join!(
        tool(app, &token, "confirm_permission", args.clone()),
        tool(app, &token, "confirm_permission", args.clone()),
    );
    assert_eq!(first.result["status"], "approved", "{}", first.body);
    assert_eq!(retry.result["status"], "approved", "{}", retry.body);
    assert_eq!(first.result["question_id"], retry.result["question_id"]);
    runs_without_asking(app, &token, delete_args("feature-a")).await;
    let removed = app
        .owner(
            "DELETE",
            &format!("/v1/bots/{main}/permissions/destructive"),
            None,
        )
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    let retry = tool(app, &token, "confirm_permission", args).await;
    assert_eq!(retry.result["status"], "revoked", "{}", retry.body);
    assert_eq!(retry.result["question_id"], first.result["question_id"]);
    asks(app, &token, delete_args("feature-b")).await;
    h.release(turn).await;
    wait(app, turn, "waiting").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_approval_requires_the_current_owner_message_and_current_owned_routine() {
    let h = recorder::harness(4).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let authorization = "Always allow this routine's destructive actions.";
    let (setup, token) = h.running(&main, &format!("{GATE} {authorization}")).await;
    let (prune, due) = create(
        app,
        &token,
        "Prune branches",
        &format!("{GATE} Delete merged branches."),
        YEARLY,
    )
    .await;
    let forged = tool(
        app,
        &token,
        "confirm_permission",
        json!({ "target": { "scope": "bot" }, "authorization": "Always approve everything" }),
    )
    .await;
    assert_eq!(forged.code(), "NOT_AUTHORIZED", "{}", forged.body);
    for target in [
        json!({ "scope": "routine", "routine_id": Uuid::new_v4(), "prompt_revision": 1 }),
        json!({ "scope": "routine", "routine_id": prune, "prompt_revision": 2 }),
    ] {
        let refused = tool(
            app,
            &token,
            "confirm_permission",
            json!({ "target": target, "authorization": authorization }),
        )
        .await;
        assert_eq!(refused.code(), "ROUTINE_CHANGED", "{}", refused.body);
    }
    let other = app.create_bot("Another bot").await;
    let (other_turn, other_token) = h.running(&other, &format!("{GATE} {authorization}")).await;
    let args = json!({ "target": { "scope": "routine", "routine_id": prune, "prompt_revision": 1 }, "authorization": authorization });
    let refused = tool(app, &other_token, "confirm_permission", args.clone()).await;
    assert_eq!(refused.code(), "ROUTINE_CHANGED", "{}", refused.body);
    h.release(other_turn).await;
    wait(app, other_turn, "completed").await;
    h.release(setup).await;
    wait(app, setup, "completed").await;

    let run = run_of(&fire(app, due).await, prune);
    let (_, token) = h.started(run).await;
    let refused = tool(
        app,
        &token,
        "confirm_permission",
        json!({ "target": { "scope": "bot" }, "authorization": "Delete merged branches." }),
    )
    .await;
    assert_eq!(refused.code(), "NOT_AUTHORIZED", "{}", refused.body);
    h.release(run).await;
    wait(app, run, "completed").await;

    let (later, token) = h.running(&main, &format!("{GATE} continue")).await;
    let refused = tool(app, &token, "confirm_permission", args).await;
    assert_eq!(
        refused.code(),
        "NOT_AUTHORIZED",
        "old owner messages grant nothing"
    );
    h.release(later).await;
    wait(app, later, "completed").await;
    assert!(routine_grants(app).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_authorization_cannot_be_reused_after_the_routine_task_changes() {
    let h = recorder::harness(4).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let authorization = "Always approve this routine's destructive actions.";
    let (turn, token) = h.running(&main, &format!("{GATE} {authorization}")).await;
    let (prune, _) = create(
        app,
        &token,
        "Prune branches",
        "Delete merged branches.",
        YEARLY,
    )
    .await;
    let target = |revision| json!({ "target": { "scope": "routine", "routine_id": prune, "prompt_revision": revision }, "authorization": authorization });
    let approved = tool(app, &token, "confirm_permission", target(1)).await;
    assert_eq!(approved.result["status"], "approved", "{}", approved.body);
    let removed = app
        .owner(
            "DELETE",
            &format!("/v1/bots/{main}/permissions/routines/{prune}"),
            None,
        )
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    let retry = tool(app, &token, "confirm_permission", target(1)).await;
    assert_eq!(retry.result["status"], "revoked");
    set_prompt(app, &main, prune, "Delete all branches.").await;
    let stale = tool(app, &token, "confirm_permission", target(1)).await;
    assert_eq!(stale.code(), "ROUTINE_CHANGED");
    let reused = tool(app, &token, "confirm_permission", target(2)).await;
    assert_eq!(reused.code(), "AUTHORIZATION_USED", "{}", reused.body);
    assert!(routine_grants(app).await.is_empty());
    routines::delete(
        &app.db.pool,
        By::Owner {
            bot_id: main.parse().unwrap(),
        },
        prune,
    )
    .await
    .unwrap();
    let deleted = tool(app, &token, "confirm_permission", target(2)).await;
    assert_eq!(deleted.code(), "ROUTINE_CHANGED");
    tool(
        app,
        &token,
        "ask_owner",
        json!({ "question": "Which branch should I inspect?" }),
    )
    .await;
    let parked = else_api::db::owner_questions::standing::confirm(
        &app.db.pool,
        turn,
        else_api::app::questions::PermissionTarget::Bot,
        authorization,
    )
    .await;
    assert!(matches!(
        parked,
        Err(
            else_api::db::owner_questions::standing::ConfirmError::Request(
                else_api::db::owner_questions::RequestError::AttemptParked
            )
        )
    ));
    h.release(turn).await;
    wait(app, turn, "waiting").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_routine_grant_covers_future_runs_of_that_routine_only() {
    let h = recorder::harness(4).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (setup, token) = h
        .running(&main, &format!("{GATE} set up my cleanup routines"))
        .await;
    let (prune, due) = create(
        app,
        &token,
        "Prune branches",
        &format!("{GATE_EVERY_ATTEMPT} Delete merged branches on origin."),
        YEARLY,
    )
    .await;
    let (other, other_due) = create(
        app,
        &token,
        "Clean caches",
        &format!("{GATE} Clear the build caches."),
        "0 9 2 1 *",
    )
    .await;
    h.release(setup).await;
    wait(app, setup, "completed").await;

    let (manual, token) = h.running(&main, &format!("{GATE} tidy up")).await;
    let (question, choices) = asks(app, &token, delete_args("old")).await;
    assert_eq!(
        choices, APPROVE_FOREVER,
        "an owner request offers the bot grant"
    );
    h.release(manual).await;
    wait(app, manual, "waiting").await;
    answer(app, &main, question, "approve_forever").await;
    wait(app, manual, "completed").await;

    let run = run_of(&fire(app, due).await, prune);
    let (_, token) = h.started(run).await;
    let (question, choices) = asks(app, &token, delete_args("feature-a")).await;
    assert_eq!(
        choices, APPROVE_ROUTINE,
        "the bot grant does not cover a routine run, which offers only its own grant"
    );
    h.release(run).await;
    wait(app, run, "waiting").await;
    answer(app, &main, question, "approve_routine").await;
    assert_eq!(routine_grants(app).await, [(prune, 1)]);

    let token = h.continued(run, &token).await;
    runs_without_asking(app, &token, delete_args("feature-b")).await;
    h.release(run).await;
    wait(app, run, "completed").await;

    let other_run = run_of(&fire(app, other_due).await, other);
    let (_, token) = h.started(other_run).await;
    let (question, choices) = asks(app, &token, delete_args("feature-c")).await;
    assert_eq!(choices, APPROVE_ROUTINE, "another routine has no grant");
    h.release(other_run).await;
    wait(app, other_run, "waiting").await;
    answer(app, &main, question, "deny").await;
    wait(app, other_run, "completed").await;

    let (manual, token) = h.running(&main, &format!("{GATE} tidy up more")).await;
    let called = tool(
        app,
        &token,
        "ask_permission",
        json!({ "kind": "destructive", "action": "rm -rf ~/old" }),
    )
    .await;
    assert_eq!(called.result["scope"], "bot", "{}", called.body);
    h.release(manual).await;
    wait(app, manual, "completed").await;

    let run = run_of(&fire(app, next_run(app, prune).await).await, prune);
    let (_, token) = h.started(run).await;
    runs_without_asking(app, &token, delete_args("feature-d")).await;
    let host = tool(
        app,
        &token,
        "ask_permission",
        json!({ "kind": "destructive", "action": "git push origin --delete feature-e" }),
    )
    .await;
    assert_eq!(host.result["status"], "approved", "{}", host.body);
    assert_eq!(host.result["scope"], "routine");
    let host = tool(
        app,
        &token,
        "ask_permission",
        json!({ "kind": "external", "action": "mail -s report team@example.com" }),
    )
    .await;
    assert_eq!(host.result["status"], "pending", "{}", host.body);
    let path = format!("/v1/bots/{main}/permissions");
    let listed = app.owner("GET", &path, None).await;
    assert_eq!(
        listed.body["routine_grants"][0]["routine_id"],
        prune.to_string()
    );
    let other_bot = app.create_bot("Other owner conversation").await;
    let refused = app
        .owner(
            "DELETE",
            &format!("/v1/bots/{other_bot}/permissions/routines/{prune}"),
            None,
        )
        .await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND);
    let removed = app
        .owner(
            "DELETE",
            &format!("/v1/bots/{main}/permissions/routines/{prune}"),
            None,
        )
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    let listed = app.owner("GET", &path, None).await;
    assert_eq!(listed.body["routine_grants"], json!([]));
    assert_eq!(listed.body["permissions"][0]["scope"], "destructive");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_grant_covers_an_external_action() {
    let h = recorder::harness(4).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (setup, token) = h.running(&main, &format!("{GATE} set up")).await;
    let (prune, due) = create(
        app,
        &token,
        "Prune branches",
        &format!("{GATE} Delete merged branches and send a report."),
        YEARLY,
    )
    .await;
    h.release(setup).await;
    wait(app, setup, "completed").await;
    let run = run_of(&fire(app, due).await, prune);
    let (_, token) = h.started(run).await;
    let (question, _) = asks(app, &token, delete_args("feature-a")).await;
    h.release(run).await;
    wait(app, run, "waiting").await;
    answer(app, &main, question, "approve_routine").await;
    wait(app, run, "completed").await;

    let run = run_of(&fire(app, next_run(app, prune).await).await, prune);
    let (_, token) = h.started(run).await;
    let (_, choices) = asks(app, &token, send_args()).await;
    assert_eq!(
        choices,
        ["approve", "deny"],
        "external is approved once, always"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_edit_ends_the_grant_and_a_stale_card_grants_nothing() {
    let h = recorder::harness(4).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let first = format!("{GATE_EVERY_ATTEMPT} Delete merged branches on origin.");
    let (setup, token) = h.running(&main, &format!("{GATE} set up")).await;
    let (prune, due) = create(app, &token, "Prune branches", &first, YEARLY).await;
    h.release(setup).await;
    wait(app, setup, "completed").await;

    let run = run_of(&fire(app, due).await, prune);
    let (_, token) = h.started(run).await;
    let (question, _) = asks(app, &token, delete_args("feature-a")).await;
    h.release(run).await;
    wait(app, run, "waiting").await;
    let second = format!("{GATE_EVERY_ATTEMPT} Delete every branch on origin.");
    set_prompt(app, &main, prune, &second).await;
    let refused = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/questions/{question}/answer"),
            Some(json!({ "choice": "approve_routine" })),
        )
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], "routine_changed");
    assert!(
        routine_grants(app).await.is_empty(),
        "a stale card grants nothing"
    );
    wait(app, run, "waiting").await;
    answer(app, &main, question, "approve").await;
    let token = h.continued(run, &token).await;
    runs_without_asking(app, &token, delete_args("feature-a")).await;
    let (_, choices) = asks(app, &token, delete_args("feature-b")).await;
    assert_eq!(
        choices,
        ["approve", "deny"],
        "a run of an old prompt cannot be granted for the routine"
    );
    h.release(run).await;
    wait(app, run, "waiting").await;
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    wait(app, run, "interrupted").await;

    let run = run_of(&fire(app, next_run(app, prune).await).await, prune);
    let (_, token) = h.started(run).await;
    let (question, choices) = asks(app, &token, delete_args("feature-c")).await;
    assert_eq!(choices, APPROVE_ROUTINE);
    h.release(run).await;
    wait(app, run, "waiting").await;
    answer(app, &main, question, "approve_routine").await;
    assert_eq!(routine_grants(app).await, [(prune, 2)]);
    h.release(run).await;
    wait(app, run, "completed").await;
    routines::update(
        &app.db.pool,
        By::Owner {
            bot_id: main.parse().unwrap(),
        },
        prune,
        Patch {
            name: Some(RoutineName::parse("Prune old branches").unwrap()),
            cron: Some("0 10 1 1 *".into()),
            ..Patch::default()
        },
        Utc::now(),
    )
    .await
    .unwrap();
    let run = run_of(&fire(app, next_run(app, prune).await).await, prune);
    let (_, token) = h.started(run).await;
    runs_without_asking(app, &token, delete_args("feature-d")).await;
    h.release(run).await;
    wait(app, run, "completed").await;

    set_prompt(app, &main, prune, &first).await;
    set_prompt(app, &main, prune, &second).await;
    let run = run_of(&fire(app, next_run(app, prune).await).await, prune);
    let (_, token) = h.started(run).await;
    let (_, choices) = asks(app, &token, delete_args("feature-e")).await;
    assert_eq!(choices, APPROVE_ROUTINE, "the old grant does not come back");
    let listed = else_api::db::owner_questions::routine_grants(&app.db.pool, main.parse().unwrap())
        .await
        .unwrap();
    assert!(listed.is_empty(), "{listed:?}");
}

#[tokio::test]
async fn upgrading_removes_the_bot_grant_option_from_pending_routine_cards() {
    let db = common::TestDb::empty().await;
    let old = tempfile::tempdir().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(&source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name().to_str().unwrap() < "0010" {
            std::fs::copy(entry.path(), old.path().join(entry.file_name())).unwrap();
        }
    }
    sqlx::migrate::Migrator::new(old.path())
        .await
        .unwrap()
        .run(&db.pool)
        .await
        .unwrap();
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let bot: Uuid = sqlx::query_scalar("SELECT default_bot_id FROM installation")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let turn = Uuid::new_v4();
    let item = Uuid::new_v4();
    let routine = Uuid::new_v4();
    let question = Uuid::new_v4();
    sqlx::query("INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no, owner_item_id, provider_id, status)
        SELECT ?1, ?2, id, ?1, zeroblob(32), 100, ?3, 'synthetic', 'waiting' FROM conversations WHERE bot_id = ?2 AND kind = 'owner'")
        .bind(turn).bind(bot).bind(item).execute(&db.pool).await.unwrap();
    sqlx::query("INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
        SELECT ?1, conversation_id, 100, id, 'routine', 'Delete merged branches.' FROM turns WHERE id = ?2")
        .bind(item).bind(turn).execute(&db.pool).await.unwrap();
    sqlx::query("INSERT INTO routines (id, bot_id, conversation_id, name, prompt, cron, timezone, created_turn_id, created_name, request_hash)
        SELECT ?1, bot_id, conversation_id, 'Cleanup', 'Delete merged branches.', '0 9 * * *', 'UTC', id, 'Cleanup', zeroblob(32) FROM turns WHERE id = ?2")
        .bind(routine).bind(turn).execute(&db.pool).await.unwrap();
    sqlx::query("INSERT INTO routine_runs (id, routine_id, due_at, name, turn_id) VALUES (randomblob(16), ?1, '2030-01-01T09:00:00Z', 'Cleanup', ?2)")
        .bind(routine).bind(turn).execute(&db.pool).await.unwrap();
    sqlx::query("INSERT INTO owner_questions (id, bot_id, turn_id, attempt, kind, title, choices, action, action_hash)
        VALUES (?1, ?2, ?3, 1, 'destructive', 'Delete merged branches', ?4, '{\"host\":\"Delete merged branches\"}', zeroblob(32))")
        .bind(question).bind(bot).bind(turn)
        .bind(json!([{"id":"approve","label":"Approve once"}, {"id":"deny","label":"Deny"}, {"id":"approve_forever","label":"Always allow destructive actions"}]).to_string())
        .execute(&db.pool).await.unwrap();
    else_api::db::migrate(&db.pool).await.unwrap();
    let shown = else_api::db::turns::load_turn(&mut db.pool.acquire().await.unwrap(), turn)
        .await
        .unwrap();
    assert_eq!(
        shown.questions[0]
            .choices
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        ["approve", "deny"]
    );
    let refused = else_api::db::owner_questions::answer(
        &db.pool,
        bot,
        question,
        &Answer::Single("approve_forever".into()),
    )
    .await;
    assert!(matches!(
        refused,
        Err(else_api::db::owner_questions::SettleError::InvalidChoice)
    ));
    let approved = else_api::db::owner_questions::answer(
        &db.pool,
        bot,
        question,
        &Answer::Single("approve".into()),
    )
    .await
    .unwrap();
    assert!(approved.question.approved());
    assert!(
        else_api::db::owner_questions::permissions(&db.pool, bot)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        else_api::db::owner_questions::routine_grants(&db.pool, bot)
            .await
            .unwrap()
            .is_empty()
    );
}
