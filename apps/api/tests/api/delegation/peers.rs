//! Bots that hand work to each other.

use super::*;

async fn delegation_of(
    app: &TestApp,
    root: Uuid,
) -> (Uuid, Uuid, String, Option<i32>, Option<String>) {
    sqlx::query_as(
        "SELECT parent_bot_id, target_bot_id, status, delivered_attempt, result
         FROM delegations WHERE root_turn_id = ?1",
    )
    .bind(root)
    .fetch_one(&app.db.pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn any_bot_hands_work_to_a_peer_or_the_main_bot_and_gets_the_reply() {
    let h = harness(common::TestApp::builder().max_active(2)).await;
    let main = h.app.main_bot().await;
    let (work, family) = tokio::join!(h.app.create_bot("Work"), h.app.create_bot("Family"));

    for (target, task) in [
        (&family, "Find the date of the school recital."),
        (&main, "Note the dentist appointment on Friday."),
    ] {
        let (root, token) = h.running(&work, GATE).await;
        let pending = tool(
            &h.app,
            &token,
            "delegate",
            json!({ "target_bot_id": target, "task": task }),
        )
        .await;
        assert_eq!(pending.result["status"], "pending", "{}", pending.body);
        h.release(root).await;
        h.app.wait_turn(root, &["completed"]).await;
        let (parent, to, status, delivered, result) = delegation_of(&h.app, root).await;
        assert_eq!(
            (parent.to_string(), to.to_string()),
            (work.clone(), target.clone())
        );
        assert_eq!((status.as_str(), delivered), ("completed", Some(2)));
        assert!(result.unwrap().contains(task));
        let attempts = h.inputs(root).await;
        let results = else_api::providers::prompt::outcomes_block(&attempts[1]);
        assert!(results.contains(task), "{results}");
        let rules = else_api::providers::prompt::instructions(&attempts[0], "/p");
        assert!(rules.contains("register it with create_bot"), "{rules}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_bots_waiting_on_each_other_both_finish_with_one_provider_slot() {
    let h = harness(common::TestApp::builder().max_active(1)).await;
    let (a, b) = tokio::join!(h.app.create_bot("Alpha"), h.app.create_bot("Beta"));
    let root_a = h.send(&a, GATE).await;
    let root_b = h.send(&b, GATE).await;

    let (_, token) = h.started(root_a).await;
    let asked = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": b, "task": "Alpha asks: what is on the calendar?" }),
    )
    .await;
    assert_eq!(asked.result["status"], "pending", "{}", asked.body);
    h.release(root_a).await;
    h.app.wait_turn(root_a, &["waiting"]).await;

    let (_, token) = h.started(root_b).await;
    let asked = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": a, "task": "Beta asks: which bills are due?" }),
    )
    .await;
    assert_eq!(asked.result["status"], "pending", "{}", asked.body);
    h.release(root_b).await;

    tokio::join!(
        h.app.wait_turn(root_a, &["completed"]),
        h.app.wait_turn(root_b, &["completed"]),
    );
    let outcomes = tokio::join!(delegation_of(&h.app, root_a), delegation_of(&h.app, root_b));
    for (_, _, status, delivered, _) in [outcomes.0, outcomes.1] {
        assert_eq!((status.as_str(), delivered), ("completed", Some(2)));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_peer_leaves_its_task_until_the_owner_stops_the_task() {
    let h = harness(common::TestApp::builder().max_active(1)).await;
    let main = h.app.main_bot().await;
    let peer = h.app.create_bot("Work").await;
    let (root, token) = h.running(&peer, GATE).await;
    let pending = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": main, "task": "[synthetic:hang] research the appointment details" }),
    )
    .await;
    assert_eq!(pending.result["status"], "pending", "{}", pending.body);
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    h.release(root).await;
    h.app.wait_turn(root, &["waiting"]).await;
    h.started(child).await;
    let stop = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{peer}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    h.app.wait_turn(root, &["waiting"]).await;
    let (_, _, status, _, _) = delegation_of(&h.app, root).await;
    assert_eq!(status, "running", "Stop leaves the task running");

    let task: Uuid = sqlx::query_scalar("SELECT id FROM delegations WHERE root_turn_id = ?1")
        .bind(root)
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    let cancel = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{peer}/waits/{task}/cancel"),
            None,
        )
        .await;
    assert_eq!(cancel.status, StatusCode::OK, "{}", cancel.body);
    h.app.wait_turn(child, &["interrupted"]).await;
    let (_, _, status, _, _) = delegation_of(&h.app, root).await;
    assert_eq!(status, "cancelled");
}
