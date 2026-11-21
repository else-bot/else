//! Drafts raised by delegated tasks and continued replies, and the sidebar
//! status they give their bot.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_approval_of_a_delegated_draft_answers_the_delegating_request() {
    chat_approval_of_a_delegated_draft(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_approval_before_the_task_parks_still_answers_the_delegating_request() {
    chat_approval_of_a_delegated_draft(false).await;
}

async fn chat_approval_of_a_delegated_draft(parked: bool) {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, root, task, child) = delegated_task(&h).await;
    let (_, token) = h.started(child).await;
    let id = draft(app, &token, lunch()).await;
    if parked {
        h.release(child).await;
        app.wait_turn(child, &["waiting"]).await;
    }

    // Before it parks, the message waits for the task's attempt to end,
    // which then continues at once instead of parking.
    let (reply, token) = joined(
        &h,
        &leaf,
        child,
        &token,
        "Looks good, send it.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let takeover: Option<Uuid> =
        sqlx::query_scalar("SELECT takeover_turn_id FROM delegations WHERE id = ?1")
            .bind(task)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(takeover, None, "the task keeps its own turn");
    assert_eq!(attempt(app, child).await, 2);

    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    h.release(child).await;
    app.wait_turn(child, &["completed"]).await;
    app.wait_turn(reply, &["completed"]).await;
    let outcome: String = sqlx::query_scalar("SELECT status FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(outcome, "completed");
    assert_ne!(
        status(app, root).await,
        "waiting",
        "the delegating request continues"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_continued_reply_ends_the_message_then_the_reply_closes_its_card() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, family_message()).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;

    let (reply, _) = joined(
        &h,
        &main,
        first,
        &token,
        "Offer me a choice of channels instead.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let card = shown(app, &main, first).await[0].clone();
    assert_eq!(card["status"], "pending", "the reply may still confirm it");
    assert_eq!(card["in_reply"], true, "{card}");

    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    app.wait_turn(reply, &["interrupted"]).await;
    app.wait_turn(first, &["waiting"]).await;
    let card = shown(app, &main, first).await[0].clone();
    assert_eq!(
        card["status"], "pending",
        "Stop ends the attempt, not its card"
    );
    assert_eq!(card["in_reply"], false, "{card}");

    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    app.wait_turn(first, &["interrupted"]).await;
    let card = journaled(app, &main, id).await;
    assert_eq!(
        card["status"], "discarded",
        "once the reply and its message have both ended, the card closes: {card}"
    );
    assert_eq!(card["can_approve"], false, "{card}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_that_moves_on_keeps_its_card_for_the_owner() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, family_message()).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;
    let (done, _) = joined(
        &h,
        &main,
        first,
        &token,
        "Make it a poll instead.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;
    assert_eq!(status(app, done).await, "queued", "it ends with its reply");
    let card = shown(app, &main, first).await[0].clone();
    assert_eq!(card["status"], "pending", "{card}");
    assert_eq!(card["can_approve"], true, "the reply still waits on it");

    let discarded = owner_action(app, &main, id, "discard", json!({})).await;
    assert_eq!(discarded.status, StatusCode::OK, "{}", discarded.body);
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;
    app.wait_turn(done, &["completed"]).await;
}

async fn sidebar_status(app: &TestApp, bot: &str) -> Value {
    let list = app.owner("GET", "/v1/bots", None).await;
    let mut status = list.body["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == bot)
        .unwrap()["sidebar_status"]
        .clone();
    if let Some(fields) = status.as_object_mut() {
        assert!(fields.remove("since").is_some(), "{status}");
    }
    status
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_draft_marks_its_bot_as_needing_approval_until_approved() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, _, _, child) = delegated_task(&h).await;
    let (_, token) = h.started(child).await;
    let id = draft(app, &token, family_message()).await;
    h.release(child).await;
    app.wait_turn(child, &["waiting"]).await;
    assert_eq!(
        sidebar_status(app, &leaf).await,
        json!({ "kind": "needs_approval", "preview": "Telegram message to Family group" }),
        "the card in the bot's chat waits on the owner"
    );

    let approved = owner_action(app, &leaf, id, "approve", json!({ "version": 1 })).await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_ne!(
        sidebar_status(app, &leaf).await["kind"],
        "needs_approval",
        "an approved draft no longer waits on the owner"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_question_marks_its_bot_as_waiting_for_the_owner() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, _, _, child) = delegated_task(&h).await;
    let (_, token) = h.started(child).await;
    let asked = tool(
        app,
        &token,
        "ask_owner",
        json!({ "question": "Which account?", "choices": ["Main", "Alt"] }),
    )
    .await;
    assert_eq!(asked.result["status"], "pending", "{}", asked.body);
    h.release(child).await;
    app.wait_turn(child, &["waiting"]).await;
    assert_eq!(
        sidebar_status(app, &leaf).await,
        json!({ "kind": "waiting_for_answer", "preview": "Which account?" }),
        "the card in the bot's chat waits on the owner"
    );
}
