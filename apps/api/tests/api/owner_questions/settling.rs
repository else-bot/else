//! When a turn continues, who may answer, and what a stop or restart does to
//! open cards.

use super::*;

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
    app.wait_turn(turn, &["waiting"]).await;
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
    app.wait_turn(turn, &["completed"]).await;
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
    app.wait_turn(turn, &["waiting"]).await;
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
async fn stop_and_restart_settle_or_keep_pending_cards() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;

    let (turn, token) = h.running(&main, GATE).await;
    let pending = tool(app, &token, "ask_owner", choice_args()).await.result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    app.wait_turn(turn, &["interrupted"]).await;
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
    app.wait_turn(turn, &["waiting"]).await;
    let h = harness(TestApp::builder().db(h.app.stop().await)).await;
    let app = &h.app;
    assert_eq!(app.wait_turn(turn, &["waiting"]).await, "waiting");
    assert_eq!(question_row(app, question).await.0, "pending");
    assert_eq!(
        shown_turn(app, &main, turn).await["questions"][0]["status"],
        "pending",
        "the card survives a restart"
    );
    let approved = answer(app, &main, question, "approve").await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    app.wait_turn(turn, &["completed"]).await;
    let attempts: i32 = sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(attempts, 2, "the same turn continued after the restart");
}
