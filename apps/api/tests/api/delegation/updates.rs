//! A delegating bot that updates or cancels a running task.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_delegating_bot_updates_a_running_task_and_gets_the_updated_reply() {
    let h = harness(TestApp::builder().max_active(3)).await;
    let app = &h.app;
    let work = leaf(app, "Work").await;
    let original = "[synthetic:hang] Research every school form due this month.";
    let (task, first, token) = task_carried_to_a_follow_up(&h, &work, original).await;

    let change = "Only the permission slip matters now.";
    let updated = tool(
        app,
        &token,
        "delegation_update",
        json!({ "task_id": task, "message": change }),
    )
    .await;
    updated.ok();
    let updated = updated.result;
    assert_eq!(updated["task_id"], task.to_string());
    assert!(
        matches!(updated["status"].as_str(), Some("queued" | "running")),
        "{updated}"
    );
    app.wait_turn(first, &["interrupted"]).await;
    let stopped: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT error_code, error_message FROM turns WHERE id = ?1")
            .bind(first)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(
        stopped.0.as_deref(),
        Some("superseded"),
        "the stopped attempt says why it stopped: {stopped:?}"
    );
    assert!(
        stopped
            .1
            .as_deref()
            .is_some_and(|m| m.contains("continues")),
        "the stopped attempt says the task continues: {stopped:?}"
    );
    let second = child_of(app, task).await;
    assert_ne!(second, first, "the update runs in a new turn");
    let conversations: Vec<Uuid> = sqlx::query_scalar(
        "SELECT conversation_id FROM turns WHERE id IN (?1, ?2) GROUP BY conversation_id",
    )
    .bind(first)
    .bind(second)
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(
        conversations.len(),
        1,
        "the update continues the task conversation"
    );

    let (_, result, _) = wait_task(app, task, "completed").await;
    assert!(
        result.as_deref().is_some_and(|r| r.contains(change)),
        "the delegating bot gets the updated reply: {result:?}"
    );
    let snapshot =
        else_api::db::conversation::snapshot(&app.db.pool, work.parse().unwrap(), 50, None)
            .await
            .unwrap()
            .unwrap();
    let prompts: Vec<&str> = snapshot
        .items
        .iter()
        .filter_map(|item| item.bot_task.as_ref().map(|t| t.prompt.as_str()))
        .collect();
    assert_eq!(
        prompts,
        [original, change],
        "the owner sees the task and its update"
    );

    let repeated = tool(
        app,
        &token,
        "delegation_update",
        json!({ "task_id": task, "message": change }),
    )
    .await;
    repeated.ok();
    let repeated = repeated.result;
    assert_eq!(repeated["status"], "completed");
    assert_eq!(repeated["next"], "This update was already applied.");
    assert_eq!(child_of(app, task).await, second, "a retry changes nothing");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_update_without_the_tasks_session_still_sees_its_earlier_reply() {
    let h = harness(TestApp::builder().max_active(3)).await;
    let app = &h.app;
    let recorder = &h.recorder;
    let work = leaf(app, "Work").await;
    let original = format!("{GATE_AFTER_OPENING} List the school forms.");
    let (task, first, token) = task_carried_to_a_follow_up(&h, &work, &original).await;
    let partial = common::until(
        "the first attempt's partial reply",
        common::poll(|| async {
            let text: Option<String> = sqlx::query_scalar(
            "SELECT group_concat(text, '') FROM items WHERE turn_id = ?1 AND kind = 'assistant'",
        )
        .bind(first)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
            text.filter(|t| t.len() >= 14)
        }),
    )
    .await;
    let native: Vec<String> = sqlx::query_scalar(
        "SELECT s.native_session_id FROM provider_sessions s JOIN turns t
         ON t.conversation_id = s.conversation_id WHERE t.id = ?1",
    )
    .bind(first)
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    recorder.forget_sessions(&native);

    let change = "Only the permission slip matters now.";
    let updated = tool(
        app,
        &token,
        "delegation_update",
        json!({ "task_id": task, "message": change }),
    )
    .await;
    updated.ok();
    wait_task(app, task, "completed").await;
    let packet = h
        .packets()
        .await
        .into_iter()
        .find(|p| p.ends_with(change))
        .expect("the update reached Work");
    assert!(
        packet.contains(&partial[..14]),
        "the update carries the task's earlier reply: {packet}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_later_update_can_repeat_an_earlier_one() {
    let h = harness(TestApp::builder().max_active(3)).await;
    let app = &h.app;
    let work = leaf(app, "Work").await;
    let (task, _, token) =
        task_carried_to_a_follow_up(&h, &work, "[synthetic:hang] Plan the week.").await;
    let include = "[synthetic:hang] Include the school forms.";
    let exclude = "[synthetic:hang] Leave out the school forms.";
    let updated = tool(
        app,
        &token,
        "delegation_update",
        json!({ "task_id": task, "message": include }),
    )
    .await;
    updated.ok();
    let included = child_of(app, task).await;
    let updated = tool(
        app,
        &token,
        "delegation_update",
        json!({ "task_id": task, "message": exclude }),
    )
    .await;
    updated.ok();
    let again = tool(
        app,
        &token,
        "delegation_update",
        json!({ "task_id": task, "message": include }),
    )
    .await;
    again.ok();
    let latest = child_of(app, task).await;
    assert_ne!(
        latest, included,
        "changing back is a new update: {}",
        again.body
    );
    let source: String = sqlx::query_scalar(
        "SELECT i.text FROM turns t JOIN items i ON i.id = t.owner_item_id WHERE t.id = ?1",
    )
    .bind(latest)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(source, include);
    let retry = tool(
        app,
        &token,
        "delegation_update",
        json!({ "task_id": task, "message": include }),
    )
    .await;
    retry.ok();
    assert_eq!(retry.result["next"], "This update was already applied.");
    assert_eq!(child_of(app, task).await, latest);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_aside_leaves_a_queued_update_to_run() {
    let h = harness(TestApp::builder().max_active(3)).await;
    let app = &h.app;
    let recorder = &h.recorder;
    let work = leaf(app, "Work").await;
    let (task, first, token) =
        task_carried_to_a_follow_up(&h, &work, "[synthetic:hang] long research").await;
    recorder.hold_hangs(true);
    let updated = tool(
        app,
        &token,
        "delegation_update",
        json!({ "task_id": task, "message": "Narrow it to the permission slip." }),
    )
    .await;
    updated.ok();
    app.wait_turn(first, &["cancelling"]).await;
    let replacement = child_of(app, task).await;
    app.wait_turn(replacement, &["queued"]).await;

    let aside = "Release a patch.";
    let sent = app
        .send(&work, Uuid::new_v4(), aside, Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let aside_turn: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    recorder.hold_hangs(false);
    app.wait_turn(replacement, &["completed"]).await;
    let (_, result, _) = wait_task(app, task, "completed").await;
    assert!(
        result
            .as_deref()
            .is_some_and(|r| r.contains("permission slip") && !r.contains(aside)),
        "the update answers the task: {result:?}"
    );
    app.wait_turn(aside_turn, &["completed"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegating_bot_cancels_a_running_task() {
    let h = harness(TestApp::builder().max_active(3)).await;
    let app = &h.app;
    let work = leaf(app, "Work").await;
    let (task, child, token) =
        task_carried_to_a_follow_up(&h, &work, "[synthetic:hang] long research").await;

    let stopping = tool(app, &token, "delegation_cancel", json!({ "task_id": task })).await;
    stopping.ok();
    let stopping = stopping.result;
    assert_eq!(stopping["status"], "stopping", "{stopping}");
    app.wait_turn(child, &["interrupted"]).await;
    let (_, _, error) = wait_task(app, task, "cancelled").await;
    assert!(
        error
            .as_deref()
            .is_some_and(|e| e.starts_with("cancelled: ") && e.contains("cancelled the task")),
        "the delegating bot learns who stopped it: {error:?}"
    );

    let again = tool(app, &token, "delegation_cancel", json!({ "task_id": task })).await;
    again.ok();
    let again = again.result;
    assert_eq!(again["status"], "cancelled");
    assert!(
        again["next"].as_str().unwrap().contains("continuation"),
        "{again}"
    );
    let delivered: Option<i64> =
        sqlx::query_scalar("SELECT delivered_attempt FROM delegations WHERE id = ?1")
            .bind(task)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(delivered, None, "the outcome is left for the continuation");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_update_to_an_ended_task_is_rejected_with_its_outcome() {
    let h = harness(TestApp::builder().max_active(3)).await;
    let app = &h.app;
    let work = leaf(app, "Work").await;
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let reply = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &work, "task": "Say which forms are due." }),
    )
    .await;
    let task: Uuid = reply.result["task_id"].as_str().unwrap().parse().unwrap();
    h.release(root).await;
    let (_, result, _) = wait_task(app, task, "completed").await;
    let child = child_of(app, task).await;
    let token = h.continued(root, &token).await;

    let rejected = tool(
        app,
        &token,
        "delegation_update",
        json!({ "task_id": task, "message": "Also the dentist." }),
    )
    .await;
    assert!(rejected.error, "{}", rejected.body);
    let rejected = rejected.result;
    assert!(
        rejected["error"].as_str().unwrap().starts_with("ENDED"),
        "{rejected}"
    );
    assert_eq!(rejected["task"]["status"], "completed");
    assert!(result.is_some());
    assert!(
        rejected["task"]["next"]
            .as_str()
            .unwrap()
            .contains("continuation"),
        "{rejected}"
    );
    assert_eq!(child_of(app, task).await, child, "nothing was changed");

    let unknown = tool(
        app,
        &token,
        "delegation_cancel",
        json!({ "task_id": Uuid::new_v4() }),
    )
    .await;
    assert!(unknown.error, "{}", unknown.body);
    let unknown = unknown.result;
    assert!(
        unknown["error"].as_str().unwrap().starts_with("NOT_FOUND"),
        "{unknown}"
    );
}
