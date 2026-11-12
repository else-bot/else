use crate::common;

use axum::http::StatusCode;
use common::TestApp;
use common::recorder::{GATE, harness, tool};
use else_api::app::bots::BotName;
use else_api::db::bots::{self, BotError, Refusal};
use else_api::grants::{DEFAULT_TTL, Tool};
use serde_json::{Value, json};
use uuid::Uuid;

async fn snapshot(app: &TestApp, bot: &str) -> Value {
    let reply = app.owner("GET", &format!("/v1/bots/{bot}"), None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.body["bot"].clone()
}

async fn updates(app: &TestApp, bot: &str) -> Vec<Value> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT data FROM bot_events WHERE bot_id = ?1 AND kind = 'bot.updated' ORDER BY seq",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    rows.iter()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}

#[tokio::test]
async fn any_bot_can_rename_a_peer_the_main_bot_or_itself() {
    let h = harness(2).await;
    let main = h.app.main_bot().await;
    let (github, same_name) = tokio::join!(h.app.create_bot("GitHub"), h.app.create_bot("GitHub"));
    let before = snapshot(&h.app, &github).await;
    let (turn, token) = h.running(&main, GATE).await;
    let args = json!({ "target_bot_id": github, "name": "  Tech Manager  " });
    let renamed = tool(&h.app, &token, "rename_bot", args.clone()).await;
    assert!(!renamed.error, "{}", renamed.body);
    assert_eq!(renamed.result["bot_id"], github);
    assert_eq!(renamed.result["name"], "Tech Manager");
    let retry = tool(&h.app, &token, "rename_bot", args).await;
    assert!(!retry.error, "{}", retry.body);
    assert_eq!(retry.result, renamed.result);
    let saved = snapshot(&h.app, &github).await;
    assert_eq!(saved["name"], "Tech Manager");
    for field in [
        "id",
        "conversation_id",
        "avatar",
        "category_id",
        "selected_provider_id",
    ] {
        assert_eq!(saved[field], before[field], "{field}");
    }
    assert_eq!(snapshot(&h.app, &same_name).await["name"], "GitHub");
    let events = updates(&h.app, &github).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["bot"]["name"], "Tech Manager");
    h.release(turn).await;
    h.app.wait_turn(&turn.to_string(), &["completed"]).await;

    let (turn, token) = h.running(&github, GATE).await;
    for (target, name) in [(&main, "Proliquid"), (&github, "Engineering")] {
        let called = tool(
            &h.app,
            &token,
            "rename_bot",
            json!({ "target_bot_id": target, "name": name }),
        )
        .await;
        assert!(!called.error, "{}", called.body);
        assert_eq!(snapshot(&h.app, target).await["name"], name);
    }
    h.release(turn).await;
    h.app.wait_turn(&turn.to_string(), &["completed"]).await;
    let app = h.app.restart().await;
    assert_eq!(snapshot(&app, &github).await["name"], "Engineering");
    assert_eq!(snapshot(&app, &main).await["name"], "Proliquid");
}

#[tokio::test]
async fn invalid_targets_and_names_leave_metadata_unchanged() {
    let h = harness(2).await;
    let main = h.app.main_bot().await;
    let github = h.app.create_bot("GitHub").await;
    let (turn, token) = h.running(&main, GATE).await;
    for args in [
        json!({ "target_bot_id": github }),
        json!({ "target_bot_id": "GitHub", "name": "Tech Manager" }),
        json!({ "target_bot_id": github, "name": 5 }),
        json!({ "target_bot_id": github, "name": "   " }),
        json!({ "target_bot_id": github, "name": "é".repeat(81) }),
        json!({ "target_bot_id": github, "name": "a\0b" }),
        json!({ "target_bot_id": github, "name": "Tech Manager", "avatar": {} }),
    ] {
        let called = tool(&h.app, &token, "rename_bot", args.clone()).await;
        assert!(called.error, "{args}: {}", called.body);
    }
    let missing = tool(
        &h.app,
        &token,
        "rename_bot",
        json!({ "target_bot_id": Uuid::new_v4(), "name": "Tech Manager" }),
    )
    .await;
    assert_eq!(missing.code(), "BOT_NOT_FOUND");
    assert_eq!(snapshot(&h.app, &github).await["name"], "GitHub");
    assert!(updates(&h.app, &github).await.is_empty());
    let deleted = h
        .app
        .owner("DELETE", &format!("/v1/bots/{github}"), None)
        .await;
    assert_eq!(deleted.status, StatusCode::OK);
    let after_delete = updates(&h.app, &github).await;
    let called = tool(
        &h.app,
        &token,
        "rename_bot",
        json!({ "target_bot_id": github, "name": "Tech Manager" }),
    )
    .await;
    assert_eq!(called.code(), "BOT_UNAVAILABLE");
    assert_eq!(updates(&h.app, &github).await, after_delete);
    h.release(turn).await;
}

#[tokio::test]
async fn delegated_owner_request_can_rename_a_bot() {
    let h = harness(2).await;
    let main = h.app.main_bot().await;
    let worker = h.app.create_bot("Work").await;
    let (root, token) = h.running(&main, GATE).await;
    let pending = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": worker, "task": format!("{GATE} Rename {main} to Proliquid.") }),
    )
    .await;
    assert!(!pending.error, "{}", pending.body);
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    let (_, child_token) = h.started(child).await;
    let called = tool(
        &h.app,
        &child_token,
        "rename_bot",
        json!({ "target_bot_id": main, "name": "Proliquid" }),
    )
    .await;
    assert!(!called.error, "{}", called.body);
    assert_eq!(snapshot(&h.app, &main).await["name"], "Proliquid");
    let parked = tool(
        &h.app,
        &token,
        "rename_bot",
        json!({ "target_bot_id": worker, "name": "Late" }),
    )
    .await;
    assert_eq!(parked.status, StatusCode::CONFLICT);
    assert!(matches!(
        bots::rename_for_turn(
            &h.app.db.pool,
            root,
            worker.parse().unwrap(),
            &BotName::parse("Late").unwrap()
        )
        .await,
        Err(BotError::Refused(Refusal::AttemptDelegated))
    ));
    h.release(child).await;
    h.release(root).await;
}

#[tokio::test]
async fn routine_runs_cannot_rename_even_with_a_forged_capability() {
    let h = harness(2).await;
    let main = h.app.main_bot().await;
    let (owner, token) = h.running(&main, GATE).await;
    let saved = tool(
        &h.app,
        &token,
        "routine_create",
        json!({ "name": "Check", "prompt": GATE, "cron": "0 9 1 1 *" }),
    )
    .await;
    assert!(!saved.error, "{}", saved.body);
    h.release(owner).await;
    h.app.wait_turn(&owner.to_string(), &["completed"]).await;
    let due = saved.result["routine"]["next_run_at"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    else_api::db::routines::admit_due(&h.app.db.pool, h.app.scheduler.registry(), due, 8)
        .await
        .unwrap();
    h.app.scheduler.wake();
    let routine: Uuid =
        sqlx::query_scalar("SELECT turn_id FROM routine_runs WHERE routine_id = ?1")
            .bind(
                saved.result["routine"]["id"]
                    .as_str()
                    .unwrap()
                    .parse::<Uuid>()
                    .unwrap(),
            )
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    let (_, token) = h.started(routine).await;
    let args = json!({ "target_bot_id": main, "name": "Unasked" });
    let denied = tool(&h.app, &token, "rename_bot", args.clone()).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    let conversation = h.inputs(routine).await[0].conversation_id;
    let forged = h
        .app
        .scheduler
        .grants()
        .issue(
            main.parse().unwrap(),
            conversation,
            routine,
            vec![Tool::EditBot],
            DEFAULT_TTL,
        )
        .await;
    let denied = tool(&h.app, &forged, "rename_bot", args).await;
    assert_eq!(denied.code(), "NOT_ALLOWED");
    let model = json!({ "target_bot_id": main, "model": "anything" });
    let denied = tool(&h.app, &token, "set_bot_model", model.clone()).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    let denied = tool(&h.app, &forged, "set_bot_model", model).await;
    assert!(denied.error, "{}", denied.body);
    let profile = else_api::app::providers::Profile {
        model: "gpt-6.1-sol".into(),
        reasoning_effort: "high".into(),
        service_tier: "default".into(),
    };
    assert!(matches!(
        bots::set_model_for_turn(
            &h.app.db.pool,
            routine,
            main.parse().unwrap(),
            "codex",
            &profile
        )
        .await,
        Err(BotError::Refused(Refusal::NotOwnerRequest))
    ));
    let profiles: i64 = sqlx::query_scalar("SELECT count(*) FROM bot_profiles WHERE bot_id = ?1")
        .bind(main.parse::<Uuid>().unwrap())
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(profiles, 0, "no model was saved");
    assert_eq!(snapshot(&h.app, &main).await["name"], "Assistant");
    h.release(routine).await;
}

#[tokio::test]
async fn completed_turn_cannot_rename_a_bot() {
    let h = harness(2).await;
    let main = h.app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    h.release(turn).await;
    h.app.wait_turn(&turn.to_string(), &["completed"]).await;
    let denied = tool(
        &h.app,
        &token,
        "rename_bot",
        json!({ "target_bot_id": main, "name": "Late" }),
    )
    .await;
    assert_eq!(denied.status, StatusCode::UNAUTHORIZED);
    assert!(matches!(
        bots::rename_for_turn(
            &h.app.db.pool,
            turn,
            main.parse().unwrap(),
            &BotName::parse("Late").unwrap()
        )
        .await,
        Err(BotError::Refused(Refusal::NotRunning))
    ));
    assert_eq!(snapshot(&h.app, &main).await["name"], "Assistant");
}
