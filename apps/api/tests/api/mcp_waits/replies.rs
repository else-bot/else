//! Waits a reply carries: follow-ups that join it, and waits that settle one at a time.

use super::*;

fn wait_on(task: &str, purpose: &str) -> Value {
    let mut args = wait_args();
    args["arguments"]["taskId"] = json!(task);
    args["purpose"] = json!(purpose);
    args
}

#[tokio::test(flavor = "multi_thread")]
async fn follow_ups_join_the_reply_and_each_of_its_waits_reports_once() {
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

    let second = follow(app, &main, first).await;
    let token = h.continued(first, &token).await;
    assert_eq!(attempts(app, first).await, 2);
    let deploy = wait_on("WEB-286", "Check the WEB-286 deploy.");
    let parked = tool(app, &token, "mcp_wait", deploy).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(first).await;
    rests_after(app, first, 2, "waiting", "the reply parks on both waits").await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("Any news on WEB-284?"), "{packet}");

    let latest = follow(app, &main, first).await;
    let token = h.continued(first, &token).await;
    assert_eq!(attempts(app, first).await, 3);
    let merge = wait_on("WEB-285", "Merge WEB-285 once T3 finishes.");
    let parked = tool(app, &token, "mcp_wait", merge).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(first).await;
    rests_after(app, first, 3, "waiting", "the reply parks on three waits").await;
    let shown = shown_waits(app, &main, first).await;
    assert_eq!(shown.as_array().map(Vec::len), Some(3), "{shown}");

    std::fs::write(dir.path().join("job.WEB-285"), "result_available").unwrap();
    check_now(app).await;
    let token = h.continued(first, &token).await;
    h.release(first).await;
    rests_after(
        app,
        first,
        4,
        "waiting",
        "one result continues the reply while its other waits still run",
    )
    .await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("Merge WEB-285"), "{packet}");
    assert!(packet.contains("Opened PR #775"), "{packet}");
    assert_eq!(
        carried(app, first).await,
        vec![
            (1, "pending".into()),
            (2, "pending".into()),
            (3, "finished".into())
        ]
    );

    std::fs::write(dir.path().join("job.WEB-284"), "result_available").unwrap();
    check_now(app).await;
    let token = h.continued(first, &token).await;
    h.release(first).await;
    rests_after(app, first, 5, "waiting", "one wait is still pending").await;
    std::fs::write(dir.path().join("job.WEB-286"), "result_available").unwrap();
    check_now(app).await;
    h.continued(first, &token).await;
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;
    for message in [second, latest] {
        app.wait_turn(message, &["completed"]).await;
        assert_eq!(
            attempts(app, message).await,
            0,
            "a joined message never runs"
        );
    }
    let delivered: Vec<(String, Option<i32>)> = sqlx::query_as(
        "SELECT status, delivered_attempt FROM mcp_waits WHERE turn_id = ?1 ORDER BY created_at",
    )
    .bind(first)
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert!(
        delivered
            .iter()
            .all(|(status, at)| status == "finished" && at.is_some()),
        "{delivered:?}"
    );
    let packets = h.packets().await;
    for purpose in [
        "Review the WEB-284 PR",
        "Check the WEB-286 deploy",
        "Merge WEB-285",
    ] {
        assert_eq!(
            packets
                .iter()
                .filter(|p| p.contains(&format!("Purpose: {purpose}")))
                .count(),
            1,
            "each result arrives once: {purpose}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn each_wait_that_settles_continues_the_reply_while_the_others_run() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (latest, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    installed.ok();
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(latest).await;
    app.wait_turn(latest, &["waiting"]).await;
    follow(app, &main, latest).await;
    let token = h.continued(latest, &token).await;
    assert_eq!(attempts(app, latest).await, 2);
    let deploy = wait_on("WEB-286", "Check the WEB-286 deploy.");
    let parked = tool(app, &token, "mcp_wait", deploy).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(latest).await;
    rests_after(app, latest, 2, "waiting", "the reply parks on two waits").await;
    follow(app, &main, latest).await;
    let token = h.continued(latest, &token).await;
    assert_eq!(attempts(app, latest).await, 3);
    let merge = wait_on("WEB-285", "Merge WEB-285 once T3 finishes.");
    let parked = tool(app, &token, "mcp_wait", merge).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(latest).await;
    rests_after(app, latest, 3, "waiting", "the reply parks on three waits").await;

    std::fs::write(dir.path().join("job.WEB-286"), "result_available").unwrap();
    check_now(app).await;
    let token = h.continued(latest, &token).await;
    h.release(latest).await;
    rests_after(
        app,
        latest,
        4,
        "waiting",
        "a wait that settles continues the reply while the others run",
    )
    .await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("Purpose: Check the WEB-286 deploy"),
        "{packet}"
    );
    assert!(
        packet.contains("Merge WEB-285 once T3 finishes.")
            && packet.contains("Review the WEB-284 PR once T3 finishes."),
        "the continuation lists the waits still running: {packet}"
    );

    std::fs::write(dir.path().join("job.WEB-284"), "result_available").unwrap();
    check_now(app).await;
    let token = h.continued(latest, &token).await;
    h.release(latest).await;
    rests_after(
        app,
        latest,
        5,
        "waiting",
        "a continuation that parked nothing still wakes on the next wait",
    )
    .await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("Purpose: Review the WEB-284 PR"),
        "{packet}"
    );

    std::fs::write(dir.path().join("job.WEB-285"), "result_available").unwrap();
    check_now(app).await;
    h.continued(latest, &token).await;
    h.release(latest).await;
    rests_after(app, latest, 6, "completed", "the last wait ends the reply").await;
    assert_eq!(
        sqlx::query_as::<_, (i32, String, Option<i32>)>(
            "SELECT attempt, status, delivered_attempt FROM mcp_waits
             WHERE turn_id = ?1 ORDER BY created_at",
        )
        .bind(latest)
        .fetch_all(&app.db.pool)
        .await
        .unwrap(),
        vec![
            (1, "finished".into(), Some(5)),
            (2, "finished".into(), Some(4)),
            (3, "finished".into(), Some(6)),
        ]
    );
    let packets = h.packets().await;
    for purpose in [
        "Review the WEB-284 PR",
        "Check the WEB-286 deploy",
        "Merge WEB-285",
    ] {
        assert_eq!(
            packets
                .iter()
                .filter(|p| p.contains(&format!("Purpose: {purpose}")))
                .count(),
            1,
            "each result arrives once: {purpose}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn upgrading_continues_a_turn_whose_settled_carried_wait_its_own_wait_stranded() {
    let db = TestDb::empty().await;
    let before = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            else_api::db::MIGRATOR
                .migrations
                .iter()
                .filter(|m| m.version < 65)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    before.run(&db.pool).await.unwrap();
    let (bot, conversation) = (Uuid::new_v4(), Uuid::new_v4());
    let (stranded, waiting) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO bots (id, name) VALUES (?1, 'Old')")
        .bind(bot)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations (id, bot_id, last_turn_no) VALUES (?1, ?2, 2)")
        .bind(conversation)
        .bind(bot)
        .execute(&db.pool)
        .await
        .unwrap();
    for (no, turn) in [(1, waiting), (2, stranded)] {
        sqlx::query(
            "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
                 owner_item_id, provider_id, status, attempt)
             VALUES (?1, ?2, ?3, ?4, zeroblob(32), ?5, ?6, 'synthetic', 'waiting', 1)",
        )
        .bind(turn)
        .bind(bot)
        .bind(conversation)
        .bind(Uuid::new_v4())
        .bind(no)
        .bind(Uuid::new_v4())
        .execute(&db.pool)
        .await
        .unwrap();
    }
    for (turn, attempt, status) in [
        (waiting, 1, "pending"),
        (stranded, 1, "pending"),
        (stranded, 0, "finished"),
    ] {
        sqlx::query(
            "INSERT INTO mcp_waits (id, bot_id, turn_id, attempt, mcp_id, tool, arguments,
                 status_pointer, running_values, purpose, interval_seconds, deadline_at,
                 next_check_at, status)
             VALUES (?1, ?2, ?3, ?4, 't3-code', 'task_status', '{}', '/workState', '[]',
                 'Review the PR.', 15, '2999-01-01T00:00:00.000+00:00',
                 '2999-01-01T00:00:00.000+00:00', ?5)",
        )
        .bind(Uuid::new_v4())
        .bind(bot)
        .bind(turn)
        .bind(attempt)
        .bind(status)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    else_api::db::MIGRATOR.run(&db.pool).await.unwrap();
    let statuses: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, status FROM turns ORDER BY turn_no")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        statuses,
        vec![
            (waiting, "waiting".to_owned()),
            (stranded, "queued".to_owned())
        ],
        "only the turn with an undelivered result continues"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follow_up_that_waits_again_on_the_same_job_keeps_the_one_wait() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("job");
    std::fs::write(&state, "working").unwrap();
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let installed = tool(app, &token, "mcp_install", install(&state)).await;
    installed.ok();
    let parked = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;

    follow(app, &main, first).await;
    let token = h.continued(first, &token).await;
    let again = tool(app, &token, "mcp_wait", wait_args()).await;
    assert_eq!(again.result["status"], "already_waiting", "{}", again.body);
    assert_eq!(again.result["wait_id"], parked.result["wait_id"]);
    assert_eq!(again.result["parked"], false, "{}", again.body);
    h.release(first).await;
    rests_after(
        app,
        first,
        2,
        "waiting",
        "the reply parks on its first wait",
    )
    .await;
    assert_eq!(carried(app, first).await, vec![(1, "pending".into())]);
}
