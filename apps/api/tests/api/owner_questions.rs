use crate::common;

use std::time::Duration;

use axum::http::StatusCode;
use common::TestApp;
use common::recorder::{
    Called, GATE, GATE_AFTER_OPENING, GATE_EVERY_ATTEMPT, Harness, harness, tool,
};
use else_api::grants::Tool;
use serde_json::{Value, json};
use uuid::Uuid;

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

async fn sidebar_status(app: &TestApp, bot: &str) -> Value {
    let list = app.owner("GET", "/v1/bots", None).await;
    let status = list.body["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == bot)
        .unwrap()["sidebar_status"]
        .clone();
    let data: String = sqlx::query_scalar(
        "SELECT data FROM bot_events WHERE bot_id = ?1 AND kind = 'bot.updated' ORDER BY seq DESC LIMIT 1",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    let event: Value = serde_json::from_str(&data).unwrap();
    assert_eq!(event["bot"]["sidebar_status"], status);
    without_since(status)
}

/// The status with its request time checked and removed.
fn without_since(mut status: Value) -> Value {
    if let Some(since) = status.as_object_mut().and_then(|f| f.remove("since")) {
        assert!(
            since
                .as_str()
                .is_some_and(|at| at.parse::<chrono::DateTime<chrono::Utc>>().is_ok()),
            "{since}"
        );
    }
    status
}

#[tokio::test(flavor = "multi_thread")]
async fn a_choice_parks_the_turn_and_the_answer_continues_it() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let asked = tool(app, &token, "ask_owner", choice_args()).await;
    let result = asked.result;
    assert_eq!(result["status"], "pending", "{}", asked.body);
    assert_eq!(result["parked"], true);
    let question: Uuid = result["question_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(result["choices"][1]["id"], "b");
    let more = tool(app, &token, "memory_bots", json!({})).await;
    assert_eq!(more.status, StatusCode::CONFLICT, "{}", more.body);
    assert_eq!(more.body["error"]["code"], "attempt_parked");

    h.release(turn).await;
    wait(app, turn, "waiting").await;
    let shown = shown_turn(app, &main, turn).await;
    let card = &shown["questions"][0];
    assert_eq!(card["id"], question.to_string());
    assert_eq!(card["kind"], "choice");
    assert_eq!(card["status"], "pending");
    assert_eq!(card["title"], "Which template for the newsletter?");
    assert_eq!(card["context"], "Draft: Hello everyone, this month...");
    assert_eq!(card["choices"][0]["label"], "Plain text");
    assert_eq!(card["answer"], Value::Null);
    assert_eq!(journaled(app, &main, question).await["status"], "pending");
    assert_eq!(
        sidebar_status(app, &main).await,
        json!({ "kind": "waiting_for_answer", "preview": "Which template for the newsletter?" })
    );

    let bad = answer(app, &main, question, "c").await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.body);
    assert_eq!(bad.body["error"]["code"], "invalid_choice");
    let chosen = answer(app, &main, question, "b").await;
    assert_eq!(chosen.status, StatusCode::OK, "{}", chosen.body);
    assert_eq!(chosen.body["question"]["status"], "answered");
    assert_eq!(chosen.body["question"]["answer"], "b");
    assert_eq!(chosen.body["requeued"], true);
    wait(app, turn, "completed").await;
    assert_eq!(sidebar_status(app, &main).await, Value::Null);
    let shown = shown_turn(app, &main, turn).await;
    assert_eq!(shown["questions"][0]["status"], "answered");
    assert_eq!(journaled(app, &main, question).await["answer"], "b");
    let (_, _, consumed) = question_row(app, question).await;
    assert_eq!(consumed, None, "a choice is not an action approval");

    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("[else.bot owner answers]"), "{packet}");
    assert!(packet.contains("The owner chose: HTML digest"), "{packet}");
    assert!(packet.contains("kind=\"choice\""), "{packet}");

    let again = answer(app, &main, question, "b").await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body["requeued"], false);
    let stale = answer(app, &main, question, "a").await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);
    assert_eq!(stale.body["error"]["code"], "question_settled");
    assert_eq!(question_row(app, question).await.1, Some("b".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_turns_unread_when_its_turn_parks_on_a_question() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_AFTER_OPENING).await;
    let read_state = || async {
        app.owner("GET", &format!("/v1/bots/{main}"), None)
            .await
            .body["bot"]["read_state"]
            .clone()
    };
    tokio::time::timeout(common::SETTLE, async {
        loop {
            let snapshot = app
                .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
                .await;
            let items = snapshot.body["items"].as_array().unwrap().clone();
            if items.iter().any(|i| i["kind"] == "assistant") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the reply started");
    let working = read_state().await;
    assert_eq!(working["latest"], Value::Null, "{working}");
    assert_eq!(working["unread"], false);

    let asked = tool(app, &token, "ask_owner", choice_args()).await;
    assert_eq!(asked.result["parked"], true, "{}", asked.body);
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    let parked = read_state().await;
    assert_eq!(parked["latest"]["turn_id"], turn.to_string(), "{parked}");
    assert_eq!(parked["unread"], true);
    assert_eq!(parked["unread_replies"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn five_options_accept_exactly_one_answer_by_default_or_explicitly() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    for multiple in [None, Some(false)] {
        let (turn, token) = h.running(&main, GATE).await;
        let mut args = json!({
            "question": "What should I prepare for the changelog?",
            "choices": ["Skip posting", "Prepare a Discord + Telegram draft",
                "Prepare a Discord + Telegram + X draft", "Edit the draft first", "Something else"],
        });
        if let Some(multiple) = multiple {
            args["multiple"] = json!(multiple);
        }
        let pending = tool(app, &token, "ask_owner", args).await.result;
        let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
        assert_eq!(pending["multiple"], false);
        h.release(turn).await;
        wait(app, turn, "waiting").await;
        let shown = shown_turn(app, &main, turn).await;
        let card = &shown["questions"][0];
        assert_eq!(card["multiple"], false);
        assert_eq!(card["choices"].as_array().unwrap().len(), 5);
        assert_eq!(
            card["choices"][3],
            json!({ "id": "d", "label": "Edit the draft first" })
        );
        assert_eq!(
            card["choices"][4],
            json!({ "id": "e", "label": "Something else" })
        );

        for invalid in [json!(["d"]), json!(["a", "d"])] {
            let refused = select(app, &main, question, invalid).await;
            assert_eq!(refused.status, StatusCode::BAD_REQUEST);
            assert_eq!(refused.body["error"]["code"], "invalid_choice");
        }
        assert_eq!(status(app, turn).await, "waiting");
        let chosen = answer(app, &main, question, "d").await;
        assert_eq!(chosen.status, StatusCode::OK, "{}", chosen.body);
        assert_eq!(chosen.body["question"]["answer"], "d");
        assert_eq!(chosen.body["requeued"], true);
        wait(app, turn, "completed").await;
        let shown = shown_turn(app, &main, turn).await;
        assert_eq!(shown["questions"][0]["status"], "answered");
        assert_eq!(shown["questions"][0]["answer"], "d");
        assert_eq!(journaled(app, &main, question).await["answer"], "d");
        let packet = h.packets().await.last().unwrap().clone();
        assert!(
            packet.contains("The owner chose: Edit the draft first"),
            "{packet}"
        );
        assert_eq!(
            answer(app, &main, question, "a").await.status,
            StatusCode::CONFLICT
        );
        assert_eq!(question_row(app, question).await.1, Some("d".into()));
    }
}

fn multiple_args() -> Value {
    json!({
        "question": "Which inbox tasks should I take care of?",
        "choices": ["Archive cold recruiter emails", "Unsubscribe from newsletters",
                    "Label receipts and invoices", "Draft replies to starred emails",
                    "Summarize unread threads"],
        "multiple": true,
    })
}

async fn select(app: &TestApp, bot: &str, question: Uuid, choices: Value) -> common::Reply {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/questions/{question}/answer"),
        Some(json!({ "choices": choices })),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn multiple_choices_wait_for_done_and_deliver_only_selected_actions() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let pending = tool(app, &token, "ask_owner", multiple_args()).await.result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(pending["multiple"], true);
    assert_eq!(pending["choices"].as_array().unwrap().len(), 5);
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    let shown = shown_turn(app, &main, turn).await;
    assert_eq!(shown["questions"][0]["multiple"], true);
    assert_eq!(shown["questions"][0]["answer"], Value::Null);
    assert_eq!(journaled(app, &main, question).await["multiple"], true);

    for invalid in [
        json!([]),
        json!(["a", "a"]),
        json!(["f"]),
        json!(["a", "b", "c", "d", "e", "f"]),
    ] {
        let refused = select(app, &main, question, invalid).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    }
    assert_eq!(
        answer(app, &main, question, "a").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(status(app, turn).await, "waiting");
    let chosen = select(app, &main, question, json!(["e", "c"])).await;
    assert_eq!(chosen.status, StatusCode::OK, "{}", chosen.body);
    assert_eq!(chosen.body["question"]["answer"], json!(["c", "e"]));
    assert_eq!(chosen.body["requeued"], true);
    wait(app, turn, "completed").await;
    let card = &shown_turn(app, &main, turn).await["questions"][0];
    assert_eq!(card["status"], "answered");
    assert_eq!(card["answer"], json!(["c", "e"]));
    assert_eq!(
        journaled(app, &main, question).await["answer"],
        json!(["c", "e"])
    );
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("Label receipts and invoices"), "{packet}");
    assert!(packet.contains("Summarize unread threads"), "{packet}");
    assert!(
        !packet.contains("Archive cold recruiter emails"),
        "{packet}"
    );
    assert!(!packet.contains("Unsubscribe from newsletters"), "{packet}");
    assert!(
        !packet.contains("Draft replies to starred emails"),
        "{packet}"
    );
    assert_eq!(permissions(app, &main).await, 0);
    let again = select(app, &main, question, json!(["e", "c"])).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body["requeued"], false);
    let stale = select(app, &main, question, json!(["a"])).await;
    assert_eq!(stale.status, StatusCode::CONFLICT);
    assert_eq!(stale.body["error"]["code"], "question_settled");
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_closes_a_multiple_choice_card_without_selecting_actions() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let pending = tool(app, &token, "ask_owner", multiple_args()).await.result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    let sent = app
        .send(
            &main,
            Uuid::new_v4(),
            "Only summarize unread threads.",
            Some("synthetic"),
        )
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED);
    let stale = select(app, &main, question, json!(["a", "b"])).await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);
    assert_eq!(stale.body["error"]["code"], "question_settled");
    let shown = shown_turn(app, &main, turn).await;
    assert_eq!(shown["questions"][0]["status"], "skipped");
    assert_eq!(shown["questions"][0]["answer"], Value::Null);
    assert_eq!(permissions(app, &main).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn asking_the_same_question_again_returns_it_and_other_shapes_conflict() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    for bad in [
        json!({ "question": "q", "choices": ["only one"] }),
        json!({ "question": "q", "choices": ["a", "b"], "extra": 1 }),
    ] {
        let refused = tool(app, &token, "ask_owner", bad).await;
        assert!(
            refused
                .result
                .as_str()
                .unwrap()
                .starts_with("INVALID_QUESTION"),
            "{}",
            refused.body
        );
    }
    let first = tool(app, &token, "ask_owner", choice_args()).await.result;
    assert_eq!(first["status"], "pending", "{first}");
    let parked = tool(app, &token, "ask_owner", choice_args()).await;
    assert_eq!(parked.status, StatusCode::CONFLICT, "{}", parked.body);
    use else_api::app::questions::{Spec, Status};
    use else_api::db::owner_questions::{RequestError, create};

    let again = create(&app.db.pool, turn, &Spec::choice(&choice_args()).unwrap())
        .await
        .unwrap();
    assert_eq!(again.id.to_string(), first["question_id"]);
    assert_eq!(again.status, Status::Pending);
    let mut multiple = choice_args();
    multiple["multiple"] = json!(true);
    let mut other = choice_args();
    other["choices"] = json!(["Plain text", "Both"]);
    for changed in [multiple, other] {
        let spec = Spec::choice(&changed).unwrap();
        assert_eq!(
            create(&app.db.pool, turn, &spec).await.unwrap_err(),
            RequestError::Conflict
        );
    }
    let cards: i64 = sqlx::query_scalar("SELECT count(*) FROM owner_questions")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(cards, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_follow_up_skips_the_card_and_grants_nothing() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let pending = tool(app, &token, "computer_run", delete_args())
        .await
        .result;
    assert_eq!(pending["status"], "pending", "{pending}");
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(turn).await;
    wait(app, turn, "waiting").await;

    let follow_up = "Actually, keep the old site for now and just archive it.";
    let sent = app
        .send(&main, Uuid::new_v4(), follow_up, Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["steered_into"], turn.to_string());
    let (row_status, row_answer, consumed) = question_row(app, question).await;
    assert_eq!(
        (row_status.as_str(), row_answer, consumed),
        ("skipped", None, None)
    );
    assert_eq!(permissions(app, &main).await, 0);
    assert_eq!(journaled(app, &main, question).await["status"], "skipped");

    let late = answer(app, &main, question, "approve").await;
    assert_eq!(late.status, StatusCode::CONFLICT, "{}", late.body);
    assert_eq!(
        late.body["error"]["message"],
        "This question was already skipped."
    );
    assert_eq!(question_row(app, question).await.0, "skipped");
    wait(app, turn, "completed").await;
    app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    assert_eq!(attempts(app, turn).await, 2, "the reply continued with it");
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains(follow_up));
    assert!(
        packet.contains(&format!(
            "id=\"{question}\" kind=\"destructive\" status=\"skipped\""
        )) && packet.contains("Not approved"),
        "the reply learns its action was not approved: {packet}"
    );
}

async fn attempts(app: &TestApp, turn: Uuid) -> i32 {
    sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follow_up_while_the_asking_attempt_still_runs_skips_the_card_too() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let pending = tool(app, &token, "ask_owner", choice_args()).await.result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(status(app, turn).await, "running");
    let sent = app
        .send(&main, Uuid::new_v4(), "Use plain text.", Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["steered_into"], turn.to_string());
    assert_eq!(question_row(app, question).await.0, "skipped");
    h.release(turn).await;
    wait(app, turn, "completed").await;
    app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    assert_eq!(attempts(app, turn).await, 2, "the reply continued with it");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_open_question_uses_a_card_and_chat_supersedes_it_without_approval() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let pending = tool(
        app,
        &token,
        "ask_owner",
        json!({ "question": "Who should receive the draft?" }),
    )
    .await
    .result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(pending["status"], "pending");
    let shown = shown_turn(app, &main, turn).await;
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
    assert_eq!(question_row(app, question).await.0, "skipped");
    assert_eq!(permissions(app, &main).await, 0);
    assert_eq!(
        answer(app, &main, question, "approve").await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn destructive_file_writes_wait_for_owner_approval() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let refused = tool(
        app,
        &token,
        "computer_write_file",
        json!({ "path": "important.txt", "content": "replacement" }),
    )
    .await;
    assert!(refused.error);
    let pending = tool(app, &token, "computer_write_file", json!({
        "path": "important.txt", "content": "replacement", "effect": "destructive", "purpose": "Replace important.txt"
    })).await.result;
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["parked"], true);
    let shown = shown_turn(app, &main, turn).await;
    let card = &shown["questions"][0];
    assert_eq!(card["kind"], "destructive");
    assert_eq!(card["title"], "Replace important.txt");
    assert_eq!(card["context"], "Write 11 bytes to important.txt");
    let question: Uuid = card["id"].as_str().unwrap().parse().unwrap();
    let invalid = select(app, &main, question, json!(["approve", "approve_forever"])).await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    assert_eq!(permissions(app, &main).await, 0);
    assert_eq!(
        sidebar_status(app, &main).await,
        json!({ "kind": "needs_approval", "preview": "Replace important.txt" })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_external_action_needs_approval_for_exactly_that_call_once() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;

    let undeclared = tool(
        app,
        &token,
        "computer_run",
        json!({ "command": "node send.js --all" }),
    )
    .await;
    assert!(undeclared.error);
    assert!(
        undeclared
            .result
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

    let pending = tool(app, &token, "computer_run", send_args()).await;
    let result = pending.result;
    assert_eq!(result["status"], "pending", "{}", pending.body);
    assert_eq!(result["kind"], "external");
    let question: Uuid = result["question_id"].as_str().unwrap().parse().unwrap();
    let parked = tool(app, &token, "computer_run", send_args()).await;
    assert_eq!(parked.status, StatusCode::CONFLICT, "the attempt must end");
    let args = send_args();
    let spec = else_api::app::questions::Spec::tool_approval(
        else_api::app::questions::Kind::External,
        "computer_run",
        &args,
        None,
        args["purpose"].as_str().unwrap().to_owned(),
        args["context"].as_str().unwrap().to_owned(),
    );
    let same = else_api::db::owner_questions::gate(&app.db.pool, turn, &spec, None)
        .await
        .unwrap();
    assert!(
        matches!(same, else_api::db::owner_questions::Gate::Pending(card) if card.id == question),
        "a repeated request uses the existing card"
    );

    h.release(turn).await;
    wait(app, turn, "waiting").await;
    let card = shown_turn(app, &main, turn).await["questions"][0].clone();
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
    assert_eq!(
        ids,
        ["approve", "deny", "approve_tool"],
        "only a grant for this tool's external calls"
    );

    let forever = answer(app, &main, question, "approve_forever").await;
    assert_eq!(forever.status, StatusCode::BAD_REQUEST, "{}", forever.body);
    assert_eq!(permissions(app, &main).await, 0);
    let approved = answer(app, &main, question, "approve").await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(approved.body["requeued"], true);
    let token = h.continued(turn, &token).await;
    let packet = h.packets().await.last().unwrap().clone();
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

    let ran = tool(app, &token, "computer_run", send_args()).await;
    let text = ran.result;
    assert!(
        text.as_str()
            .is_some_and(|t| t.starts_with("COMPUTER_UNAVAILABLE")),
        "the approved call reached execution: {}",
        ran.body
    );
    assert_eq!(
        question_row(app, question).await.2,
        Some(2),
        "consumed once"
    );
    let again = tool(app, &token, "computer_run", send_args()).await.result;
    assert_eq!(
        again["status"], "pending",
        "the same action again is a new card: {again}"
    );
    assert_ne!(again["question_id"], question.to_string());
}

/// Installs the fixture MCP, asks for `call` (an external MCP request),
/// approves its card, and runs it again in the resumed attempt.
async fn approved_mcp_call(h: &Harness, bot: &str, call: Value) -> (Uuid, Uuid, Called) {
    let app = &h.app;
    let (turn, token) = h.running(bot, GATE_EVERY_ATTEMPT).await;
    let installed = tool(
        app,
        &token,
        "mcp_install",
        crate::shared_mcp::installation(),
    )
    .await;
    assert!(!installed.error, "{}", installed.body);
    let pending = tool(app, &token, "mcp_request", call.clone()).await.result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    assert_eq!(
        answer(app, bot, question, "approve").await.status,
        StatusCode::OK
    );
    let token = h.continued(turn, &token).await;
    let card = shown_turn(app, bot, turn).await["questions"][0].clone();
    assert_eq!(card["used"], Value::Null, "approved but not yet run");
    let ran = tool(app, &token, "mcp_request", call).await;
    h.release(turn).await;
    wait(app, turn, "completed").await;
    (turn, question, ran)
}

fn echo_call(tool: &str) -> Value {
    json!({ "id": "t3-code", "method": "tools/call", "effect": "external",
            "params": { "name": tool, "arguments": { "title": "Rust 1.100" } },
            "purpose": "File the Rust 1.100 issue" })
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approved_call_that_ran_before_a_failure_stays_visible() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, question, ran) = approved_mcp_call(&h, &main, echo_call("echo")).await;
    assert!(!ran.error, "the approved call ran: {}", ran.body);
    let card = shown_turn(app, &main, turn).await["questions"][0].clone();
    assert_eq!(card["used"]["outcome"], "completed", "{card}");
    let ran_at = card["used"]["at"]
        .as_str()
        .and_then(|at| at.parse::<chrono::DateTime<chrono::Utc>>().ok())
        .unwrap_or_else(|| panic!("the card records when the call ran: {card}"));
    assert_eq!(journaled(app, &main, question).await["used"], card["used"]);

    sqlx::query(
        "UPDATE turns SET status = 'failed', error_code = 'provider_policy',
             error_message = 'Claude reported native agent activity (task_started)'
         WHERE id = ?1",
    )
    .bind(turn)
    .execute(&app.db.pool)
    .await
    .unwrap();
    let records = format!(
        "- turn 1 (failed): mcp_request \"{}\" completed at {}\n",
        card["title"].as_str().unwrap(),
        ran_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    for (message, told) in [("Did you file it?", true), ("Thanks.", false)] {
        let sent = app
            .send(&main, Uuid::new_v4(), message, Some("synthetic"))
            .await;
        app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
            .await;
        let packet = h.packets().await.last().unwrap().clone();
        assert_eq!(
            packet.contains("[else.bot approved actions]") && packet.contains(&records),
            told,
            "the warm session is told once: {packet}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approved_mcp_call_the_upstream_tool_rejects_records_an_error() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, _, ran) = approved_mcp_call(&h, &main, echo_call("reject")).await;
    assert!(ran.error, "the upstream error is visible: {}", ran.body);
    let card = shown_turn(app, &main, turn).await["questions"][0].clone();
    assert_eq!(card["used"]["outcome"], "error", "{card}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approved_call_records_what_its_tool_returned_once() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let pending = tool(app, &token, "computer_run", delete_args())
        .await
        .result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    let approved = answer(app, &main, question, "approve_forever").await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let token = h.continued(turn, &token).await;

    let ran = tool(app, &token, "computer_run", delete_args()).await;
    assert!(
        ran.result
            .as_str()
            .is_some_and(|t| t.starts_with("COMPUTER_UNAVAILABLE")),
        "the call under the standing grant failed before running: {}",
        ran.body
    );
    let card = shown_turn(app, &main, turn).await["questions"][0].clone();
    assert_eq!(
        card["used"]["outcome"], "error",
        "a broad grant still marks its own card, with the tool's error: {card}"
    );

    let again = else_api::db::owner_questions::record_use(
        &app.db.pool,
        question,
        else_api::db::owner_questions::UseOutcome::Completed,
    )
    .await
    .unwrap();
    assert!(!again, "a recorded outcome is final");
    let card = shown_turn(app, &main, turn).await["questions"][0].clone();
    assert_eq!(card["used"]["outcome"], "error");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_action_stays_denied_for_the_request() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let pending = tool(app, &token, "computer_run", send_args()).await.result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    let denied = answer(app, &main, question, "deny").await;
    assert_eq!(denied.status, StatusCode::OK, "{}", denied.body);
    let token = h.continued(turn, &token).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("answer=\"deny\""), "{packet}");
    assert!(
        packet.contains("Denied: do not do this external action"),
        "{packet}"
    );

    let retry = tool(app, &token, "computer_run", send_args()).await;
    assert!(retry.error);
    assert!(
        retry.result.as_str().unwrap().starts_with("DENIED"),
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
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let pending = tool(app, &token, "computer_run", delete_args())
        .await
        .result;
    assert_eq!(pending["kind"], "destructive");
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    let card = shown_turn(app, &main, turn).await["questions"][0].clone();
    assert_eq!(card["choices"][2]["id"], "approve_forever");
    assert_eq!(card["title"], "Run a command in the bot's computer");

    let forever = answer(app, &main, question, "approve_forever").await;
    assert_eq!(forever.status, StatusCode::OK, "{}", forever.body);
    assert_eq!(permissions(app, &main).await, 1);
    let listed = app
        .owner("GET", &format!("/v1/bots/{main}/permissions"), None)
        .await;
    assert_eq!(listed.body["permissions"][0]["scope"], "destructive");
    wait(app, turn, "completed").await;

    let (turn, token) = h.running(&main, GATE).await;
    let other_delete = json!({ "command": "rm -rf ~/work/cache", "effect": "destructive" });
    let ran = tool(app, &token, "computer_run", other_delete.clone()).await;
    assert!(
        ran.result
            .as_str()
            .is_some_and(|t| t.starts_with("COMPUTER_UNAVAILABLE")),
        "{}",
        ran.body
    );
    let host = tool(
        app,
        &token,
        "ask_permission",
        json!({ "kind": "destructive", "action": "rm -rf ~/old", "reason": "Clean up" }),
    )
    .await
    .result;
    assert_eq!(host["status"], "approved");
    assert_eq!(host["standing"], true);
    let external = tool(app, &token, "computer_run", send_args()).await.result;
    assert_eq!(external["status"], "pending", "{external}");
    h.release(turn).await;
    wait(app, turn, "waiting").await;

    let revoked = app
        .owner(
            "DELETE",
            &format!("/v1/bots/{main}/permissions/destructive"),
            None,
        )
        .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    assert_eq!(permissions(app, &main).await, 0);
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success());
    wait(app, turn, "interrupted").await;
    let (turn, token) = h.running(&main, GATE).await;
    let asks = tool(app, &token, "computer_run", other_delete).await.result;
    assert_eq!(asks["status"], "pending", "revoked: asks again");
    h.release(turn).await;
    wait(app, turn, "waiting").await;
}

fn unfollow_args(accounts: &str, target: &str, effect: &str) -> Value {
    json!({ "command": format!("node unfollow.js {accounts}"), "effect": effect,
            "purpose": "Unfollow people who have not replied in DM after 14 days",
            "task_scope": { "task": "Unfollow X accounts that never replied", "target": target } })
}

fn choice_ids(card: &Value) -> Vec<String> {
    card["choices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scoped_computer_call_can_be_always_allowed_for_its_task_and_target() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = "X account @proliquid_xyz";
    let (turn, token) = h.running(&main, GATE).await;

    let mut malformed = unfollow_args("@a", account, "external");
    malformed["task_scope"]["target"] = json!(" ");
    let refused = tool(app, &token, "computer_run", malformed).await;
    assert!(refused.error, "{}", refused.body);
    assert!(
        refused
            .result
            .as_str()
            .unwrap()
            .starts_with("task_scope is {task, target}"),
        "{}",
        refused.body
    );

    let pending = tool(
        app,
        &token,
        "computer_run",
        unfollow_args("@UST_mean @rahulxero", account, "external"),
    )
    .await
    .result;
    assert_eq!(pending["status"], "pending", "{pending}");
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    let card = shown_turn(app, &main, turn).await["questions"][0].clone();
    assert_eq!(choice_ids(&card), ["approve", "deny", "approve_task"]);
    assert_eq!(card["action"]["tool"], "computer_run");
    assert_eq!(card["action"]["task_scope"]["target"], account);

    let always = answer(app, &main, question, "approve_task").await;
    assert_eq!(always.status, StatusCode::OK, "{}", always.body);
    let listed = app
        .owner("GET", &format!("/v1/bots/{main}/permissions"), None)
        .await;
    assert_eq!(
        listed.body["permissions"][0]["target"],
        json!({ "scope": "task", "task": "Unfollow X accounts that never replied",
                "target": account, "kind": "external", "covers": "computer" }),
        "{}",
        listed.body
    );
    wait(app, turn, "completed").await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("Task scope: {\"task\":\"Unfollow X accounts that never replied\""),
        "{packet}"
    );

    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let ran = tool(
        app,
        &token,
        "computer_run",
        unfollow_args("@someone_else", account, "external"),
    )
    .await;
    assert!(
        ran.result
            .as_str()
            .is_some_and(|t| t.starts_with("COMPUTER_UNAVAILABLE")),
        "a later batch in the saved scope runs without a card: {}",
        ran.body
    );
    let mut turn_token = (turn, token);
    for (other, choices) in [
        (
            unfollow_args("@a", "X account @other", "external"),
            &["approve", "deny", "approve_task"][..],
        ),
        (
            unfollow_args("@a", account, "destructive"),
            &["approve", "deny", "approve_task"][..],
        ),
        (
            json!({ "command": "node unfollow.js @a", "effect": "external" }),
            &["approve", "deny", "approve_tool"][..],
        ),
    ] {
        let (turn, token) = turn_token;
        let asks = tool(app, &token, "computer_run", other.clone())
            .await
            .result;
        assert_eq!(asks["status"], "pending", "{other}: {asks}");
        h.release(turn).await;
        wait(app, turn, "waiting").await;
        let card = shown_turn(app, &main, turn).await["questions"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(choice_ids(&card), choices, "{other}");
        turn_token = (turn, follow(&h, &main, turn, &token).await);
    }
    app.wait_turn(&turn_token.0.to_string(), &["running"]).await;
}

/// Send a follow-up while `turn` waits on a card: it joins the reply,
/// skips the card, and the reply continues. Returns a grant for the
/// continuation.
async fn follow(h: &Harness, bot: &str, turn: Uuid, token: &str) -> String {
    let app = &h.app;
    let sent = app
        .send(bot, Uuid::new_v4(), "Try the next one.", Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["steered_into"], turn.to_string());
    h.continued(turn, token).await
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_records_never_store_typed_text() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
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
        let (turn, token) = h.running(&main, GATE).await;
        let pending = tool(app, &token, name, args).await.result;
        let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
        let snapshot = shown_turn(app, &main, turn).await;
        assert!(!snapshot.to_string().contains(canary));
        assert!(
            !journaled(app, &main, question)
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
        h.release(turn).await;
        wait(app, turn, "waiting").await;
        assert_eq!(
            answer(app, &main, question, "approve").await.status,
            StatusCode::OK
        );
        wait(app, turn, "completed").await;
        let packet = h.packets().await.last().unwrap().clone();
        assert!(packet.contains("[else.bot owner answers]"));
        assert!(!packet.contains(canary), "{packet}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_continues_only_after_all_owner_requests_settle() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let pending = tool(app, &token, "ask_owner", choice_args()).await.result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    let secret = Uuid::new_v4();
    sqlx::query("INSERT INTO secret_requests (id, bot_id, turn_id, attempt, name, reason, env_names)
        SELECT ?1, bot_id, id, attempt, 'login', 'Authenticate', '[\"LOGIN_TOKEN\"]' FROM turns WHERE id = ?2")
        .bind(secret).bind(turn).execute(&app.db.pool).await.unwrap();
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    let declined = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/secret-requests/{secret}/cancel"),
            None,
        )
        .await;
    assert_eq!(declined.status, StatusCode::OK, "{}", declined.body);
    assert_eq!(status(app, turn).await, "waiting");
    assert_eq!(question_row(app, question).await.0, "pending");
    assert_eq!(
        answer(app, &main, question, "a").await.status,
        StatusCode::OK
    );
    wait(app, turn, "completed").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_permission_waits_and_the_answer_names_the_action() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let args = json!({ "kind": "external", "action": "mail -s 'Invoice' client@example.com < invoice.txt",
                       "reason": "Email the invoice to the client" });
    let pending = tool(app, &token, "ask_permission", args.clone())
        .await
        .result;
    assert_eq!(pending["status"], "pending", "{pending}");
    assert_eq!(pending["parked"], true);
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    let card = shown_turn(app, &main, turn).await["questions"][0].clone();
    assert_eq!(card["title"], "Email the invoice to the client");
    assert_eq!(
        card["action"]["host"],
        "mail -s 'Invoice' client@example.com < invoice.txt"
    );
    assert_eq!(card["choices"].as_array().unwrap().len(), 2);

    assert_eq!(
        answer(app, &main, question, "approve").await.status,
        StatusCode::OK
    );
    let token = h.continued(turn, &token).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("Host action: mail -s 'Invoice' client@example.com"),
        "{packet}"
    );
    assert!(
        packet.contains("Approved: do exactly this host action, once"),
        "{packet}"
    );

    let again = tool(app, &token, "ask_permission", args).await.result;
    assert_eq!(again["status"], "pending");
    assert_ne!(again["question_id"], question.to_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_owner_answers_and_only_for_the_asking_bot() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let other = app.create_bot("Home").await;
    let (turn, token) = h.running(&main, GATE).await;
    let bare = h.grant_with(turn, vec![Tool::ReadMemory]).await;
    let refused = tool(app, &bare, "ask_owner", choice_args()).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    let pending = tool(app, &token, "ask_owner", choice_args()).await.result;
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
        answer(app, &other, question, "a").await.status,
        StatusCode::NOT_FOUND,
        "another bot's question looks like none"
    );
    assert_eq!(
        answer(app, &main, Uuid::new_v4(), "a").await.status,
        StatusCode::NOT_FOUND
    );
    let malformed = app
        .owner("POST", &path, Some(json!({ "choice": "a", "note": "x" })))
        .await;
    assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
    assert_eq!(question_row(app, question).await.0, "pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_delegated_for_an_owner_request_asks_open_questions_in_its_bot_chat() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, child, child_token) = delegated_task(&h).await;
    let asked = tool(app, &child_token, "ask_owner", choice_args())
        .await
        .result;
    assert_eq!(asked["parked"], true, "{asked}");
    let shown = shown_turn(app, &leaf, child).await;
    assert_eq!(shown["questions"][0]["status"], "pending", "{shown}");
}

/// A running task the main bot delegated to another bot while serving an
/// owner message: that bot's ID and the task's turn.
async fn delegated_task(h: &Harness) -> (String, Uuid, String) {
    let app = &h.app;
    let main = app.main_bot().await;
    let leaf = app.create_bot("Ops").await;
    let warm = app
        .send(&leaf, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    app.wait_turn(warm.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let (turn, parent) = h.running(&main, GATE).await;
    let delegated = tool(
        app,
        &parent,
        "delegate",
        json!({ "target_bot_id": leaf, "task": format!("{GATE_EVERY_ATTEMPT} clean the server disk now") }),
    )
    .await;
    h.release(turn).await;
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
    let (_, token) = h.started(child).await;
    (leaf, child, token)
}

#[tokio::test(flavor = "multi_thread")]
async fn delegated_tasks_ask_for_approval_in_their_bot_chat_but_cannot_record_a_bot_grant() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, child, child_token) = delegated_task(&h).await;
    let refused = tool(
        app,
        &child_token,
        "confirm_permission",
        json!({ "target": { "scope": "bot" }, "authorization": "Always allow destructive actions" }),
    )
    .await;
    assert!(refused.error, "{}", refused.body);
    assert!(
        refused.result.as_str().unwrap().starts_with("NOT_ALLOWED"),
        "{}",
        refused.body
    );
    let pending = tool(app, &child_token, "computer_run", delete_args())
        .await
        .result;
    let question: Uuid = pending["question_id"]
        .as_str()
        .unwrap_or_else(|| panic!("an approval card: {pending}"))
        .parse()
        .unwrap();
    h.release(child).await;
    wait(app, child, "waiting").await;
    let shown = shown_turn(app, &leaf, child).await;
    let choices: Vec<&str> = shown["questions"][0]["choices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        choices,
        ["approve", "deny"],
        "no standing grant from delegated work"
    );
    assert_eq!(
        shown["questions"][0]["id"],
        question.to_string(),
        "the card shows in the target bot's chat: {shown}"
    );
    assert_eq!(
        answer(app, &leaf, question, "approve").await.status,
        StatusCode::OK
    );
    let resumed = h.continued(child, &child_token).await;
    let ran = tool(app, &resumed, "computer_run", delete_args()).await;
    assert!(
        ran.result
            .as_str()
            .is_some_and(|t| t.starts_with("COMPUTER_UNAVAILABLE")),
        "the approved call runs: {}",
        ran.body
    );
    assert_eq!(question_row(app, question).await.0, "answered");
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_and_restart_settle_or_keep_pending_cards() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;

    let (turn, token) = h.running(&main, GATE).await;
    let pending = tool(app, &token, "ask_owner", choice_args()).await.result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    wait(app, turn, "interrupted").await;
    assert_eq!(question_row(app, question).await.0, "cancelled");
    assert_eq!(
        answer(app, &main, question, "a").await.status,
        StatusCode::CONFLICT
    );

    let (turn, token) = h.running(&main, GATE).await;
    let pending = tool(app, &token, "computer_run", delete_args())
        .await
        .result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(turn).await;
    wait(app, turn, "waiting").await;
    let h = harness(TestApp::builder().db(h.app.stop().await)).await;
    let app = &h.app;
    assert_eq!(
        app.wait_turn(&turn.to_string(), &["waiting"]).await,
        "waiting"
    );
    assert_eq!(question_row(app, question).await.0, "pending");
    assert_eq!(
        shown_turn(app, &main, turn).await["questions"][0]["status"],
        "pending",
        "the card survives a restart"
    );
    let approved = answer(app, &main, question, "approve").await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    wait(app, turn, "completed").await;
    let attempts: i32 = sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(attempts, 2, "the same turn continued after the restart");
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_delegated_task_closes_its_card() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let leaf = app.create_bot("Ops").await;
    let warm = app
        .send(&leaf, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    app.wait_turn(warm.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let (turn, parent) = h.running(&main, GATE).await;
    let delegated = tool(
        app,
        &parent,
        "delegate",
        json!({ "target_bot_id": leaf, "task": format!("{GATE} clean the server disk now") }),
    )
    .await;
    h.release(turn).await;
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
    let pending = tool(app, &child_token, "computer_run", delete_args())
        .await
        .result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(child).await;
    wait(app, child, "waiting").await;
    wait(app, turn, "waiting").await;
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    wait(app, turn, "waiting").await;
    assert_eq!(
        question_row(app, question).await.0,
        "pending",
        "Stop leaves the task and its card"
    );
    let cancel = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/waits/{task}/cancel"),
            None,
        )
        .await;
    assert_eq!(cancel.status, StatusCode::OK, "{}", cancel.body);
    wait(app, child, "interrupted").await;
    assert_eq!(question_row(app, question).await.0, "cancelled");
    assert_eq!(
        answer(app, &leaf, question, "approve").await.status,
        StatusCode::CONFLICT,
        "a stopped request cannot be resumed through its card"
    );
    assert_eq!(status(app, child).await, "interrupted");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_message_in_the_target_chat_skips_a_delegated_card() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let leaf = app.create_bot("Ops").await;
    let warm = app
        .send(&leaf, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    app.wait_turn(warm.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let (turn, parent) = h.running(&main, GATE).await;
    let delegated = tool(
        app,
        &parent,
        "delegate",
        json!({ "target_bot_id": leaf, "task": format!("{GATE} clean the server disk now") }),
    )
    .await;
    h.release(turn).await;
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
    let pending = tool(app, &child_token, "computer_run", delete_args())
        .await
        .result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(child).await;
    wait(app, child, "waiting").await;

    let reply = app
        .send(
            &leaf,
            Uuid::new_v4(),
            "Always approve disk cleanups",
            Some("synthetic"),
        )
        .await;
    assert!(reply.status.is_success(), "{}", reply.body);
    assert_eq!(
        reply.body["turn"]["steered_into"],
        child.to_string(),
        "the message joins the task that asked"
    );
    assert_eq!(
        question_row(app, question).await.0,
        "skipped",
        "the owner answered in chat instead of on the card"
    );
    assert_eq!(
        shown_turn(app, &leaf, child).await["questions"][0]["status"],
        "skipped",
        "the card in the target bot's chat closes"
    );
    assert_eq!(
        answer(app, &leaf, question, "approve").await.status,
        StatusCode::CONFLICT,
        "a skipped card cannot approve"
    );
    wait(app, child, "completed").await;
    app.wait_turn(reply.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    assert_eq!(attempts(app, child).await, 2, "the task continued with it");
}

async fn delegated_card(h: &Harness, leaf: &str) -> (Uuid, Uuid, Uuid, Uuid, String) {
    let app = &h.app;
    let main = app.main_bot().await;
    let warm = app
        .send(leaf, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    app.wait_turn(warm.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let (root, parent) = h.running(&main, GATE).await;
    let delegated = tool(
        app,
        &parent,
        "delegate",
        json!({ "target_bot_id": leaf, "task": format!("{GATE_EVERY_ATTEMPT} publish the patch release") }),
    )
    .await;
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
    let pending = tool(app, &child_token, "computer_run", delete_args())
        .await
        .result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(child).await;
    wait(app, child, "waiting").await;
    h.release(root).await;
    wait(app, root, "waiting").await;
    (root, task, child, question, child_token)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_reply_in_chat_joins_a_delegated_card_and_the_task_reports_back() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let leaf = app.create_bot("Release").await;
    let (root, task, child, question, _) = delegated_card(&h, &leaf).await;

    let follow_up = "Always approve the patch releases.";
    let sent = app
        .send(&leaf, Uuid::new_v4(), follow_up, Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["steered_into"], child.to_string());
    assert_eq!(question_row(app, question).await.0, "skipped");
    let reply = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    h.release(child).await;
    wait(app, child, "completed").await;
    app.wait_turn(&reply, &["completed"]).await;
    wait(app, root, "completed").await;
    assert!(
        h.packets().await.iter().any(|p| p.contains(follow_up)),
        "the task continued with the owner's message"
    );

    let (state, carrier, takeover): (String, Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT status, root_turn_id, takeover_turn_id FROM delegations WHERE id = ?1",
    )
    .bind(task)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(
        (state.as_str(), carrier, takeover),
        ("completed", root, None),
        "the task reports back to the reply that delegated it"
    );
    assert_eq!(
        answer(app, &leaf, question, "approve").await.status,
        StatusCode::CONFLICT,
        "the stale card cannot run the task again"
    );
    assert_eq!(permissions(app, &main).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_task_ends_the_message_that_joined_it() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let leaf = app.create_bot("Release").await;
    let (_, task, child, _, child_token) = delegated_card(&h, &leaf).await;

    let sent = app
        .send(&leaf, Uuid::new_v4(), "Yes, publish it.", Some("synthetic"))
        .await;
    assert_eq!(sent.body["turn"]["steered_into"], child.to_string());
    let reply: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    h.continued(child, &child_token).await;
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/waits/{task}/cancel"),
            None,
        )
        .await;
    assert_eq!(stop.status, StatusCode::OK, "{}", stop.body);
    wait(app, child, "interrupted").await;
    wait(app, reply, "interrupted").await;
    let state: String = sqlx::query_scalar("SELECT status FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(state, "cancelled", "a stopped request stays stopped");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_chat_message_leaves_the_task_its_first_message_joined() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let leaf = app.create_bot("Release").await;
    let (root, task, child, _, child_token) = delegated_card(&h, &leaf).await;

    let first = app
        .send(&leaf, Uuid::new_v4(), "Yes, publish it.", Some("synthetic"))
        .await;
    assert_eq!(first.body["turn"]["steered_into"], child.to_string());
    let first: Uuid = first.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    h.continued(child, &child_token).await;
    let correction = "Publish it as a patch release.";
    let second = app
        .send(&leaf, Uuid::new_v4(), correction, Some("synthetic"))
        .await;
    assert_eq!(
        second.body["turn"]["steered_into"],
        Value::Null,
        "the task no longer waits on the owner, so the message runs on its own"
    );
    h.release(child).await;
    app.wait_turn(second.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    wait(app, first, "completed").await;
    wait(app, root, "completed").await;
    let state: String = sqlx::query_scalar("SELECT status FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(state, "completed");
    assert!(
        h.packets()
            .await
            .iter()
            .any(|p| p.contains("Yes, publish it.")),
        "the task continued with the first message"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follow_up_to_the_delegating_bot_joins_the_reply_that_receives_the_task() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let leaf = app.create_bot("Telegram").await;
    let (root, task, child, _, child_token) = delegated_card(&h, &leaf).await;

    let approval = app
        .send(&leaf, Uuid::new_v4(), "Yes, publish it.", Some("synthetic"))
        .await;
    assert_eq!(approval.body["turn"]["steered_into"], child.to_string());
    let approval: Uuid = approval.body["turn"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    h.continued(child, &child_token).await;
    let follow = app
        .send(
            &main,
            Uuid::new_v4(),
            "Sorry, I meant Discord.",
            Some("synthetic"),
        )
        .await;
    assert_eq!(follow.status, StatusCode::ACCEPTED, "{}", follow.body);
    assert_eq!(follow.body["turn"]["steered_into"], root.to_string());
    let follow: Uuid = follow.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    h.release(child).await;
    wait(app, approval, "completed").await;
    wait(app, root, "completed").await;
    wait(app, follow, "completed").await;

    let (state, carrier, delivered): (String, Uuid, Option<i64>) = sqlx::query_as(
        "SELECT status, root_turn_id, delivered_attempt FROM delegations WHERE id = ?1",
    )
    .bind(task)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(state, "completed", "the task still reports back");
    assert_eq!(
        carrier, root,
        "the task stays with the reply that delegated it"
    );
    assert!(delivered.is_some(), "the reply received the outcome");
}
