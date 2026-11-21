//! Calls that wait for the owner's approval, and always-allow grants.

use super::*;

fn send_args() -> Value {
    json!({ "command": "node send.js --all", "effect": "external", "secrets": ["smtp"],
            "purpose": "Send the newsletter to 120 subscribers",
            "context": "To: newsletter subscribers\nDraft: Hello everyone, this month's update is ready." })
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
        else_api::app::questions::ApprovalKind::External,
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
    app.wait_turn(turn, &["waiting"]).await;
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
    installed.ok();
    let pending = tool(app, &token, "mcp_request", call.clone()).await.result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    assert_eq!(
        answer(app, bot, question, "approve").await.status,
        StatusCode::OK
    );
    let token = h.continued(turn, &token).await;
    let card = shown_turn(app, bot, turn).await["questions"][0].clone();
    assert_eq!(card["used"], Value::Null, "approved but not yet run");
    let ran = tool(app, &token, "mcp_request", call).await;
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
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
    ran.ok();
    let card = shown_turn(app, &main, turn).await["questions"][0].clone();
    assert_eq!(card["used"]["outcome"], "completed", "{card}");
    let ran_at = card["used"]["at"]
        .as_str()
        .and_then(|at| at.parse::<chrono::DateTime<chrono::Utc>>().ok())
        .unwrap_or_else(|| panic!("the card records when the call ran: {card}"));
    assert_eq!(journaled(app, &main, question).await["used"], card["used"]);

    // End the turn as a provider policy failure does.
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
    app.wait_turn(turn, &["waiting"]).await;
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
    app.wait_turn(turn, &["waiting"]).await;
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
    app.wait_turn(turn, &["waiting"]).await;
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
    app.wait_turn(turn, &["completed"]).await;

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
    app.wait_turn(turn, &["waiting"]).await;

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
    app.wait_turn(turn, &["interrupted"]).await;
    let (turn, token) = h.running(&main, GATE).await;
    let asks = tool(app, &token, "computer_run", other_delete).await.result;
    assert_eq!(asks["status"], "pending", "revoked: asks again");
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
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
    app.wait_turn(turn, &["waiting"]).await;
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
    app.wait_turn(turn, &["completed"]).await;
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
        app.wait_turn(turn, &["waiting"]).await;
        let card = shown_turn(app, &main, turn).await["questions"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(choice_ids(&card), choices, "{other}");
        turn_token = (turn, follow(&h, &main, turn, &token).await);
    }
    app.wait_turn(turn_token.0, &["running"]).await;
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
        app.wait_turn(turn, &["waiting"]).await;
        assert_eq!(
            answer(app, &main, question, "approve").await.status,
            StatusCode::OK
        );
        app.wait_turn(turn, &["completed"]).await;
        let packet = h.packets().await.last().unwrap().clone();
        assert!(packet.contains("[else.bot owner answers]"));
        assert!(!packet.contains(canary), "{packet}");
    }
}
