//! Waits inside delegated tasks.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_task_waits_on_a_job_and_its_delegator_resumes_only_when_it_finishes() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let dev = app.create_bot("Dev").await;
    sqlx::query("UPDATE bots SET selected_provider_id = 'synthetic' WHERE id = ?1")
        .bind(dev.parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
    let (root, token) = h.running(&main, GATE).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    installed.ok();
    let task = format!("{GATE} Run WEB-284 in T3, review its PR, and report back.");
    let accepted = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": dev, "task": task }),
    )
    .await
    .result;
    assert_eq!(accepted["status"], "pending", "{accepted}");
    let (delegation, child): (Uuid, Uuid) =
        sqlx::query_as("SELECT id, child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    let (_, worker) = h.started(child).await;
    let parked = tool(app, &worker, "mcp_wait", wait_args()).await.result;
    assert_eq!(parked["parked"], true, "{parked}");
    assert!(
        parked["next"].as_str().unwrap().contains("without a reply"),
        "{parked}"
    );
    h.release(child).await;
    app.wait_turn(child, &["waiting"]).await;
    h.release(root).await;
    app.wait_turn(root, &["waiting"]).await;
    let pending: String = sqlx::query_scalar("SELECT status FROM delegations WHERE id = ?1")
        .bind(delegation)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(pending, "running", "parking does not settle the delegation");

    let owner = app
        .send(
            &dev,
            Uuid::new_v4(),
            "What is on your plate?",
            Some("synthetic"),
        )
        .await;
    assert_eq!(owner.status, StatusCode::ACCEPTED, "{}", owner.body);
    app.wait_turn(owner.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    assert_eq!(
        app.wait_turn(child, &["waiting"]).await,
        "waiting",
        "the owner's work ran while the task kept waiting"
    );
    assert_eq!(attempts(app, root).await, 1);

    std::fs::write(&state, "result_available").unwrap();
    check_now(app).await;
    app.wait_turn(child, &["completed"]).await;
    assert_eq!(attempts(app, child).await, 2, "the same task continued");
    assert_eq!(wait_row(app, child).await.0, "finished");
    app.wait_turn(root, &["completed"]).await;
    assert_eq!(attempts(app, root).await, 2, "the delegator resumed once");
    let (status, delivered): (String, Option<i32>) =
        sqlx::query_as("SELECT status, delivered_attempt FROM delegations WHERE id = ?1")
            .bind(delegation)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), delivered), ("completed", Some(2)));
    let packets = h.packets().await;
    assert!(
        packets
            .iter()
            .any(|p| p.contains("[else.bot MCP waits]") && p.contains("Opened PR #775")),
        "the task's continuation got the job's result"
    );
    assert!(
        packets
            .last()
            .unwrap()
            .contains("[else.bot delegation results]"),
        "{:?}",
        packets.last()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_settled_wait_continues_a_reply_whose_task_still_runs() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let dev = app.create_bot("Dev").await;
    sqlx::query("UPDATE bots SET selected_provider_id = 'synthetic' WHERE id = ?1")
        .bind(dev.parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
    let (root, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    installed.ok();
    let task = json!({ "target_bot_id": dev, "task": "[synthetic:hang] Audit the billing code." });
    let accepted = tool(app, &token, "delegate", task).await;
    assert_eq!(accepted.result["status"], "pending", "{}", accepted.body);
    h.release(root).await;
    app.wait_turn(root, &["waiting"]).await;

    follow(app, &main, root).await;
    let token = h.continued(root, &token).await;
    assert_eq!(attempts(app, root).await, 2);
    let task_root: Uuid =
        sqlx::query_scalar("SELECT root_turn_id FROM delegations WHERE status = 'running'")
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(task_root, root, "the task stays with its reply");
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(root).await;
    rests_after(
        app,
        root,
        2,
        "waiting",
        "the reply parks on its task and wait",
    )
    .await;

    std::fs::write(&state, "result_available").unwrap();
    check_now(app).await;
    h.continued(root, &token).await;
    h.release(root).await;
    rests_after(
        app,
        root,
        3,
        "waiting",
        "the settled wait continues the reply while its task still runs",
    )
    .await;
    assert_eq!(wait_row(app, root).await.1, Some(3));
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("Opened PR #775"), "{packet}");
}
