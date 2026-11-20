//! "Always allow" on approval cards outside the bot-wide destructive grant:
//! external routine cards, MCP tools in routine runs, computer tools without
//! a task scope, and an owner who types "Always allow" instead of choosing.

use crate::common;

use axum::http::StatusCode;
use common::TestApp;
use common::recorder::{self, GATE, GATE_EVERY_ATTEMPT, Harness, tool};
use serde_json::{Value, json};
use uuid::Uuid;

use super::routine_grants::{
    YEARLY, answer, asks, create, delete_args, fire, next_run, run_of, runs_without_asking,
    send_args, wait,
};
use super::tool_permissions::{SERVER, create as ticket, executed};

async fn routine_grant_kinds(app: &TestApp, routine: Uuid) -> Vec<String> {
    sqlx::query_scalar("SELECT kind FROM routine_grants WHERE routine_id = ?1 ORDER BY kind")
        .bind(routine)
        .fetch_all(&app.db.pool)
        .await
        .unwrap()
}

async fn question(app: &TestApp, id: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, answer FROM owner_questions WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn permissions(app: &TestApp, bot: &str) -> Value {
    let listed = app
        .owner("GET", &format!("/v1/bots/{bot}/permissions"), None)
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    listed.body
}

/// A routine saved from a finished owner request, with the tickets MCP
/// installed when `mcp` is set.
async fn routine(
    h: &Harness,
    bot: &str,
    prompt: &str,
    mcp: bool,
) -> (Uuid, chrono::DateTime<chrono::Utc>) {
    let (setup, token) = h.running(bot, &format!("{GATE} set up")).await;
    if mcp {
        let installed = tool(
            &h.app,
            &token,
            "mcp_install",
            json!({"id":"tickets", "name":"Tickets", "description":"Disposable ticket board",
                "skill":"Create tickets on request.", "documentation":"Use tools/list.",
                "config":{"type":"stdio", "command":"python3", "args":["-u", "-c", SERVER]}}),
        )
        .await;
        assert!(!installed.error, "{}", installed.body);
    }
    let created = create(
        &h.app,
        &token,
        "Daily job",
        &format!("{GATE} {prompt}"),
        YEARLY,
    )
    .await;
    h.release(setup).await;
    wait(&h.app, setup, "completed").await;
    created
}

#[tokio::test(flavor = "multi_thread")]
async fn an_external_routine_card_always_allows_that_routine_until_revoked() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (report, due) = routine(&h, &main, "Send the build report.", false).await;

    let run = run_of(&fire(app, due).await, report);
    let (_, token) = h.started(run).await;
    let (card, choices) = asks(app, &token, send_args()).await;
    assert_eq!(choices, ["approve", "deny", "approve_routine"]);
    h.release(run).await;
    wait(app, run, "waiting").await;
    answer(app, &main, card, "approve_routine").await;
    wait(app, run, "completed").await;
    assert_eq!(routine_grant_kinds(app, report).await, ["external"]);
    let listed = permissions(app, &main).await;
    assert_eq!(listed["routine_grants"][0]["kind"], "external");

    let run = run_of(&fire(app, next_run(app, report).await).await, report);
    let (_, token) = h.started(run).await;
    runs_without_asking(app, &token, send_args()).await;
    let (_, choices) = asks(app, &token, delete_args("feature-a")).await;
    assert_eq!(
        choices,
        ["approve", "deny", "approve_routine"],
        "an external grant does not cover destructive actions"
    );
    h.release(run).await;
    wait(app, run, "waiting").await;

    let revoked = app
        .owner(
            "DELETE",
            &format!("/v1/bots/{main}/permissions/routines/{report}/external"),
            None,
        )
        .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    assert!(routine_grant_kinds(app, report).await.is_empty());
    let again = app
        .owner(
            "DELETE",
            &format!("/v1/bots/{main}/permissions/routines/{report}/external"),
            None,
        )
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_tool_card_in_a_routine_run_saves_a_tool_grant_the_next_run_uses() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (daily, due) = routine(&h, &main, "Create the daily ticket.", true).await;

    let run = run_of(&fire(app, due).await, daily);
    let (_, token) = h.started(run).await;
    let pending = tool(app, &token, "mcp_request", ticket("external", "Monday")).await;
    assert_eq!(pending.result["status"], "pending", "{}", pending.body);
    let card: Uuid = pending.result["question_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let choices: String = sqlx::query_scalar("SELECT choices FROM owner_questions WHERE id = ?1")
        .bind(card)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert!(choices.contains("approve_tool"), "{choices}");
    h.release(run).await;
    wait(app, run, "waiting").await;
    answer(app, &main, card, "approve_tool").await;
    wait(app, run, "completed").await;

    let run = run_of(&fire(app, next_run(app, daily).await).await, daily);
    let (_, token) = h.started(run).await;
    executed(
        &tool(app, &token, "mcp_request", ticket("external", "Tuesday")).await,
        "Tuesday",
    );
    let deletion = tool(
        app,
        &token,
        "mcp_request",
        json!({"id":"tickets", "method":"tools/call",
            "params":{"name":"delete_ticket","arguments":{"id":"WEB-1"}},
            "effect":"destructive", "purpose":"Delete a ticket"}),
    )
    .await;
    assert_eq!(
        deletion.result["status"], "pending",
        "another tool still asks: {}",
        deletion.body
    );
    h.release(run).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_external_computer_call_without_a_task_scope_offers_a_grant_for_that_tool() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, &format!("{GATE} send the report")).await;
    let (card, choices) = asks(app, &token, send_args()).await;
    assert_eq!(choices, ["approve", "deny", "approve_tool"]);
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    answer(app, &main, card, "approve_tool").await;
    wait(app, turn, "completed").await;
    let listed = permissions(app, &main).await;
    let target = json!({"scope":"computer_tool", "tool":"computer_run", "kind":"external"});
    assert_eq!(listed["permissions"][0]["target"], target);

    let (turn, token) = h.running(&main, &format!("{GATE} send it again")).await;
    let mut other = send_args();
    other["command"] = json!("node send.js --team");
    runs_without_asking(app, &token, other).await;
    let (deletion, choices) = asks(app, &token, delete_args("feature-a")).await;
    assert_eq!(
        choices,
        ["approve", "deny", "approve_forever"],
        "the tool grant does not cover destructive calls"
    );
    let scope = listed["permissions"][0]["scope"].as_str().unwrap();
    let revoked = app
        .owner(
            "DELETE",
            &format!("/v1/bots/{main}/permissions/{scope}"),
            None,
        )
        .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    answer(app, &main, deletion, "deny").await;
    wait(app, turn, "completed").await;

    let (turn, token) = h.running(&main, &format!("{GATE} and once more")).await;
    asks(app, &token, send_args()).await;
    h.release(turn).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn typing_always_allow_answers_the_waiting_card_with_its_grant_and_nothing_wider() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h
        .running(&main, &format!("{GATE_EVERY_ATTEMPT} send the report"))
        .await;
    let (card, _) = asks(app, &token, send_args()).await;
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    h.send(&main, "Always allow.").await;
    assert_eq!(
        question(app, card).await,
        ("answered".to_owned(), Some("approve_tool".to_owned()))
    );
    let listed = permissions(app, &main).await;
    assert_eq!(listed["permissions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["permissions"][0]["target"]["tool"], "computer_run");
    let token = h.continued(turn, &token).await;
    let again = tool(
        app,
        &token,
        "confirm_permission",
        json!({ "target": { "scope": "bot" }, "authorization": "Always allow." }),
    )
    .await;
    assert_eq!(again.code(), "CARD_ANSWERED", "{}", again.body);
    h.release(turn).await;
    wait(app, turn, "completed").await;

    let (turn, token) = h.running(&main, &format!("{GATE} prune branches")).await;
    let (card, _) = asks(app, &token, delete_args("feature-a")).await;
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    h.send(&main, "Always allow.").await;
    assert_eq!(
        question(app, card).await,
        ("skipped".to_owned(), None),
        "the bot-wide destructive grant needs a click"
    );
    assert_eq!(
        permissions(app, &main).await["permissions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn typing_always_approve_answers_a_waiting_routine_card() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (report, due) = routine(&h, &main, "Send the build report.", false).await;
    let run = run_of(&fire(app, due).await, report);
    let (_, token) = h.started(run).await;
    let (card, _) = asks(app, &token, send_args()).await;
    h.release(run).await;
    wait(app, run, "waiting").await;
    h.send(&main, "Always approve.").await;
    assert_eq!(
        question(app, card).await,
        ("answered".to_owned(), Some("approve_routine".to_owned()))
    );
    assert_eq!(routine_grant_kinds(app, report).await, ["external"]);
    wait(app, run, "completed").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_typed_reply_after_another_owner_message_answers_no_older_card() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (report, due) = routine(&h, &main, "Send the build report.", false).await;
    let run = run_of(&fire(app, due).await, report);
    let (_, token) = h.started(run).await;
    let (card, _) = asks(app, &token, send_args()).await;
    h.release(run).await;
    wait(app, run, "waiting").await;
    let chat = h.send(&main, "What is on my calendar today?").await;
    wait(app, chat, "completed").await;
    h.send(&main, "Always approve.").await;
    assert_eq!(question(app, card).await, ("pending".to_owned(), None));
    assert!(routine_grant_kinds(app, report).await.is_empty());
    answer(app, &main, card, "deny").await;
    wait(app, run, "completed").await;
}
