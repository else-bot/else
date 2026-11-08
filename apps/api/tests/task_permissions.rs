mod common;

use std::time::Duration;

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use common::TestApp;
use common::recorder::{self, GATE, GATE_EVERY_ATTEMPT, Harness, tool};
use serde_json::{Value, json};
use uuid::Uuid;

const TASK: &str = "Manage Tailscale via SSH";
const HOST: &str = "omarchy-m1";
const ALWAYS: [&str; 3] = ["approve", "deny", "approve_task"];
const ONCE: [&str; 2] = ["approve", "deny"];

fn scoped(kind: &str, command: &str, task: &str, target: &str) -> Value {
    json!({ "kind": kind, "action": format!("ssh {target} sudo tailscale {command}"),
            "reason": "Manage Tailscale", "task_scope": { "task": task, "target": target } })
}

fn tailscale(command: &str) -> Value {
    scoped("destructive", command, TASK, HOST)
}

fn task_target(task: &str) -> Value {
    json!({ "scope": "task", "task": task, "target": HOST, "kind": "destructive" })
}

const UP_TO_DATE_REQUEST: &str =
    "Can you enable require PR to be up to date with branch before merging";

fn requested_host_action() -> Value {
    json!({
        "kind": "external",
        "action": "gh api --method PUT repos/else-bot/else/rulesets/26617470 --input merge-rules/require-up-to-date.json",
        "reason": "Require PR branches to be up to date",
        "authorization": UP_TO_DATE_REQUEST,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_owner_request_runs_its_host_action_without_another_card() {
    let h = recorder::harness(4).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, token) = h
        .running(&bot, &format!("{GATE} {UP_TO_DATE_REQUEST}"))
        .await;
    let called = tool(app, &token, "ask_permission", requested_host_action()).await;
    assert_eq!(called.result["status"], "approved", "{}", called.body);
    assert_eq!(called.result["standing"], false);
    assert_eq!(called.result["scope"], Value::Null);
    assert!(permissions(app, &bot).await.is_empty());
    let questions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM owner_questions WHERE turn_id = ?1")
            .bind(turn)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(questions, 0, "the request never creates an approval card");
    h.release(turn).await;
    wait(app, turn, "completed").await;

    let (next, token) = h.running(&bot, GATE).await;
    let called = tool(app, &token, "ask_permission", requested_host_action()).await;
    assert_eq!(called.result["status"], "pending", "{}", called.body);
    h.release(next).await;
    wait(app, next, "waiting").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invented_or_malformed_host_authorization_does_not_approve_an_action() {
    let h = recorder::harness(4).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;
    for invalid in [json!(""), json!("  "), json!(false), json!(["yes"])] {
        let mut args = requested_host_action();
        args["authorization"] = invalid;
        let called = tool(app, &token, "ask_permission", args).await;
        assert!(called.error, "{}", called.body);
        assert_eq!(called.code(), "INVALID_REQUEST", "{}", called.body);
    }
    let called = tool(app, &token, "ask_permission", requested_host_action()).await;
    assert_eq!(called.result["status"], "pending", "{}", called.body);
    assert!(permissions(app, &bot).await.is_empty());
    h.release(turn).await;
    wait(app, turn, "waiting").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_quote_cannot_override_a_pending_or_denied_host_action() {
    use else_api::app::questions::{Spec, permission_authorization};
    use else_api::db::owner_questions::{self, Gate};

    let h = recorder::harness(4).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, token) = h
        .running(&bot, &format!("{GATE_EVERY_ATTEMPT} {UP_TO_DATE_REQUEST}"))
        .await;
    let args = requested_host_action();
    let mut without_quote = args.clone();
    without_quote
        .as_object_mut()
        .unwrap()
        .remove("authorization");
    let (question, _) = pending(app, &token, without_quote).await;
    let spec = Spec::permission(&args).unwrap();
    let quote = permission_authorization(&args).unwrap();
    assert!(matches!(
        owner_questions::gate_host(&app.db.pool, turn, &spec, quote)
            .await
            .unwrap(),
        Gate::Pending(_)
    ));
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    assert_eq!(answer(app, &bot, question, "deny").await, StatusCode::OK);
    let token = h.continued(turn, &token).await;
    let called = tool(app, &token, "ask_permission", args).await;
    assert_eq!(called.result["status"], "denied", "{}", called.body);
    assert!(permissions(app, &bot).await.is_empty());
    h.release(turn).await;
    wait(app, turn, "completed").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_quote_cannot_approve_a_different_action_after_the_attempt_parks() {
    use else_api::app::questions::{Spec, permission_authorization};
    use else_api::db::owner_questions::{self, RequestError};

    let h = recorder::harness(4).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, token) = h
        .running(&bot, &format!("{GATE} {UP_TO_DATE_REQUEST}"))
        .await;
    let called = tool(
        app,
        &token,
        "ask_owner",
        json!({"question": "Which repository?", "choices": ["else", "other"]}),
    )
    .await;
    assert_eq!(called.result["status"], "pending", "{}", called.body);
    let args = requested_host_action();
    let result = owner_questions::gate_host(
        &app.db.pool,
        turn,
        &Spec::permission(&args).unwrap(),
        permission_authorization(&args).unwrap(),
    )
    .await;
    assert!(
        matches!(result, Err(RequestError::AttemptParked)),
        "{result:?}"
    );
    h.release(turn).await;
    wait(app, turn, "waiting").await;
}

async fn wait(app: &TestApp, turn: Uuid, status: &str) {
    app.wait_turn_for(&turn.to_string(), &[status], Duration::from_secs(30))
        .await;
}

async fn answer(app: &TestApp, bot: &str, question: Uuid, choice: &str) -> StatusCode {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/questions/{question}/answer"),
        Some(json!({ "choice": choice })),
    )
    .await
    .status
}

async fn pending(app: &TestApp, token: &str, args: Value) -> (Uuid, Vec<String>) {
    let called = tool(app, token, "ask_permission", args).await;
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

async fn runs_by_task_grant(app: &TestApp, token: &str, args: Value) {
    let called = tool(app, token, "ask_permission", args).await;
    assert_eq!(called.result["status"], "approved", "{}", called.body);
    assert_eq!(called.result["scope"], "task", "{}", called.body);
}

async fn asks_in_new_request(h: &Harness, bot: &str, args: Value, choices: &[&str]) {
    let (turn, token) = h.running(bot, GATE).await;
    let (_, shown) = pending(&h.app, &token, args).await;
    assert_eq!(shown, choices);
    h.release(turn).await;
    wait(&h.app, turn, "waiting").await;
}

async fn permissions(app: &TestApp, bot: &str) -> Vec<Value> {
    let listed = app
        .owner("GET", &format!("/v1/bots/{bot}/permissions"), None)
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    listed.body["permissions"].as_array().unwrap().clone()
}

async fn revoke(app: &TestApp, bot: &str, scope: &str) {
    let revoked = app
        .owner(
            "DELETE",
            &format!("/v1/bots/{bot}/permissions/{scope}"),
            None,
        )
        .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{}", revoked.body);
}

async fn always_allow(h: &Harness, bot: &str, args: Value) -> Uuid {
    let (turn, token) = h.running(bot, GATE).await;
    let (question, choices) = pending(&h.app, &token, args).await;
    assert_eq!(choices, ALWAYS);
    h.release(turn).await;
    wait(&h.app, turn, "waiting").await;
    assert_eq!(
        answer(&h.app, bot, question, "approve_task").await,
        StatusCode::OK
    );
    wait(&h.app, turn, "completed").await;
    question
}

#[tokio::test(flavor = "multi_thread")]
async fn always_allow_covers_one_task_on_one_target_until_revoked() {
    let h = recorder::harness(8).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let question = always_allow(&h, &main, tailscale("up --ssh")).await;
    let granted = permissions(app, &main).await;
    assert_eq!(granted.len(), 1, "{granted:?}");
    assert_eq!(granted[0]["target"], task_target(TASK));
    assert_eq!(granted[0]["question_id"], question.to_string());
    let scope = granted[0]["scope"].as_str().unwrap().to_owned();
    assert!(scope.starts_with("task:destructive:"), "{scope}");
    let card: String = sqlx::query_scalar("SELECT action FROM owner_questions WHERE id = ?1")
        .bind(question)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&card).unwrap()["task_scope"],
        json!({ "task": TASK, "target": HOST }),
        "the card stores its structured scope"
    );

    let (next, token) = h.running(&main, GATE).await;
    let input = else_api::db::turns::input(&app.db.pool, next)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&input.standing_permissions).unwrap(),
        json!([task_target(TASK)])
    );
    runs_by_task_grant(app, &token, tailscale("set --advertise-exit-node")).await;
    runs_by_task_grant(app, &token, tailscale("set --advertise-exit-node")).await;
    runs_by_task_grant(
        app,
        &token,
        scoped("destructive", "down", "  manage  tailscale VIA ssh ", HOST),
    )
    .await;
    let unscoped = json!({ "kind": "destructive", "action": "ssh omarchy-m1 sudo tailscale down" });
    let (_, choices) = pending(app, &token, unscoped).await;
    assert_eq!(choices, ONCE, "an undeclared scope is never covered");
    h.release(next).await;
    wait(app, next, "waiting").await;

    asks_in_new_request(
        &h,
        &main,
        scoped("destructive", "up", TASK, "omarchy-m2"),
        &ALWAYS,
    )
    .await;
    asks_in_new_request(
        &h,
        &main,
        scoped("destructive", "up", "Manage Docker via SSH", HOST),
        &ALWAYS,
    )
    .await;
    asks_in_new_request(&h, &main, scoped("external", "up", TASK, HOST), &ALWAYS).await;
    let other = app.create_bot("Other").await;
    asks_in_new_request(&h, &other, tailscale("up"), &ALWAYS).await;
    assert_eq!(permissions(app, &main).await.len(), 1);

    revoke(app, &main, &scope).await;
    assert!(permissions(app, &main).await.is_empty());
    asks_in_new_request(&h, &main, tailscale("set --advertise-exit-node"), &ALWAYS).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn approving_once_or_denying_remembers_nothing_and_plain_host_cards_offer_no_blanket_grant() {
    let h = recorder::harness(8).await;
    let app = &h.app;
    let main = app.main_bot().await;
    for choice in ["approve", "deny"] {
        let (turn, token) = h.running(&main, GATE).await;
        let (question, choices) = pending(app, &token, tailscale("up")).await;
        assert_eq!(choices, ALWAYS);
        h.release(turn).await;
        wait(app, turn, "waiting").await;
        assert_eq!(answer(app, &main, question, choice).await, StatusCode::OK);
        wait(app, turn, "completed").await;
        assert!(permissions(app, &main).await.is_empty(), "{choice}");
    }

    let (turn, token) = h.running(&main, GATE).await;
    let (question, choices) = pending(
        app,
        &token,
        json!({ "kind": "destructive", "action": "rm -rf ~/old-site" }),
    )
    .await;
    assert_eq!(choices, ONCE);
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    for refused in ["approve_forever", "approve_task", "approve_tool"] {
        assert_eq!(
            answer(app, &main, question, refused).await,
            StatusCode::BAD_REQUEST,
            "{refused}"
        );
    }
    assert_eq!(
        answer(app, &main, question, "approve").await,
        StatusCode::OK
    );
    wait(app, turn, "completed").await;
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM bot_permissions")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(grants, 0, "no blanket destructive grant");

    asks_in_new_request(&h, &main, tailscale("up"), &ALWAYS).await;
    asks_in_new_request(
        &h,
        &main,
        json!({ "kind": "destructive", "action": "rm -rf ~/other-site" }),
        &ONCE,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exact_denial_outlasts_a_task_grant_given_later_in_the_request() {
    let h = recorder::harness(4).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let (denied, _) = pending(app, &token, tailscale("down")).await;
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    assert_eq!(answer(app, &main, denied, "deny").await, StatusCode::OK);
    let token = h.continued(turn, &token).await;

    let (allowed, choices) = pending(app, &token, tailscale("up")).await;
    assert_eq!(choices, ALWAYS);
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    assert_eq!(
        answer(app, &main, allowed, "approve_task").await,
        StatusCode::OK
    );
    let token = h.continued(turn, &token).await;

    let again = tool(app, &token, "ask_permission", tailscale("down")).await;
    assert_eq!(again.result["status"], "denied", "{}", again.body);
    runs_by_task_grant(app, &token, tailscale("up")).await;
    runs_by_task_grant(app, &token, tailscale("status")).await;
    h.release(turn).await;
    wait(app, turn, "completed").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_can_record_a_task_grant_from_the_owner_words_only() {
    let h = recorder::harness(8).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let authorization = "Always allow managing Tailscale over SSH on omarchy-m1.";
    let (turn, token) = h.running(&main, &format!("{GATE} {authorization}")).await;
    let confirm = |target: Value, authorization: &str| {
        let token = token.clone();
        let authorization = authorization.to_owned();
        async move {
            tool(
                app,
                &token,
                "confirm_permission",
                json!({ "target": target, "authorization": authorization }),
            )
            .await
        }
    };
    let forged = confirm(task_target(TASK), "Always allow everything.").await;
    assert_eq!(forged.code(), "NOT_AUTHORIZED", "{}", forged.body);
    for malformed in [
        task_target("   "),
        task_target(&"x".repeat(201)),
        json!({ "scope": "task", "task": TASK, "target": HOST, "kind": "choice" }),
    ] {
        let refused = confirm(malformed.clone(), authorization).await;
        assert_eq!(refused.code(), "NOT_AUTHORIZED", "{malformed}");
    }
    let refused = confirm(json!({ "scope": "task", "task": TASK }), authorization).await;
    assert!(refused.error, "{}", refused.body);
    assert!(permissions(app, &main).await.is_empty());

    let confirmed = confirm(task_target(" Manage  Tailscale via SSH "), authorization).await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    let granted = permissions(app, &main).await;
    assert_eq!(granted[0]["target"], task_target(TASK), "stored normalized");
    let retry = confirm(task_target(TASK), authorization).await;
    assert_eq!(
        retry.result["question_id"], confirmed.result["question_id"],
        "{}",
        retry.body
    );
    runs_by_task_grant(app, &token, tailscale("up")).await;

    let scope = granted[0]["scope"].as_str().unwrap().to_owned();
    revoke(app, &main, &scope).await;
    let retry = confirm(task_target(TASK), authorization).await;
    assert_eq!(retry.result["status"], "revoked", "{}", retry.body);
    let renamed_retry = confirm(task_target(&TASK.to_lowercase()), authorization).await;
    assert_eq!(
        renamed_retry.code(),
        "AUTHORIZATION_USED",
        "{}",
        renamed_retry.body
    );
    assert!(permissions(app, &main).await.is_empty());
    let (_, choices) = pending(app, &token, tailscale("down")).await;
    assert_eq!(choices, ALWAYS, "revoked: asks again");
    h.release(turn).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn work_delegated_for_an_owner_request_uses_but_never_records_a_task_grant() {
    let h = recorder::harness(8).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let worker = app.create_bot("Worker").await;
    let authorization = "Always allow managing Tailscale over SSH on omarchy-m1.";
    let (worker_turn, worker_token) = h.running(&worker, &format!("{GATE} {authorization}")).await;
    let confirmed = tool(
        app,
        &worker_token,
        "confirm_permission",
        json!({ "target": task_target(TASK), "authorization": authorization }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    h.release(worker_turn).await;
    wait(app, worker_turn, "completed").await;

    let (turn, token) = h.running(&main, GATE).await;
    let delegated = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": worker, "task": format!("{GATE} {authorization}") }),
    )
    .await;
    assert!(!delegated.error, "{}", delegated.body);
    let task: Uuid = delegated.result["task_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let (_, child_token) = h.started(child).await;
    let saved = &h.inputs(child).await[0].standing_permissions;
    assert_eq!(
        saved.len(),
        1,
        "the task sees its saved task scope: {saved:?}"
    );
    runs_by_task_grant(app, &child_token, tailscale("up")).await;
    let recorded = tool(
        app,
        &child_token,
        "confirm_permission",
        json!({ "target": task_target("Manage Docker via SSH"), "authorization": authorization }),
    )
    .await;
    assert_ne!(recorded.result["status"], "approved", "{}", recorded.body);
    let (_, choices) = pending(
        app,
        &child_token,
        scoped("destructive", "up", TASK, "other-host"),
    )
    .await;
    assert_eq!(choices, ONCE, "another target asks once, offering no grant");
    assert_eq!(permissions(app, &worker).await.len(), 1);
    for turn in [turn, child] {
        h.release(turn).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn routine_runs_keep_routine_grants_and_never_receive_task_grants() {
    let h = recorder::harness(8).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let authorization = "Always allow managing Tailscale over SSH on omarchy-m1.";
    let (setup, token) = h.running(&main, &format!("{GATE} {authorization}")).await;
    let created = tool(
        app,
        &token,
        "routine_create",
        json!({ "name": "Tailscale", "prompt": format!("{GATE} Refresh Tailscale."),
                "cron": "0 9 1 1 *" }),
    )
    .await;
    assert!(!created.error, "{}", created.body);
    let due: DateTime<Utc> =
        DateTime::parse_from_rfc3339(created.result["routine"]["next_run_at"].as_str().unwrap())
            .unwrap()
            .to_utc();
    let confirmed = tool(
        app,
        &token,
        "confirm_permission",
        json!({ "target": task_target(TASK), "authorization": authorization }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    h.release(setup).await;
    wait(app, setup, "completed").await;

    else_api::db::routines::admit_due(&app.db.pool, app.scheduler.registry(), due, 8)
        .await
        .unwrap();
    app.scheduler.wake();
    let run: Uuid =
        sqlx::query_scalar("SELECT turn_id FROM routine_runs WHERE turn_id IS NOT NULL")
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    let (_, token) = h.started(run).await;
    let refused = tool(
        app,
        &token,
        "confirm_permission",
        json!({ "target": task_target(TASK), "authorization": "Refresh Tailscale." }),
    )
    .await;
    assert!(refused.error, "{}", refused.body);
    let mut action = tailscale("up");
    action["authorization"] = json!("Refresh Tailscale.");
    let (question, choices) = pending(app, &token, action).await;
    assert_eq!(choices, ["approve", "deny", "approve_routine"]);
    h.release(run).await;
    wait(app, run, "waiting").await;
    assert_eq!(
        answer(app, &main, question, "approve_task").await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(answer(app, &main, question, "deny").await, StatusCode::OK);
    wait(app, run, "completed").await;
}
