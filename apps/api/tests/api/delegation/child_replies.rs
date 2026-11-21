//! How the parent chat shows a child's reply, error, or partial reply.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn the_parent_chat_shows_the_childs_reply_once_before_its_continuation() {
    let h = harness(TestApp::builder().max_active(1)).await;
    let app = &h.app;
    let school = leaf(app, "School").await;
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE).await;
    let main_id: Uuid = main.parse().unwrap();
    let task = "Collect the planetarium form details.";
    let reply = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &school, "task": task }),
    )
    .await;
    let task_id = reply.result["task_id"].as_str().unwrap().to_owned();
    h.release(root).await;
    app.wait_turn(root, &["completed"]).await;
    let result: String = sqlx::query_scalar("SELECT result FROM delegations WHERE id = ?1")
        .bind(task_id.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();

    let snapshot = app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await
        .body;
    let items = snapshot["items"].as_array().unwrap();
    let root_items: Vec<&Value> = items
        .iter()
        .filter(|i| i["turn_id"] == root.to_string().as_str())
        .collect();
    let kinds: Vec<&str> = root_items
        .iter()
        .map(|i| i["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["user", "assistant", "status", "assistant"],
        "{snapshot}"
    );
    let shown = root_items[2];
    assert_eq!(shown["id"], task_id.as_str(), "the reply is the delegation");
    assert_eq!(shown["text"], result.as_str());
    assert!(result.contains("planetarium form details"), "{result}");
    assert_eq!(
        shown["bot_reply"],
        json!({ "sender_bot_id": school, "sender_name": "School",
                "prompt": task, "status": "completed" })
    );
    assert_eq!(shown["bot_task"], Value::Null);
    assert!(
        items
            .iter()
            .filter(|i| i["id"] != task_id.as_str())
            .all(|i| i["bot_reply"].is_null()),
        "{snapshot}"
    );

    let paged = app
        .owner(
            "GET",
            &format!(
                "/v1/bots/{main}/conversation?limit=1&before_seq={}",
                shown["seq"].as_i64().unwrap() + 1
            ),
            None,
        )
        .await
        .body;
    assert_eq!(
        paged["items"],
        json!([shown]),
        "pagination shows the same item"
    );

    let events = bot_events(&app.db.pool, main_id).await;
    let upserts: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, (kind, data))| kind == "item.upsert" && data["item"]["id"] == task_id.as_str())
        .map(|(at, _)| at)
        .collect();
    assert_eq!(upserts.len(), 1, "journaled once");
    let at = upserts[0];
    assert_eq!(events[at].1["item"], *shown, "replay matches the snapshot");
    assert_eq!(
        events[at].1["conversation_id"],
        owner_conversation(&app.db.pool, main_id)
            .await
            .to_string()
            .as_str()
    );
    let root_status = |range: &[(String, Value)]| -> Vec<String> {
        range
            .iter()
            .filter(|(kind, data)| {
                kind == "turn.updated" && data["turn"]["id"] == root.to_string().as_str()
            })
            .map(|(_, data)| data["turn"]["status"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        root_status(&events[..at]).last().map(String::as_str),
        Some("waiting"),
        "the root was parked"
    );
    assert_eq!(
        root_status(&events[at..]).get(..2),
        Some(&["queued".to_owned(), "preparing".to_owned()][..]),
        "the root requeues with the reply, then resumes"
    );
    let continuation = root_items[3]["id"].as_str().unwrap();
    assert!(
        events[..at]
            .iter()
            .all(|(_, data)| data["item"]["id"] != continuation),
        "the continuation's text comes after the reply"
    );

    let target = app
        .owner("GET", &format!("/v1/bots/{school}/conversation"), None)
        .await
        .body;
    assert!(
        target["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["bot_reply"].is_null() && i["id"] != task_id.as_str()),
        "the child's own history has no parent reply card: {target}"
    );

    let logged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_entry WHERE bot_id = ?1 AND request_key = ?2",
    )
    .bind(main_id)
    .bind(format!("item:{task_id}"))
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(logged, 0, "the display item creates no memory entry");
    let next = app
        .send(
            &main,
            Uuid::new_v4(),
            "[synthetic:hang] and now?",
            Some("synthetic"),
        )
        .await;
    let next: Uuid = next.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    let input = else_api::db::turns::input(&app.db.pool, next)
        .await
        .unwrap();
    let kinds: Vec<_> = input.history.iter().map(|h| h.kind).collect();
    assert!(
        !kinds.contains(&else_api::db::turns::ItemKind::Status),
        "{kinds:?}"
    );
}

async fn parent_replies(db: &TestDb, main: Uuid) -> (Vec<(String, String, Uuid)>, usize) {
    let snapshot = else_api::db::conversation::snapshot(&db.pool, main, 50, None)
        .await
        .unwrap()
        .unwrap();
    let shown = snapshot
        .items
        .iter()
        .filter_map(|i| {
            let reply = i.bot_reply.as_ref()?;
            assert_eq!(reply.sender_name, "Target");
            assert_eq!(reply.prompt, "seeded task");
            Some((reply.status.as_str().to_owned(), i.text.clone(), i.turn_id))
        })
        .collect();
    let upserts = bot_events(&db.pool, main)
        .await
        .iter()
        .filter(|(kind, data)| kind == "item.upsert" && !data["item"]["bot_reply"].is_null())
        .count();
    (shown, upserts)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_child_shows_its_error_once_however_often_it_is_settled() {
    let db = TestDb::migrated().await;
    let (main, root, child, _) = seed(&db).await;
    common::seed::force_status(&db.pool, child, TurnStatus::Preparing).await;
    else_api::db::turns::fail_unstarted(&db.pool, child, "no input")
        .await
        .unwrap();
    else_api::db::turns::fail_unstarted(&db.pool, child, "no input")
        .await
        .unwrap();
    let target: Uuid = sqlx::query_scalar("SELECT bot_id FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    else_api::db::events::turn_changed(&mut tx, target, child)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (shown, upserts) = parent_replies(&db, main).await;
    assert_eq!(shown, [("failed".to_owned(), "no input".to_owned(), root)]);
    assert_eq!(upserts, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_child_stopped_by_the_attempt_limit_names_the_limit_apart_from_its_progress() {
    let db = TestDb::migrated().await;
    let (main, root, child, delegation) = seed(&db).await;
    let progress = "Routine paused. Now the computer state.";
    let target: Uuid = sqlx::query_scalar("SELECT bot_id FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    common::seed::item(&db.pool, child, ItemKind::Assistant, progress).await;
    let else_api::app::providers::TurnOutcome::Failed { code, message } =
        else_api::providers::cli::attempt::deadline_failure()
    else {
        panic!("the deadline fails the turn");
    };
    common::seed::force_status(&db.pool, child, TurnStatus::Running).await;
    // Fail the task's turn as its provider deadline does.
    sqlx::query(
        "UPDATE turns SET status = 'failed', error_code = ?2, error_message = ?3 WHERE id = ?1",
    )
    .bind(child)
    .bind(&code)
    .bind(&message)
    .execute(&db.pool)
    .await
    .unwrap();
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    else_api::db::events::turn_changed(&mut tx, target, child)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let snapshot = else_api::db::conversation::snapshot(&db.pool, main, 50, None)
        .await
        .unwrap()
        .unwrap();
    let shown = snapshot.items.iter().find(|i| i.id == delegation).unwrap();
    let reply = shown.bot_reply.as_ref().unwrap();
    assert_eq!(reply.status, else_wire::items::BotReplyStatus::Failed);
    assert_eq!(shown.text, progress, "the progress stays the reply text");
    assert_eq!(reply.reason.as_deref(), Some(message.as_str()));
    assert!(message.contains("30-minute attempt limit"), "{message}");

    let mut conn = db.pool.acquire().await.unwrap();
    let outcomes = else_api::db::delegations::undelivered(&mut conn, root)
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].result.as_deref(), Some(progress));
    assert_eq!(
        outcomes[0].error.as_deref(),
        Some(format!("turn_deadline: {message}").as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_child_keeps_its_partial_reply_and_an_empty_one_shows_empty() {
    let db = TestDb::migrated().await;
    let (main, root, child, _) = seed(&db).await;
    common::seed::item(&db.pool, child, ItemKind::Assistant, "Half of the form").await;
    else_api::db::turns::lifecycle::recover_after_restart(&db.pool)
        .await
        .unwrap();
    let (shown, upserts) = parent_replies(&db, main).await;
    assert_eq!(
        shown,
        [("cancelled".to_owned(), "Half of the form".to_owned(), root)]
    );
    assert_eq!(upserts, 1);

    let db = TestDb::migrated().await;
    let (main, root, child, _) = seed(&db).await;
    common::seed::force_status(&db.pool, child, TurnStatus::Completed).await;
    let target: Uuid = sqlx::query_scalar("SELECT bot_id FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    else_api::db::events::turn_changed(&mut tx, target, child)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (shown, upserts) = parent_replies(&db, main).await;
    assert_eq!(shown, [("completed".to_owned(), String::new(), root)]);
    assert_eq!(upserts, 1);
}
