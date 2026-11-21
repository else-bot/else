//! How a delegated task shows in the chat that receives it and the chat that
//! sent it.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_new_bot_acknowledges_delegated_work_in_its_conversation() {
    let h = harness(TestApp::builder().max_active(1)).await;
    let app = &h.app;
    let target = app.create_bot("GitHub").await;
    sqlx::query("UPDATE bots SET selected_provider_id = 'synthetic' WHERE id = ?1")
        .bind(target.parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE).await;
    let task = "Set up the repository cleanup routine every six hours.";
    let reply = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &target, "task": task }),
    )
    .await;
    assert_eq!(reply.result["status"], "pending", "{}", reply.body);

    let snapshot = app
        .owner("GET", &format!("/v1/bots/{target}/conversation"), None)
        .await
        .body;
    let items = snapshot["items"].as_array().unwrap();
    assert!(
        !items.is_empty(),
        "receiving bot conversation is empty after accepting delegated work: {snapshot}"
    );
    let sender: String = sqlx::query_scalar("SELECT name FROM bots WHERE id = ?1")
        .bind(main.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert!(
        items.iter().any(|item| {
            item["kind"] != "user"
                && item["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(&sender) && text.contains(task))
        }),
        "missing acknowledgement of sender and task: {snapshot}"
    );

    h.release(root).await;
    app.wait_turn(root, &["completed"]).await;
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{target}/conversation"), None)
        .await
        .body;
    assert!(
        snapshot["items"].as_array().unwrap().iter().any(|item| {
            item["kind"] == "assistant"
                && item["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("Synthetic reply"))
        }),
        "receiving bot reply is missing: {snapshot}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegation_receipt_is_shown_once_before_the_child_runs() {
    let db = TestDb::migrated().await;
    let (main, root, child, delegation) = seed(&db).await;
    let target: Uuid = sqlx::query_scalar("SELECT target_bot_id FROM delegations WHERE id = ?1")
        .bind(delegation)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let retry =
        else_api::db::delegations::create(&db.pool, root, target, "seeded task", None, &[], None)
            .await
            .unwrap();
    assert_eq!((retry.created, retry.delegation_id), (false, delegation));

    let sender: String = sqlx::query_scalar("SELECT name FROM bots WHERE id = ?1")
        .bind(main)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let snapshot = else_api::db::conversation::snapshot(&db.pool, target, 50, None)
        .await
        .unwrap()
        .unwrap();
    let shown: Vec<(else_api::db::turns::ItemKind, Uuid)> =
        snapshot.items.iter().map(|i| (i.kind, i.turn_id)).collect();
    assert_eq!(
        shown,
        [(else_api::db::turns::ItemKind::Status, child)],
        "one receipt, no owner message"
    );
    let task = snapshot.items[0].bot_task.as_ref().unwrap();
    assert_eq!(task.sender_bot_id, main);
    assert_eq!(task.sender_name, sender);
    assert_eq!(task.prompt, "seeded task");
    let receipt = &snapshot.items[0].text;
    assert!(
        receipt.contains(&sender) && receipt.contains("seeded task"),
        "{receipt}"
    );
    let turns: Vec<(Uuid, &str)> = snapshot
        .turns
        .iter()
        .map(|t| (t.id, t.status.as_str()))
        .collect();
    assert_eq!(turns, [(child, "queued")]);

    let source: Uuid = sqlx::query_scalar("SELECT owner_item_id FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    else_api::db::events::item_upsert(&mut tx, target, source)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let conversation = owner_conversation(&db.pool, target).await;
    let events = turn_events(&db.pool, target, child).await;
    assert_eq!(
        events[0].1["item"]["bot_task"]["sender_bot_id"],
        main.to_string()
    );
    assert_eq!(events[0].1["item"]["bot_task"]["prompt"], "seeded task");
    let kinds: Vec<&str> = events.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(kinds, ["item.upsert", "turn.updated"], "{events:?}");
    for (_, data) in &events {
        assert_eq!(data["conversation_id"], conversation.to_string().as_str());
    }
    assert_eq!(
        events[0].1["item"]["id"],
        snapshot.items[0].id.to_string().as_str()
    );
    assert_eq!(events[1].1["turn"]["status"], "queued");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_reply_streams_into_the_receiving_chat_in_order() {
    let h = harness(TestApp::builder().max_active(1)).await;
    let app = &h.app;
    let school = leaf(app, "School").await;
    let school_id: Uuid = school.parse().unwrap();
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE).await;
    let task = "Collect the planetarium form details.";
    tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &school, "task": task }),
    )
    .await;
    h.release(root).await;
    app.wait_turn(root, &["completed"]).await;
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();

    let page = |query: &'static str| {
        let school = &school;
        async move {
            app.owner(
                "GET",
                &format!("/v1/bots/{school}/conversation?{query}"),
                None,
            )
            .await
            .body
        }
    };
    let rows = |snapshot: &Value| -> Vec<(i64, String, String)> {
        snapshot["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                (
                    i["seq"].as_i64().unwrap(),
                    i["kind"].as_str().unwrap().to_owned(),
                    i["turn_id"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let newest = page("limit=2").await;
    let child_id = child.to_string();
    assert_eq!(
        rows(&newest),
        [
            (3, "status".to_owned(), child_id.clone()),
            (4, "assistant".to_owned(), child_id.clone())
        ],
        "{newest}"
    );
    assert_eq!(newest["older_before_seq"], 3);
    let older = page("limit=2&before_seq=3").await;
    let older_kinds: Vec<String> = rows(&older).into_iter().map(|(_, kind, _)| kind).collect();
    assert_eq!(
        older_kinds,
        ["user", "assistant"],
        "the earlier owner exchange"
    );
    assert_eq!(older["older_before_seq"], Value::Null);

    let conversation = owner_conversation(&app.db.pool, school_id).await;
    let events = turn_events(&app.db.pool, school_id, child).await;
    assert!(
        events
            .iter()
            .all(|(_, data)| data["conversation_id"] == conversation.to_string().as_str()),
        "{events:?}"
    );
    let reply_upserts = events
        .iter()
        .filter(|(kind, data)| kind == "item.upsert" && data["item"]["kind"] == "assistant")
        .count();
    assert_eq!(reply_upserts, 1, "{events:?}");
    let statuses: Vec<&str> = events
        .iter()
        .filter(|(kind, _)| kind == "turn.updated")
        .map(|(_, data)| data["turn"]["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses.first(), Some(&"queued"), "{statuses:?}");
    assert!(statuses.contains(&"running"), "{statuses:?}");
    assert_eq!(statuses.last(), Some(&"completed"), "{statuses:?}");

    let replies: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM memory_entry WHERE bot_id = ?1 AND body LIKE '%Synthetic reply%planetarium form details%'",
    )
    .bind(school_id)
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(replies, ["assistant_message"]);

    let next = app
        .send(
            &school,
            Uuid::new_v4(),
            "[synthetic:hang] and now?",
            Some("synthetic"),
        )
        .await;
    let next: Uuid = next.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    let input = else_api::db::turns::input(&app.db.pool, next)
        .await
        .unwrap();
    let packet = common::fresh_packet(&input);
    assert!(
        packet.contains("Delegated task (another bot's request, not an owner message)")
            && !packet.contains(&format!("Owner: {task}")),
        "{packet}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn task_history_pages_its_source_and_reply_without_the_owner_receipt() {
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let target = app.create_bot("GitHub").await;
    let target_id: Uuid = target.parse().unwrap();
    sqlx::query("UPDATE bots SET selected_provider_id = 'synthetic' WHERE id = ?1")
        .bind(target_id)
        .execute(&app.db.pool)
        .await
        .unwrap();
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE).await;
    let task =
        format!("{GATE_AFTER_OPENING} Set up the repository cleanup routine every six hours.");
    tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": target, "task": task }),
    )
    .await;
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    h.release(root).await;
    let (_, grant) = h.started(child).await;
    let reply: Uuid = tokio::time::timeout(common::SETTLE, async {
        loop {
            let found: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM items WHERE turn_id = ?1 AND kind = 'assistant'",
            )
            .bind(child)
            .fetch_optional(&app.db.pool)
            .await
            .unwrap();
            if let Some(found) = found {
                break found;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let newest = app
        .call("GET", "/tools/v1/history?limit=1", None, Some(&grant))
        .await;
    assert_eq!(newest.body["items"][0]["id"], reply.to_string());
    let older = app
        .call("GET", "/tools/v1/history?before_seq=2", None, Some(&grant))
        .await;
    assert_eq!(older.status, StatusCode::OK, "{}", older.body);
    let items = older.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{}", older.body);
    assert_eq!(items[0]["kind"], "user");
    for id in [
        reply.to_string(),
        items[0]["id"].as_str().unwrap().to_owned(),
    ] {
        let own = app
            .call("GET", &format!("/tools/v1/items/{id}"), None, Some(&grant))
            .await;
        assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    }
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{target}/conversation"), None)
        .await;
    let receipt = snapshot.body["items"][0]["id"].as_str().unwrap();
    let hidden = app
        .call(
            "GET",
            &format!("/tools/v1/items/{receipt}"),
            None,
            Some(&grant),
        )
        .await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND);
    h.release(child).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_delegated_task_keeps_its_scope_and_its_stop_is_shown() {
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let work = leaf(app, "Work").await;
    let work_id: Uuid = work.parse().unwrap();
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE).await;
    tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &work, "task": "[synthetic:hang] long research" }),
    )
    .await;
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    h.release(root).await;
    let (_, grant) = h.started(child).await;
    let history = app
        .call("GET", "/tools/v1/history", None, Some(&grant))
        .await;
    assert_eq!(history.status, StatusCode::OK, "{}", history.body);
    let items = history.body["items"].as_array().unwrap();
    let kinds: Vec<&str> = items.iter().map(|i| i["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["user"], "its task, without the owner's transcript");
    for item in items {
        let id = item["id"].as_str().unwrap();
        let own = app
            .call("GET", &format!("/tools/v1/items/{id}"), None, Some(&grant))
            .await;
        assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    }
    let hello: Uuid = sqlx::query_scalar(
        "SELECT i.id FROM items i JOIN conversations c ON c.id = i.conversation_id
         WHERE c.bot_id = ?1 AND c.kind = 'owner' AND i.kind = 'user'",
    )
    .bind(work_id)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    let foreign = app
        .call(
            "GET",
            &format!("/tools/v1/items/{hello}"),
            None,
            Some(&grant),
        )
        .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);

    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{work}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    app.wait_turn(child, &["interrupted"]).await;
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{work}/conversation"), None)
        .await
        .body;
    let shown = snapshot["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == child.to_string().as_str())
        .map(|t| t["status"].clone());
    assert_eq!(shown, Some(json!("interrupted")), "{snapshot}");
    let last = turn_events(&app.db.pool, work_id, child)
        .await
        .pop()
        .unwrap();
    assert_eq!(last.1["turn"]["status"], "interrupted");
}
