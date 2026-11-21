//! Bots the main bot creates to delegate to.

use super::*;

async fn bot_count(app: &TestApp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM bots")
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_main_bot_creates_specialists_it_can_delegate_to_at_once() {
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE).await;

    let reply = tool(app, &token, "create_bot", json!({ "name": "  GitHub " })).await;
    let created = reply.result;
    assert_eq!(created["created"], true, "{}", reply.body);
    assert_eq!(created["status"], "active");
    assert_eq!(created["name"], "GitHub");
    let github = created["bot_id"].as_str().unwrap().to_owned();
    let listed = app.owner("GET", "/v1/bots", None).await.body;
    let shown = listed["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == github.as_str())
        .unwrap_or_else(|| panic!("{listed}"))
        .clone();
    assert_eq!(shown["name"], "GitHub");
    let profile: i64 = sqlx::query_scalar("SELECT count(*) FROM bot_profiles WHERE bot_id = ?1")
        .bind(github.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(profile, 0, "no profile of its own");
    let grants: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_read_grants WHERE reader_bot_id = ?1 OR target_bot_id = ?1",
    )
    .bind(github.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(grants, 0, "no shared memory");

    let before = bot_count(app).await;
    let args = json!({ "name": "GitHub" });
    let (a, b, c) = tokio::join!(
        tool(app, &token, "create_bot", args.clone()),
        tool(app, &token, "create_bot", args.clone()),
        tool(app, &token, "create_bot", args.clone()),
    );
    for reply in [a, b, c] {
        let again = reply.result;
        assert_eq!(again["bot_id"], github.as_str(), "{}", reply.body);
        assert_eq!(again["created"], false);
    }
    sqlx::query("UPDATE turns SET attempt = attempt + 1 WHERE id = ?1")
        .bind(root)
        .execute(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        tool(app, &token, "create_bot", args).await.result["bot_id"],
        github.as_str()
    );
    assert_eq!(bot_count(app).await, before);

    let ops = json!({ "name": "Ops" });
    let (a, b, c) = tokio::join!(
        tool(app, &token, "create_bot", ops.clone()),
        tool(app, &token, "create_bot", ops.clone()),
        tool(app, &token, "create_bot", ops.clone()),
    );
    let replies: Vec<Value> = [a, b, c].into_iter().map(|reply| reply.result).collect();
    assert_eq!(
        replies.iter().filter(|r| r["created"] == true).count(),
        1,
        "{replies:?}"
    );
    assert!(replies.iter().all(|r| r["bot_id"] == replies[0]["bot_id"]));
    assert_ne!(
        replies[0]["bot_id"],
        github.as_str(),
        "another name is another bot"
    );
    assert_eq!(bot_count(app).await, before + 1);

    let refusals = [
        (
            json!({ "name": "X", "role": "Maintain repositories." }),
            "INVALID_ARGUMENTS",
        ),
        (
            json!({ "name": "X", "profile": "gpt" }),
            "INVALID_ARGUMENTS",
        ),
        (json!({}), "INVALID_ARGUMENTS"),
        (json!({ "name": 7 }), "INVALID_ARGUMENTS"),
        (json!({ "name": "   " }), "INVALID_BOT"),
    ];
    for (arguments, code) in refusals {
        let reply = tool(app, &token, "create_bot", arguments.clone()).await;
        assert!(reply.error, "{arguments}");
        assert_eq!(reply.code(), code, "{arguments}: {}", reply.body);
    }
    assert_eq!(bot_count(app).await, before + 1);

    let pending = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &github, "task": "List stale branches." }),
    )
    .await
    .result;
    assert_eq!(pending["status"], "pending", "{pending}");
    assert_eq!(pending["target"]["bot_id"], github.as_str());
    let parked = tool(app, &token, "create_bot", json!({ "name": "Late" })).await;
    assert_eq!(parked.status, StatusCode::CONFLICT);
    let name = else_api::app::bots::BotName::parse("Late").unwrap();
    let direct =
        else_api::db::bots::create_for_turn(&app.db.pool, root, &name, None, None, None).await;
    assert!(
        matches!(
            direct,
            Err(else_api::db::bots::BotError::Refused(
                else_api::db::bots::Refusal::AttemptDelegated
                    | else_api::db::bots::Refusal::NotRunning
            ))
        ),
        "{direct:?}"
    );
    assert_eq!(bot_count(app).await, before + 1);
    h.release(root).await;
    app.wait_turn(root, &["completed"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_creation_replays_as_deleted() {
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE).await;
    let args = json!({ "name": "GitHub" });
    let github = tool(app, &token, "create_bot", args.clone()).await.result["bot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let deleted = app
        .owner("DELETE", &format!("/v1/bots/{github}"), None)
        .await;
    assert!(deleted.status.is_success(), "{}", deleted.body);
    let before = bot_count(app).await;
    let replay = tool(app, &token, "create_bot", args).await.result;
    assert_eq!(replay["bot_id"], github.as_str());
    assert_eq!(replay["created"], false);
    assert_eq!(replay["status"], "deleted");
    assert_eq!(bot_count(app).await, before, "no replacement");
    h.release(root).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn creation_requires_an_owner_request_and_a_purpose_for_peer_bots() {
    let h = harness(TestApp::builder().max_active(3)).await;
    let app = &h.app;
    let work = leaf(app, "Work").await;
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, "[synthetic:hang] plan the week").await;
    let before = bot_count(app).await;
    let args = json!({ "name": "GitHub" });
    let plain = h.grant_with(root, vec![Tool::ReadMemory]).await;
    assert_eq!(
        tool(app, &plain, "create_bot", args.clone()).await.status,
        StatusCode::FORBIDDEN
    );

    let (leaf_turn, leaf_token) = h.running(&work, GATE).await;
    let reply = tool(app, &leaf_token, "create_bot", args.clone()).await;
    assert_eq!(reply.code(), "PURPOSE_REQUIRED", "{}", reply.body);
    h.release(leaf_turn).await;
    let pending = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &work, "task": "[synthetic:hang] research" }),
    )
    .await
    .result;
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(
            pending["task_id"]
                .as_str()
                .unwrap()
                .parse::<Uuid>()
                .unwrap(),
        )
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    h.started(child).await;
    let child_token = h.grant_with(child, vec![Tool::CreateBot]).await;
    let reply = tool(app, &child_token, "create_bot", args.clone()).await;
    assert_eq!(reply.code(), "NOT_ALLOWED", "{}", reply.body);
    let name = else_api::app::bots::BotName::parse("GitHub").unwrap();
    assert!(matches!(
        else_api::db::bots::create_for_turn(&app.db.pool, child, &name, None, None, None).await,
        Err(else_api::db::bots::BotError::Refused(
            else_api::db::bots::Refusal::NotOwnerConversation
        ))
    ));

    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    app.wait_turn(root, &["waiting"]).await;
    assert_eq!(
        tool(app, &token, "create_bot", args).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert!(matches!(
        else_api::db::bots::create_for_turn(&app.db.pool, root, &name, None, None, None).await,
        Err(else_api::db::bots::BotError::Refused(
            else_api::db::bots::Refusal::NotRunning
        ))
    ));
    assert_eq!(bot_count(app).await, before);
}
