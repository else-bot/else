//! Background work beside a waiting request, and what a delegated task can
//! reach.

use super::*;

/// A bot waiting on a task is idle: a mail event or routine run in the same
/// chat runs at once, and the waiting request continues after it.
#[tokio::test(flavor = "multi_thread")]
async fn background_work_in_a_waiting_chat_runs_and_the_request_continues_after_it() {
    for kind in [ItemKind::Connector, ItemKind::Routine] {
        let db = TestDb::migrated().await;
        let (main, root, child, _) = seed(&db).await;
        common::seed::force_status(&db.pool, root, TurnStatus::Waiting).await;
        common::seed::force_status(&db.pool, child, TurnStatus::Running).await;
        let event = common::seed::turn(main)
            .turn_no(2)
            .message(kind, "New mail arrived.")
            .insert(&db.pool)
            .await;

        let claimed = else_api::db::turns::claim_next(&db.pool, &[], true)
            .await
            .unwrap()
            .map(|c| (c.id, c.background));
        assert_eq!(
            claimed,
            Some((event, true)),
            "{kind:?} runs while {main} waits"
        );

        // The task's turn ends; the delegation trigger settles its task.
        sqlx::query(
            "UPDATE turns SET status = 'completed',
                 finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE id = ?1",
        )
        .bind(child)
        .execute(&db.pool)
        .await
        .unwrap();
        let root_status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
            .bind(root)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            root_status, "queued",
            "the task's result continues the request"
        );
        assert!(
            else_api::db::turns::claim_next(&db.pool, &[], true)
                .await
                .unwrap()
                .is_none(),
            "one active turn per bot"
        );

        // The background turn ends without a provider.
        sqlx::query(
            "UPDATE turns SET status = 'completed',
                 finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE id = ?1",
        )
        .bind(event)
        .execute(&db.pool)
        .await
        .unwrap();
        let next = else_api::db::turns::claim_next(&db.pool, &[], true)
            .await
            .unwrap()
            .map(|c| c.id);
        assert_eq!(
            next,
            Some(root),
            "the continuation runs after the {kind:?} turn"
        );
    }
}

/// Background work running beside a waiting request cannot settle the
/// cards that request waits on: only the owner does.
#[tokio::test(flavor = "multi_thread")]
async fn background_work_leaves_a_waiting_requests_cards_to_the_owner() {
    use else_api::app::outgoing::{Content, Email};
    use else_api::db::{computer_handoffs, outgoing_drafts};
    let db = TestDb::migrated().await;
    let (main, root, _, _) = seed(&db).await;
    let email = |body: &str| {
        Content::Email(Email {
            from: "me@example.com".into(),
            to: vec!["ada@example.com".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Lunch".into(),
            body: body.into(),
            account_id: None,
            reply_to: None,
            attachments: Vec::new(),
        })
    };
    sqlx::query("UPDATE turns SET attempt = 2 WHERE id = ?1")
        .bind(root)
        .execute(&db.pool)
        .await
        .unwrap();
    let draft = outgoing_drafts::propose(&db.pool, root, &email("Friday?"), None)
        .await
        .unwrap();
    let handoff = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO computer_handoffs (id, bot_id, turn_id, attempt, reason)
         SELECT ?1, bot_id, id, attempt, 'Log in to the mail account.' FROM turns WHERE id = ?2",
    )
    .bind(handoff)
    .bind(root)
    .execute(&db.pool)
    .await
    .unwrap();
    common::seed::force_status(&db.pool, root, TurnStatus::Waiting).await;
    let event = common::seed::turn(main)
        .turn_no(2)
        .status(TurnStatus::Running)
        .attempt(1)
        .insert(&db.pool)
        .await;

    let revised =
        outgoing_drafts::propose(&db.pool, event, &email("Saturday?"), Some(draft.id)).await;
    assert!(
        matches!(revised, Err(outgoing_drafts::DraftError::Held)),
        "{revised:?}"
    );
    let asked = computer_handoffs::create(&db.pool, event, "Log in again.").await;
    assert_eq!(
        asked.err(),
        Some(computer_handoffs::CreateError::OwnerAsked)
    );
    let state: (String, String, String) = sqlx::query_as(
        "SELECT t.status, d.status, h.status FROM turns t, outgoing_drafts d, computer_handoffs h
         WHERE t.id = ?1 AND d.id = ?2 AND h.id = ?3",
    )
    .bind(root)
    .bind(draft.id)
    .bind(handoff)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        state,
        ("waiting".into(), "pending".into(), "pending".into()),
        "the request keeps waiting on its own cards"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_task_to_another_bot_is_a_different_child() {
    let db = TestDb::migrated().await;
    let (main, root, _, first) = seed(&db).await;
    let other = common::seed::bot(&db.pool, "Other").await.id;
    let _ = main;
    sqlx::query("UPDATE turns SET attempt = 2 WHERE id = ?1")
        .bind(root)
        .execute(&db.pool)
        .await
        .unwrap();
    let second =
        else_api::db::delegations::create(&db.pool, root, other, "seeded task", None, &[], None)
            .await
            .unwrap();
    assert!(second.created);
    assert_ne!(second.delegation_id, first);
    let target: Uuid = sqlx::query_scalar("SELECT target_bot_id FROM delegations WHERE id = ?1")
        .bind(first)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(first)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    // The task's turn ends; the delegation trigger settles its task.
    sqlx::query("UPDATE turns SET status = 'completed', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE id = ?1")
        .bind(child)
        .execute(&db.pool)
        .await
        .unwrap();
    let retry =
        else_api::db::delegations::create(&db.pool, root, target, "seeded task", None, &[], None)
            .await
            .unwrap();
    assert!(!retry.created);
    assert_eq!(retry.status, else_api::db::delegations::Status::Completed);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_task_cannot_read_memory_the_parent_could_not() {
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let work = leaf(app, "Work").await;
    let private = leaf(app, "Private").await;
    let shared = leaf(app, "Shared").await;
    let main = app.main_bot().await;
    for (reader, target) in [(&work, &private), (&work, &shared), (&main, &shared)] {
        let reply = app
            .owner(
                "PUT",
                &format!("/v1/bots/{reader}/memory-access/{target}"),
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
    }
    let (root, token) = h.running(&main, GATE).await;
    let delegated = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": work, "task": GATE }),
    )
    .await;
    delegated.ok();
    h.release(root).await;
    let task = delegated.result["task_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let child = child_of(app, task).await;
    let (_, token) = h.started(child).await;
    let call = |bot: Uuid| {
        tool(
            app,
            &token,
            "memory_search",
            json!({ "query": "hello", "bot_id": bot }),
        )
    };
    assert!(
        call(private.parse().unwrap()).await.error,
        "Work's own grant to Private is not inherited"
    );
    call(shared.parse().unwrap()).await.ok();
    call(work.parse().unwrap()).await.ok();
    h.release(child).await;
}
