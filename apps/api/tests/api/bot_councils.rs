use crate::common;

use axum::http::StatusCode;
use common::recorder::{GATE, harness, tool};
use serde_json::json;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn peer_creation_acknowledges_the_main_bot_once_with_its_purpose() {
    let h = harness(3).await;
    let main = h.app.main_bot().await;
    let peer = h.app.create_bot("Work").await;
    let category = h
        .app
        .owner(
            "POST",
            "/v1/categories",
            Some(json!({
                "request_id": Uuid::new_v4(), "name": "Engineering"
            })),
        )
        .await;
    assert!(category.status.is_success(), "{}", category.body);
    let category = category.body["category"]["id"].as_str().unwrap();
    let (root, token) = h.running(&peer, GATE).await;
    let args = json!({"name": "Release", "purpose": "Track releases and flag deployment blockers.",
        "category_id": category});
    let (a, b) = tokio::join!(
        tool(&h.app, &token, "create_bot", args.clone()),
        tool(&h.app, &token, "create_bot", args.clone())
    );
    assert!(!a.error && !b.error, "{} {}", a.body, b.body);
    assert_eq!(a.result["bot_id"], b.result["bot_id"]);
    assert_ne!(a.result["created"], b.result["created"]);
    let created = a.result["bot_id"].as_str().unwrap();
    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await;
    let receipts: Vec<_> = snapshot.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["text"].as_str().is_some_and(|s| s.contains(created)))
        .collect();
    assert_eq!(receipts.len(), 1, "{}", snapshot.body);
    assert_eq!(receipts[0]["kind"], "status");
    let notice = receipts[0]["text"].as_str().unwrap();
    for expected in [
        "Work",
        "Release",
        "Track releases and flag deployment blockers.",
    ] {
        assert!(notice.contains(expected), "{notice}");
    }
    assert_eq!(
        receipts[0]["bot_created"],
        json!({"bot_id": created, "name": "Release", "creator_bot_id": peer,
            "creator_name": "Work", "purpose": "Track releases and flag deployment blockers."})
    );
    let saved = h.app.owner("GET", "/v1/bots", None).await;
    assert!(
        saved.body["bots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["id"] == created && b["category_id"] == category)
    );
    let conflict = tool(
        &h.app,
        &token,
        "create_bot",
        json!({"name": "Release", "purpose": "Different task", "category_id": category}),
    )
    .await;
    assert!(conflict.error, "{}", conflict.body);
    assert!(conflict.body.to_string().contains("REQUEST_CONFLICT"));
    let missing = tool(&h.app, &token, "create_bot", json!({"name": "Unknown"})).await;
    assert!(missing.error && missing.body.to_string().contains("PURPOSE_REQUIRED"));
    for purpose in ["".to_owned(), "x".repeat(2001), "bad\0purpose".to_owned()] {
        assert!(
            tool(
                &h.app,
                &token,
                "create_bot",
                json!({"name": "Invalid", "purpose": purpose})
            )
            .await
            .error
        );
    }
    let (main_turn, main_token) = h.running(&main, GATE).await;
    let inputs = h.inputs(main_turn).await;
    assert!(
        inputs[0]
            .history
            .iter()
            .any(|e| e.kind == "status" && e.text == notice)
    );
    let found = tool(
        &h.app,
        &main_token,
        "memory_search",
        json!({"query": "deployment blockers"}),
    )
    .await;
    assert!(!found.error, "{}", found.body);
    assert!(found.result.to_string().contains(created), "{}", found.body);
    let (new_turn, new_token) = h.running(created, GATE).await;
    let memory = tool(
        &h.app,
        &new_token,
        "memory_search",
        json!({"query": "deployment blockers"}),
    )
    .await;
    assert!(
        !memory.error && memory.result.to_string().contains("tool_activity"),
        "{}",
        memory.body
    );
    h.release(root).await;
    h.release(main_turn).await;
    h.release(new_turn).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_category_council_collects_every_report_with_one_provider_slot() {
    let h = harness(1).await;
    let main = h.app.main_bot().await;
    let category = h
        .app
        .owner(
            "POST",
            "/v1/categories",
            Some(json!({
                "request_id": Uuid::new_v4(), "name": "Engineering"
            })),
        )
        .await;
    assert!(category.status.is_success(), "{}", category.body);
    let category = category.body["category"]["id"].as_str().unwrap();
    let mut targets = Vec::new();
    for name in ["Build", "Review", "Release"] {
        let bot = h.app.create_bot(name).await;
        let assigned = h
            .app
            .owner(
                "PATCH",
                &format!("/v1/bots/{bot}/category"),
                Some(json!({"category_id": category})),
            )
            .await;
        assert!(assigned.status.is_success(), "{}", assigned.body);
        targets.push(bot);
    }
    let outside = h.app.create_bot("Personal").await;
    let deleted = h.app.create_bot("Retired").await;
    h.app
        .owner("DELETE", &format!("/v1/bots/{deleted}"), None)
        .await;
    let (root, token) = h.running(&main, GATE).await;
    let all = tool(&h.app, &token, "list_bots", json!({})).await;
    assert!(!all.error, "{}", all.body);
    assert_eq!(all.result["main_bot_id"], main);
    assert!(
        all.result["categories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == category)
    );
    assert!(
        all.result["bots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["bot_id"] == outside)
    );
    let listed = tool(
        &h.app,
        &token,
        "list_bots",
        json!({"category_id": category}),
    )
    .await;
    assert!(!listed.error, "{}", listed.body);
    let members: Vec<_> = listed.result["bots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["bot_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(members.len(), 3);
    assert!(members.iter().all(|id| targets.contains(id)));
    assert!(
        tool(
            &h.app,
            &token,
            "list_bots",
            json!({"category_id": Uuid::new_v4()})
        )
        .await
        .error
    );
    let task = "Report the current engineering risks, evidence, and recommended next steps.";
    let accepted = tool(
        &h.app,
        &token,
        "council",
        json!({"target_bot_ids": members, "task": task}),
    )
    .await;
    assert!(
        !accepted.error && accepted.result["parked"] == true,
        "{}",
        accepted.body
    );
    assert_eq!(accepted.result["tasks"].as_array().unwrap().len(), 3);
    let ids: Vec<Uuid> = targets.iter().map(|id| id.parse().unwrap()).collect();
    let replay = else_api::db::delegations::council(&h.app.db.pool, root, &ids, task)
        .await
        .unwrap();
    assert!(replay.iter().all(|a| !a.created));
    h.release(root).await;
    h.app
        .wait_turn_for(
            &root.to_string(),
            &["completed"],
            std::time::Duration::from_secs(20),
        )
        .await;
    let inputs = h.inputs(root).await;
    assert_eq!(inputs.len(), 2);
    let outcomes = &inputs[1].delegation.outcomes;
    assert_eq!(outcomes.len(), 3, "{outcomes:?}");
    assert!(outcomes.iter().all(|o| o.status == "completed" && o.result.as_ref().is_some_and(|r| r.contains(task))));
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM memory_read_grants")
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(grants, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn councils_are_atomic_bounded_and_cannot_create_nested_work() {
    let h = harness(1).await;
    let main = h.app.main_bot().await;
    let peer = h.app.create_bot("Review").await;
    let (root, token) = h.running(&main, GATE).await;
    for ids in [
        vec![],
        vec![peer.clone(), peer.clone()],
        vec![peer.clone(), Uuid::max().to_string()],
        vec![main.clone()],
        (0..33).map(|_| Uuid::new_v4().to_string()).collect(),
    ] {
        let rejected = tool(
            &h.app,
            &token,
            "council",
            json!({"target_bot_ids": ids, "task": "Review the release."}),
        )
        .await;
        assert!(rejected.error, "{}", rejected.body);
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM delegations WHERE root_turn_id = ?1")
                .bind(root)
                .fetch_one(&h.app.db.pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }
    let accepted = tool(
        &h.app,
        &token,
        "council",
        json!({"target_bot_ids": [peer], "task": "[synthetic:hang] review"}),
    )
    .await;
    assert!(!accepted.error, "{}", accepted.body);
    let extra = h.app.create_bot("Late").await;
    assert!(
        else_api::db::delegations::council(
            &h.app.db.pool,
            root,
            &[extra.parse().unwrap()],
            "Review"
        )
        .await
        .is_err()
    );
    h.release(root).await;
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    h.app.wait_turn(&child.to_string(), &["running"]).await;
    assert!(matches!(
        else_api::db::delegations::council(
            &h.app.db.pool,
            child,
            &[main.parse().unwrap()],
            "Report"
        )
        .await,
        Err(else_api::db::delegations::DelegateError::NotAllowed)
    ));
    let stopped = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({"request_id": Uuid::new_v4()})),
        )
        .await;
    assert!(stopped.status.is_success(), "{}", stopped.body);
    h.app.wait_turn(&root.to_string(), &["interrupted"]).await;
    h.app.wait_turn(&child.to_string(), &["interrupted"]).await;
    assert_eq!(
        tool(&h.app, &token, "list_bots", json!({})).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_member_is_included_with_the_other_council_reports() {
    let h = harness(3).await;
    let main = h.app.main_bot().await;
    let mut targets = Vec::new();
    for name in ["Build", "Review", "Release"] {
        targets.push(h.app.create_bot(name).await);
    }
    let (root, token) = h.running(&main, GATE).await;
    let accepted = tool(
        &h.app,
        &token,
        "council",
        json!({
            "target_bot_ids": targets, "task": "[gate] Report the release risks."
        }),
    )
    .await;
    assert!(!accepted.error, "{}", accepted.body);
    h.release(root).await;
    let children: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT child_turn_id, target_bot_id FROM delegations WHERE root_turn_id = ?1 ORDER BY target_bot_id"
    ).bind(root).fetch_all(&h.app.db.pool).await.unwrap();
    for (child, _) in &children {
        h.started(*child).await;
    }
    let cancelled = children[0].1;
    let stopped = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{cancelled}/cancel"),
            Some(json!({"request_id": Uuid::new_v4()})),
        )
        .await;
    assert!(stopped.status.is_success(), "{}", stopped.body);
    h.release(children[0].0).await;
    h.app
        .wait_turn(&children[0].0.to_string(), &["interrupted"])
        .await;
    h.app.wait_turn(&root.to_string(), &["waiting"]).await;
    h.release(children[1].0).await;
    h.release(children[2].0).await;
    h.app.wait_turn(&root.to_string(), &["completed"]).await;
    let inputs = h.inputs(root).await;
    assert_eq!(inputs.len(), 2);
    let outcomes = &inputs[1].delegation.outcomes;
    assert_eq!(outcomes.len(), 3);
    assert!(
        outcomes
            .iter()
            .any(|o| o.target_bot_id == cancelled && o.status == "cancelled")
    );
    assert_eq!(
        outcomes.iter().filter(|o| o.status == "completed").count(),
        2
    );
}

#[tokio::test]
async fn earlier_creation_notices_gain_their_card_on_upgrade() {
    let db = common::TestDb::empty().await;
    let dir = tempfile::tempdir().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut names: Vec<_> = std::fs::read_dir(&source)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_str().unwrap() < "0027")
        .collect();
    names.sort();
    for name in &names {
        std::fs::copy(source.join(name), dir.path().join(name)).unwrap();
    }
    sqlx::migrate::Migrator::new(dir.path())
        .await
        .unwrap()
        .run(&db.pool)
        .await
        .unwrap();
    let (main, creator, created) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    for (id, name) in [
        (main, "Assistant"),
        (creator, "Tech (bot_id backup)\nPurpose: Manager"),
        (created, "Auto"),
    ] {
        sqlx::query("INSERT INTO bots (id, name) VALUES (?1, ?2)")
            .bind(id)
            .bind(name)
            .execute(&db.pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO conversations (id, bot_id) VALUES (randomblob(16), ?1)")
        .bind(main)
        .execute(&db.pool)
        .await
        .unwrap();
    let (turn, notice, other, stale) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let text = format!(
        "Tech (bot_id backup)\nPurpose: Manager (bot_id {creator}) created Auto (bot_id {created}).\nPurpose: Own the routines.\nKeep approvals."
    );
    let renamed = format!(
        "Old Name (bot_id {creator}) created Auto (bot_id {created}).\nPurpose: Own the routines."
    );
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, status)
         SELECT ?1, ?2, c.id, ?3, ?4, 1, ?5, 'system', 'completed'
         FROM conversations c WHERE c.bot_id = ?2",
    )
    .bind(turn)
    .bind(main)
    .bind(created)
    .bind(vec![1u8; 32])
    .bind(notice)
    .execute(&db.pool)
    .await
    .unwrap();
    for (seq, id, text) in [
        (1, notice, text.as_str()),
        (2, other, "Purpose: unrelated"),
        (3, stale, renamed.as_str()),
    ] {
        sqlx::query(
            "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
             SELECT ?1, conversation_id, ?2, id, 'status', ?3 FROM turns WHERE id = ?4",
        )
        .bind(id)
        .bind(seq)
        .bind(text)
        .bind(turn)
        .execute(&db.pool)
        .await
        .unwrap();
    }
    else_api::db::MIGRATOR.run(&db.pool).await.unwrap();
    let card: sqlx::types::Json<else_api::db::turns::BotCreated> =
        sqlx::query_scalar("SELECT bot_created FROM items WHERE id = ?1")
            .bind(notice)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        serde_json::to_value(&card.0).unwrap(),
        json!({"bot_id": created, "name": "Auto", "creator_bot_id": creator,
            "creator_name": "Tech (bot_id backup)\nPurpose: Manager", "purpose": "Own the routines.\nKeep approvals."})
    );
    for id in [other, stale] {
        let untouched: Option<String> =
            sqlx::query_scalar("SELECT bot_created FROM items WHERE id = ?1")
                .bind(id)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(untouched, None, "unverifiable notices stay plain");
    }
    let history = else_api::db::conversation::snapshot(&db.pool, main, 10, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(history.items.len(), 3, "the conversation still loads");
}
