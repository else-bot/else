//! Owner questions: a bot asks the owner to choose or to approve one
//! action over MCP, the owner answers through the owner API, and the same
//! turn continues with the choice. A newer owner message skips a pending
//! card and grants nothing; an approval covers exactly one call.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::{TestApp, TestDb};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::synthetic::Synthetic;
use serde_json::{Value, json};
use uuid::Uuid;

/// Long enough (a few seconds) to call tools while it streams.
const SLOW: &str = "[synthetic:slow] send the newsletter to the subscribers list.";

struct Harness {
    app: TestApp,
    synthetic: Arc<Synthetic>,
}

async fn harness() -> Harness {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let synthetic = Arc::new(Synthetic::new(Duration::from_millis(5)));
    let registry = else_api::providers::Registry::new(vec![synthetic.clone()]);
    Harness {
        app: TestApp::with_registry(db, 4, registry),
        synthetic,
    }
}

async fn grant(app: &TestApp, turn_id: Uuid, tools: Vec<Tool>) -> String {
    let (bot, conversation): (Uuid, Uuid) =
        sqlx::query_as("SELECT bot_id, conversation_id FROM turns WHERE id = ?1")
            .bind(turn_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    app.scheduler
        .grants()
        .issue(bot, conversation, turn_id, tools, DEFAULT_TTL)
        .await
}

fn owner_tools() -> Vec<Tool> {
    vec![Tool::ReadMemory, Tool::AskOwner, Tool::Computer]
}

async fn running(app: &TestApp, bot: &str) -> (Uuid, String) {
    let sent = app.send(bot, Uuid::new_v4(), SLOW, Some("synthetic")).await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["running"]).await;
    let turn_id: Uuid = turn.parse().unwrap();
    (turn_id, grant(app, turn_id, owner_tools()).await)
}

async fn tool(app: &TestApp, token: &str, name: &str, args: Value) -> common::Reply {
    app.call(
        "POST",
        "/tools/v1/mcp",
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                     "params": { "name": name, "arguments": args } })),
        Some(token),
    )
    .await
}

fn tool_text(reply: &common::Reply) -> Value {
    let text = reply.body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    serde_json::from_str(text).unwrap_or_else(|_| json!(text))
}

fn is_error(reply: &common::Reply) -> bool {
    reply.body["result"]["isError"] == true
}

fn choice_args() -> Value {
    json!({ "question": "Which template for the newsletter?",
            "choices": ["Plain text", "HTML digest"],
            "context": "Draft: Hello everyone, this month..." })
}

fn send_args() -> Value {
    json!({ "command": "node send.js --all", "effect": "external", "secrets": ["smtp"],
            "purpose": "Send the newsletter to 120 subscribers",
            "context": "To: newsletter subscribers\nDraft: Hello everyone, this month's update is ready." })
}

fn delete_args() -> Value {
    json!({ "command": "rm -rf ~/work/old-site", "effect": "destructive" })
}

async fn answer(app: &TestApp, bot: &str, question: Uuid, choice: &str) -> common::Reply {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/questions/{question}/answer"),
        Some(json!({ "choice": choice })),
    )
    .await
}

async fn status(app: &TestApp, turn: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn wait(app: &TestApp, turn: Uuid, status: &str) {
    app.wait_turn_for(&turn.to_string(), &[status], Duration::from_secs(30))
        .await;
}

async fn question_row(app: &TestApp, question: Uuid) -> (String, Option<String>, Option<i32>) {
    sqlx::query_as("SELECT status, answer, consumed_attempt FROM owner_questions WHERE id = ?1")
        .bind(question)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn permissions(app: &TestApp, bot: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM bot_permissions WHERE bot_id = ?1")
        .bind(bot.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn shown_turn(app: &TestApp, bot: &str, turn: Uuid) -> Value {
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .expect("the turn is in the snapshot")
        .clone()
}

async fn journaled(app: &TestApp, bot: &str, question: Uuid) -> Value {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT kind, data FROM bot_events WHERE bot_id = ?1 ORDER BY seq")
            .bind(bot.parse::<Uuid>().unwrap())
            .fetch_all(&app.db.pool)
            .await
            .unwrap();
    rows.iter()
        .filter(|(kind, _)| kind == "turn.updated")
        .filter_map(|(_, data)| serde_json::from_str::<Value>(data).ok())
        .filter_map(|data| {
            data["turn"]["questions"]
                .as_array()?
                .iter()
                .find(|q| q["id"] == question.to_string())
                .cloned()
        })
        .next_back()
        .expect("the question was journaled")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_choice_parks_the_turn_and_the_answer_continues_it() {
    let Harness { app, synthetic } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    let asked = tool(&app, &token, "ask_owner", choice_args()).await;
    let result = tool_text(&asked);
    assert_eq!(result["status"], "pending", "{}", asked.body);
    assert_eq!(result["parked"], true);
    let question: Uuid = result["question_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(result["choices"][1]["id"], "b");
    let more = tool(&app, &token, "memory_bots", json!({})).await;
    assert_eq!(more.status, StatusCode::CONFLICT, "{}", more.body);
    assert_eq!(more.body["error"]["code"], "attempt_parked");

    wait(&app, turn, "waiting").await;
    let shown = shown_turn(&app, &main, turn).await;
    let card = &shown["questions"][0];
    assert_eq!(card["id"], question.to_string());
    assert_eq!(card["kind"], "choice");
    assert_eq!(card["status"], "pending");
    assert_eq!(card["title"], "Which template for the newsletter?");
    assert_eq!(card["context"], "Draft: Hello everyone, this month...");
    assert_eq!(card["choices"][0]["label"], "Plain text");
    assert_eq!(card["answer"], Value::Null);
    assert_eq!(journaled(&app, &main, question).await["status"], "pending");

    let bad = answer(&app, &main, question, "c").await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.body);
    assert_eq!(bad.body["error"]["code"], "invalid_choice");
    let chosen = answer(&app, &main, question, "b").await;
    assert_eq!(chosen.status, StatusCode::OK, "{}", chosen.body);
    assert_eq!(chosen.body["question"]["status"], "answered");
    assert_eq!(chosen.body["question"]["answer"], "b");
    assert_eq!(chosen.body["requeued"], true);
    wait(&app, turn, "completed").await;
    let shown = shown_turn(&app, &main, turn).await;
    assert_eq!(shown["questions"][0]["status"], "answered");
    assert_eq!(journaled(&app, &main, question).await["answer"], "b");
    let (_, _, consumed) = question_row(&app, question).await;
    assert_eq!(consumed, None, "a choice is not an action approval");

    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(packet.contains("[else.bot owner answers]"), "{packet}");
    assert!(packet.contains("The owner chose: HTML digest"), "{packet}");
    assert!(packet.contains("kind=\"choice\""), "{packet}");

    let again = answer(&app, &main, question, "b").await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body["requeued"], false);
    let stale = answer(&app, &main, question, "a").await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);
    assert_eq!(stale.body["error"]["code"], "question_settled");
    assert_eq!(question_row(&app, question).await.1, Some("b".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn asking_the_same_question_again_returns_it_and_other_shapes_conflict() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    for bad in [
        json!({ "question": "q", "choices": ["only one"] }),
        json!({ "question": "q", "choices": ["a", "b"], "extra": 1 }),
    ] {
        let refused = tool(&app, &token, "ask_owner", bad).await;
        assert!(
            tool_text(&refused)
                .as_str()
                .unwrap()
                .starts_with("INVALID_QUESTION"),
            "{}",
            refused.body
        );
    }
    let first = tool_text(&tool(&app, &token, "ask_owner", choice_args()).await);
    assert_eq!(first["status"], "pending", "{first}");
    let parked = tool(&app, &token, "ask_owner", choice_args()).await;
    assert_eq!(parked.status, StatusCode::CONFLICT, "{}", parked.body);
    let token = grant(&app, turn, owner_tools()).await;
    let again = tool_text(&tool(&app, &token, "ask_owner", choice_args()).await);
    assert_eq!(again["question_id"], first["question_id"]);
    assert_eq!(again["status"], "pending");
    let token = grant(&app, turn, owner_tools()).await;
    let mut other = choice_args();
    other["choices"] = json!(["Plain text", "Both"]);
    let conflict = tool(&app, &token, "ask_owner", other).await;
    assert!(is_error(&conflict), "{}", conflict.body);
    assert!(
        tool_text(&conflict)
            .as_str()
            .unwrap()
            .starts_with("CONFLICT")
    );
    let cards: i64 = sqlx::query_scalar("SELECT count(*) FROM owner_questions")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(cards, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_follow_up_skips_the_card_and_grants_nothing() {
    let Harness { app, synthetic } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    let pending = tool_text(&tool(&app, &token, "computer_run", delete_args()).await);
    assert_eq!(pending["status"], "pending", "{pending}");
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    wait(&app, turn, "waiting").await;

    let follow_up = "Actually, keep the old site for now and just archive it.";
    let sent = app
        .send(&main, Uuid::new_v4(), follow_up, Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let (row_status, row_answer, consumed) = question_row(&app, question).await;
    assert_eq!(
        (row_status.as_str(), row_answer, consumed),
        ("skipped", None, None)
    );
    assert_eq!(permissions(&app, &main).await, 0);
    assert_eq!(journaled(&app, &main, question).await["status"], "skipped");
    assert_eq!(status(&app, turn).await, "interrupted");

    let late = answer(&app, &main, question, "approve").await;
    assert_eq!(late.status, StatusCode::CONFLICT, "{}", late.body);
    assert_eq!(
        late.body["error"]["message"],
        "This question was already skipped."
    );
    assert_eq!(question_row(&app, question).await.0, "skipped");
    app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(packet.contains(follow_up));
    assert!(!packet.contains("[else.bot owner answers]"), "{packet}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follow_up_while_the_asking_attempt_still_runs_skips_the_card_too() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    let pending = tool_text(&tool(&app, &token, "ask_owner", choice_args()).await);
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(status(&app, turn).await, "running");
    let sent = app
        .send(&main, Uuid::new_v4(), "Use plain text.", Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(question_row(&app, question).await.0, "skipped");
    app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    assert_eq!(status(&app, turn).await, "interrupted");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_open_question_uses_a_card_and_chat_supersedes_it_without_approval() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    let pending = tool_text(
        &tool(
            &app,
            &token,
            "ask_owner",
            json!({ "question": "Who should receive the draft?" }),
        )
        .await,
    );
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(pending["status"], "pending");
    let shown = shown_turn(&app, &main, turn).await;
    assert_eq!(
        shown["questions"][0]["title"],
        "Who should receive the draft?"
    );
    assert_eq!(shown["questions"][0]["choices"], json!([]));
    let sent = app
        .send(
            &main,
            Uuid::new_v4(),
            "Prepare it for Alice, but do not send it.",
            Some("synthetic"),
        )
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED);
    assert_eq!(question_row(&app, question).await.0, "skipped");
    assert_eq!(permissions(&app, &main).await, 0);
    assert_eq!(
        answer(&app, &main, question, "approve").await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn destructive_file_writes_wait_for_owner_approval() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    let refused = tool(
        &app,
        &token,
        "computer_write_file",
        json!({ "path": "important.txt", "content": "replacement" }),
    )
    .await;
    assert!(is_error(&refused));
    let pending = tool_text(&tool(&app, &token, "computer_write_file", json!({
        "path": "important.txt", "content": "replacement", "effect": "destructive", "purpose": "Replace important.txt"
    })).await);
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["parked"], true);
    let shown = shown_turn(&app, &main, turn).await;
    let card = &shown["questions"][0];
    assert_eq!(card["kind"], "destructive");
    assert_eq!(card["title"], "Replace important.txt");
    assert_eq!(card["context"], "Write 11 bytes to important.txt");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_external_action_needs_approval_for_exactly_that_call_once() {
    let Harness { app, synthetic } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;

    let undeclared = tool(
        &app,
        &token,
        "computer_run",
        json!({ "command": "node send.js --all" }),
    )
    .await;
    assert!(is_error(&undeclared));
    assert!(
        tool_text(&undeclared)
            .as_str()
            .unwrap()
            .starts_with("EFFECT_REQUIRED"),
        "{}",
        undeclared.body
    );
    let none: i64 = sqlx::query_scalar("SELECT count(*) FROM owner_questions")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(none, 0, "a missing effect asks nothing and runs nothing");

    let pending = tool(&app, &token, "computer_run", send_args()).await;
    let result = tool_text(&pending);
    assert_eq!(result["status"], "pending", "{}", pending.body);
    assert_eq!(result["kind"], "external");
    let question: Uuid = result["question_id"].as_str().unwrap().parse().unwrap();
    let parked = tool(&app, &token, "computer_run", send_args()).await;
    assert_eq!(parked.status, StatusCode::CONFLICT, "the attempt must end");
    let retry = grant(&app, turn, owner_tools()).await;
    let same = tool_text(&tool(&app, &retry, "computer_run", send_args()).await);
    assert_eq!(same["question_id"], question.to_string(), "no second card");

    wait(&app, turn, "waiting").await;
    let card = shown_turn(&app, &main, turn).await["questions"][0].clone();
    assert_eq!(card["kind"], "external");
    assert_eq!(card["title"], "Send the newsletter to 120 subscribers");
    assert_eq!(
        card["context"],
        "To: newsletter subscribers\nDraft: Hello everyone, this month's update is ready."
    );
    assert_eq!(card["action"]["tool"], "computer_run");
    assert_eq!(card["action"]["args"]["command"], "node send.js --all");
    let ids: Vec<&str> = card["choices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["approve", "deny"], "no standing grant for external");

    let forever = answer(&app, &main, question, "approve_forever").await;
    assert_eq!(forever.status, StatusCode::BAD_REQUEST, "{}", forever.body);
    assert_eq!(permissions(&app, &main).await, 0);
    let approved = answer(&app, &main, question, "approve").await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(approved.body["requeued"], true);
    wait(&app, turn, "completed").await;
    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("kind=\"external\" status=\"answered\" answer=\"approve\""),
        "{packet}"
    );
    assert!(
        packet.contains(
            "Approved: call computer_run again with the original tool call arguments, once"
        ),
        "{packet}"
    );

    let token = grant(&app, turn, owner_tools()).await;
    sqlx::query("UPDATE turns SET status = 'running' WHERE id = ?1")
        .bind(turn)
        .execute(&app.db.pool)
        .await
        .unwrap();
    let ran = tool(&app, &token, "computer_run", send_args()).await;
    let text = tool_text(&ran);
    assert!(
        text.as_str()
            .is_some_and(|t| t.starts_with("COMPUTER_UNAVAILABLE")),
        "the approved call reached execution: {}",
        ran.body
    );
    assert_eq!(
        question_row(&app, question).await.2,
        Some(2),
        "consumed once"
    );
    let again = tool_text(&tool(&app, &token, "computer_run", send_args()).await);
    assert_eq!(
        again["status"], "pending",
        "the same action again is a new card: {again}"
    );
    assert_ne!(again["question_id"], question.to_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_action_stays_denied_for_the_request() {
    let Harness { app, synthetic } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    let pending = tool_text(&tool(&app, &token, "computer_run", send_args()).await);
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    wait(&app, turn, "waiting").await;
    let denied = answer(&app, &main, question, "deny").await;
    assert_eq!(denied.status, StatusCode::OK, "{}", denied.body);
    wait(&app, turn, "completed").await;
    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(packet.contains("answer=\"deny\""), "{packet}");
    assert!(
        packet.contains("Denied: do not do this external action"),
        "{packet}"
    );

    let token = grant(&app, turn, owner_tools()).await;
    sqlx::query("UPDATE turns SET status = 'running' WHERE id = ?1")
        .bind(turn)
        .execute(&app.db.pool)
        .await
        .unwrap();
    let retry = tool(&app, &token, "computer_run", send_args()).await;
    assert!(is_error(&retry));
    assert!(
        tool_text(&retry).as_str().unwrap().starts_with("DENIED"),
        "{}",
        retry.body
    );
    let cards: i64 = sqlx::query_scalar("SELECT count(*) FROM owner_questions")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(cards, 1, "a denial is not asked again in this request");
}

#[tokio::test(flavor = "multi_thread")]
async fn always_allow_covers_destructive_actions_only_until_revoked() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    let pending = tool_text(&tool(&app, &token, "computer_run", delete_args()).await);
    assert_eq!(pending["kind"], "destructive");
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    wait(&app, turn, "waiting").await;
    let card = shown_turn(&app, &main, turn).await["questions"][0].clone();
    assert_eq!(card["choices"][2]["id"], "approve_forever");
    assert_eq!(card["title"], "Run a command in the bot's computer");

    let forever = answer(&app, &main, question, "approve_forever").await;
    assert_eq!(forever.status, StatusCode::OK, "{}", forever.body);
    assert_eq!(permissions(&app, &main).await, 1);
    let listed = app
        .owner("GET", &format!("/v1/bots/{main}/permissions"), None)
        .await;
    assert_eq!(listed.body["permissions"][0]["scope"], "destructive");
    wait(&app, turn, "completed").await;

    let (turn, token) = running(&app, &main).await;
    let other_delete = json!({ "command": "rm -rf ~/work/cache", "effect": "destructive" });
    let ran = tool(&app, &token, "computer_run", other_delete.clone()).await;
    assert!(
        tool_text(&ran)
            .as_str()
            .is_some_and(|t| t.starts_with("COMPUTER_UNAVAILABLE")),
        "{}",
        ran.body
    );
    let host = tool_text(
        &tool(
            &app,
            &token,
            "ask_permission",
            json!({ "kind": "destructive", "action": "rm -rf ~/old", "reason": "Clean up" }),
        )
        .await,
    );
    assert_eq!(host["status"], "approved");
    assert_eq!(host["standing"], true);
    let external = tool_text(&tool(&app, &token, "computer_run", send_args()).await);
    assert_eq!(external["status"], "pending", "{external}");
    wait(&app, turn, "waiting").await;

    let revoked = app
        .owner(
            "DELETE",
            &format!("/v1/bots/{main}/permissions/destructive"),
            None,
        )
        .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    assert_eq!(permissions(&app, &main).await, 0);
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success());
    wait(&app, turn, "interrupted").await;
    let (turn, token) = running(&app, &main).await;
    let asks = tool_text(&tool(&app, &token, "computer_run", other_delete).await);
    assert_eq!(asks["status"], "pending", "revoked: asks again");
    wait(&app, turn, "waiting").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_records_never_store_typed_text() {
    let Harness { app, synthetic } = harness().await;
    let main = app.main_bot().await;
    let canary = "PRIVATE-TYPED-TEXT-934f";
    for (name, args) in [
        (
            "computer_input",
            json!({ "frame": [100, 100], "actions": [{ "type": canary }], "effect": "external" }),
        ),
        (
            "computer_browser",
            json!({ "action": "type", "selector": "#message", "text": canary, "effect": "external" }),
        ),
    ] {
        let (turn, token) = running(&app, &main).await;
        let pending = tool_text(&tool(&app, &token, name, args).await);
        let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
        let snapshot = shown_turn(&app, &main, turn).await;
        assert!(!snapshot.to_string().contains(canary));
        assert!(
            !journaled(&app, &main, question)
                .await
                .to_string()
                .contains(canary)
        );
        let stored: String = sqlx::query_scalar("SELECT action FROM owner_questions WHERE id = ?1")
            .bind(question)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
        assert!(!stored.contains(canary), "{stored}");
        wait(&app, turn, "waiting").await;
        assert_eq!(
            answer(&app, &main, question, "approve").await.status,
            StatusCode::OK
        );
        wait(&app, turn, "completed").await;
        let packet = synthetic.packets().await.last().unwrap().clone();
        assert!(packet.contains("[else.bot owner answers]"));
        assert!(!packet.contains(canary), "{packet}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_continues_only_after_all_owner_requests_settle() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    let pending = tool_text(&tool(&app, &token, "ask_owner", choice_args()).await);
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    let secret = Uuid::new_v4();
    sqlx::query("INSERT INTO secret_requests (id, bot_id, turn_id, attempt, name, reason, env_names)
        SELECT ?1, bot_id, id, attempt, 'login', 'Authenticate', '[\"LOGIN_TOKEN\"]' FROM turns WHERE id = ?2")
        .bind(secret).bind(turn).execute(&app.db.pool).await.unwrap();
    wait(&app, turn, "waiting").await;
    let declined = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/secret-requests/{secret}/cancel"),
            None,
        )
        .await;
    assert_eq!(declined.status, StatusCode::OK, "{}", declined.body);
    assert_eq!(status(&app, turn).await, "waiting");
    assert_eq!(question_row(&app, question).await.0, "pending");
    assert_eq!(
        answer(&app, &main, question, "a").await.status,
        StatusCode::OK
    );
    wait(&app, turn, "completed").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_permission_waits_and_the_answer_names_the_action() {
    let Harness { app, synthetic } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    let args = json!({ "kind": "external", "action": "mail -s 'Invoice' client@example.com < invoice.txt",
                       "reason": "Email the invoice to the client" });
    let pending = tool_text(&tool(&app, &token, "ask_permission", args.clone()).await);
    assert_eq!(pending["status"], "pending", "{pending}");
    assert_eq!(pending["parked"], true);
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    wait(&app, turn, "waiting").await;
    let card = shown_turn(&app, &main, turn).await["questions"][0].clone();
    assert_eq!(card["title"], "Email the invoice to the client");
    assert_eq!(
        card["action"]["host"],
        "mail -s 'Invoice' client@example.com < invoice.txt"
    );
    assert_eq!(card["choices"].as_array().unwrap().len(), 2);

    assert_eq!(
        answer(&app, &main, question, "approve").await.status,
        StatusCode::OK
    );
    wait(&app, turn, "completed").await;
    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("Host action: mail -s 'Invoice' client@example.com"),
        "{packet}"
    );
    assert!(
        packet.contains("Approved: do exactly this host action, once"),
        "{packet}"
    );

    let token = grant(&app, turn, owner_tools()).await;
    sqlx::query("UPDATE turns SET status = 'running' WHERE id = ?1")
        .bind(turn)
        .execute(&app.db.pool)
        .await
        .unwrap();
    let again = tool_text(&tool(&app, &token, "ask_permission", args).await);
    assert_eq!(again["status"], "pending");
    assert_ne!(again["question_id"], question.to_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_owner_answers_and_only_for_the_asking_bot() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let other = app.create_bot("Home").await;
    let (turn, token) = running(&app, &main).await;
    let pending = tool_text(&tool(&app, &token, "ask_owner", choice_args()).await);
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    let path = format!("/v1/bots/{main}/questions/{question}/answer");
    for bearer in [Some(token.as_str()), None, Some("wrong")] {
        let reply = app
            .call("POST", &path, Some(json!({ "choice": "a" })), bearer)
            .await;
        assert!(
            matches!(
                reply.status,
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ),
            "{bearer:?}: {}",
            reply.status
        );
    }
    assert_eq!(
        answer(&app, &other, question, "a").await.status,
        StatusCode::NOT_FOUND,
        "another bot's question looks like none"
    );
    assert_eq!(
        answer(&app, &main, Uuid::new_v4(), "a").await.status,
        StatusCode::NOT_FOUND
    );
    let malformed = app
        .owner("POST", &path, Some(json!({ "choice": "a", "note": "x" })))
        .await;
    assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
    assert_eq!(question_row(&app, question).await.0, "pending");

    let bare = grant(&app, turn, vec![Tool::ReadMemory]).await;
    let refused = tool(&app, &bare, "ask_owner", choice_args()).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn delegated_tasks_cannot_ask_and_cannot_act_destructively() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let leaf = app.create_bot("Ops").await;
    let warm = app
        .send(&leaf, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    app.wait_turn(warm.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let (turn, _) = running(&app, &main).await;
    let parent = grant(&app, turn, vec![Tool::ReadMemory, Tool::Delegate]).await;
    let delegated = tool(
        &app,
        &parent,
        "delegate",
        json!({ "target_bot_id": leaf, "task": "[synthetic:slow] clean the server disk now" }),
    )
    .await;
    let task: Uuid = tool_text(&delegated)["task_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    app.wait_turn_for(&child.to_string(), &["running"], Duration::from_secs(30))
        .await;
    let child_token = grant(&app, child, owner_tools()).await;
    for (name, args) in [
        ("ask_owner", choice_args()),
        (
            "ask_permission",
            json!({ "kind": "external", "action": "mail x" }),
        ),
        ("computer_run", delete_args()),
    ] {
        let refused = tool(&app, &child_token, name, args).await;
        assert!(is_error(&refused), "{name}: {}", refused.body);
        assert!(
            tool_text(&refused)
                .as_str()
                .unwrap()
                .starts_with("NOT_ALLOWED"),
            "{name}: {}",
            refused.body
        );
    }
    let local = tool(
        &app,
        &child_token,
        "computer_run",
        json!({ "command": "ls", "effect": "local" }),
    )
    .await;
    assert!(
        tool_text(&local)
            .as_str()
            .is_some_and(|t| t.starts_with("COMPUTER_UNAVAILABLE")),
        "a local call is not gated: {}",
        local.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_and_restart_settle_or_keep_pending_cards() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;

    let (turn, token) = running(&app, &main).await;
    let pending = tool_text(&tool(&app, &token, "ask_owner", choice_args()).await);
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    wait(&app, turn, "waiting").await;
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    wait(&app, turn, "interrupted").await;
    assert_eq!(question_row(&app, question).await.0, "cancelled");
    assert_eq!(
        answer(&app, &main, question, "a").await.status,
        StatusCode::CONFLICT
    );

    let (turn, token) = running(&app, &main).await;
    let pending = tool_text(&tool(&app, &token, "computer_run", delete_args()).await);
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    wait(&app, turn, "waiting").await;
    let app = app.restart().await;
    assert_eq!(
        app.wait_turn(&turn.to_string(), &["waiting"]).await,
        "waiting"
    );
    assert_eq!(question_row(&app, question).await.0, "pending");
    assert_eq!(
        shown_turn(&app, &main, turn).await["questions"][0]["status"],
        "pending",
        "the card survives a restart"
    );
    let approved = answer(&app, &main, question, "approve").await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    wait(&app, turn, "completed").await;
    let attempts: i32 = sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(attempts, 2, "the same turn continued after the restart");
}
