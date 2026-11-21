//! The owner stopping a waiting reply or one of its waits, and waits across a restart.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_wait_survives_a_restart_during_and_after_the_owners_stop() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    installed.ok();
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    // A restart lands while the owner's Stop unwinds a reply that carries
    // this wait: recovery parks the turn on it instead of cancelling it.
    sqlx::query("UPDATE turns SET status = 'cancelling', error_code = 'stopped' WHERE id = ?1")
        .bind(turn)
        .execute(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(wait_row(app, turn).await.0, "pending");

    let db = h.app.stop().await;
    let h = harness(TestApp::builder().db(db)).await;
    let app = &h.app;
    assert_eq!(app.wait_turn(turn, &["waiting"]).await, "waiting");
    assert_eq!(wait_row(app, turn).await.0, "pending");
    let stopped = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert_eq!(stopped.status, StatusCode::OK, "{}", stopped.body);
    common::never_polled(
        "Stop settles the MCP wait",
        Duration::from_secs(1),
        || async {
            let status = wait_row(app, turn).await.0;
            (status != "pending").then_some(status)
        },
    )
    .await;
    assert_eq!(
        app.wait_turn(turn, &["waiting"]).await,
        "waiting",
        "Stop ends generation only; the turn keeps waiting"
    );
    std::fs::write(&state, "result_available").unwrap();
    check_now(app).await;
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await.0, "finished");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owner_stops_one_wait_and_the_bot_learns_it_was_cancelled() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    installed.ok();
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    std::fs::write(&state, "result_available").unwrap();
    check_now(app).await;
    app.wait_turn(turn, &["completed"]).await;
    let finished: Uuid = sqlx::query_scalar("SELECT id FROM mcp_waits WHERE turn_id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();

    std::fs::write(&state, "working").unwrap();
    let (turn, token) = h.running(&main, GATE).await;
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let shown = shown_waits(app, &main, turn).await;
    assert_eq!(
        shown[0]["purpose"], "Review the WEB-284 PR once T3 finishes.",
        "{shown}"
    );
    assert_eq!(shown[0]["last_status"], "working", "{shown}");
    let wait = shown[0]["id"].as_str().unwrap().to_owned();
    let cancel = |bot: String, id: String| async move {
        app.owner("POST", &format!("/v1/bots/{bot}/waits/{id}/cancel"), None)
            .await
    };
    let first = cancel(main.clone(), wait.clone()).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(
        (&first.body["kind"], &first.body["created"]),
        (&json!("mcp"), &json!(true))
    );
    app.wait_turn(turn, &["completed"]).await;
    let (status, delivered, _) = wait_row(app, turn).await;
    assert_eq!((status.as_str(), delivered), ("stopped", Some(2)));
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("status=\"stopped\""), "{packet}");
    assert!(packet.contains("cancelled by owner"), "{packet}");

    let again = cancel(main.clone(), wait).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(again.body["created"], false, "a retry returns the stop");
    let ended = cancel(main.clone(), finished.to_string()).await;
    assert_eq!(ended.status, StatusCode::CONFLICT, "{}", ended.body);
    assert_eq!(ended.body["error"]["code"], "wait_ended", "{}", ended.body);
    let other = app.create_bot("Scout").await;
    let foreign = cancel(other, finished.to_string()).await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND, "{}", foreign.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_continued_reply_keeps_its_waits() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    installed.ok();
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;

    let next = follow(app, &main, first).await;
    let token = h.continued(first, &token).await;
    assert_eq!(attempts(app, first).await, 2);
    let stopped = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stopped.status.is_success(), "{}", stopped.body);
    app.wait_turn(first, &["waiting"]).await;
    assert_eq!(turn_status(app, next).await, "interrupted");
    assert_eq!(carried(app, first).await, vec![(1, "pending".into())]);

    std::fs::write(&state, "result_available").unwrap();
    check_now(app).await;
    h.continued(first, &token).await;
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;
    assert_eq!(attempts(app, first).await, 3);
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("Opened PR #775"), "{packet}");
}

async fn turn_status(app: &TestApp, turn: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}
