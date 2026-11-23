//! Questions and approvals a delegated task raises in its bot's chat.

use super::*;

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
    app.wait_turn(child, &["waiting"]).await;
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
    app.wait_turn(child, &["waiting"]).await;
    app.wait_turn(turn, &["waiting"]).await;
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    app.wait_turn(turn, &["waiting"]).await;
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
    app.wait_turn(child, &["interrupted"]).await;
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
    app.wait_turn(child, &["waiting"]).await;

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
    app.wait_turn(child, &["completed"]).await;
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
    app.wait_turn(child, &["waiting"]).await;
    h.release(root).await;
    app.wait_turn(root, &["waiting"]).await;
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
    app.wait_turn(child, &["completed"]).await;
    app.wait_turn(&reply, &["completed"]).await;
    app.wait_turn(root, &["completed"]).await;
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
    app.wait_turn(child, &["interrupted"]).await;
    app.wait_turn(reply, &["interrupted"]).await;
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
    app.wait_turn(first, &["completed"]).await;
    app.wait_turn(root, &["completed"]).await;
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
    app.wait_turn(approval, &["completed"]).await;
    app.wait_turn(root, &["completed"]).await;
    app.wait_turn(follow, &["completed"]).await;

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

/// A task's card shows only in the chat of the bot doing the task: a
/// message in the delegating chat joins the reply that delegated and
/// leaves the card open there.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_in_the_delegating_chat_leaves_the_tasks_card() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let leaf = app.create_bot("Release").await;
    let (root, _, child, question, _) = delegated_card(&h, &leaf).await;

    let sent = app
        .send(
            &main,
            Uuid::new_v4(),
            "Also tag the release notes.",
            Some("synthetic"),
        )
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["steered_into"], root.to_string());
    assert_eq!(question_row(app, question).await.0, "pending");
    assert_eq!(
        shown_turn(app, &leaf, child).await["questions"][0]["status"],
        "pending"
    );
    assert_eq!(sidebar_status(app, &leaf).await["kind"], "needs_approval");
}
