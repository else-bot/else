use crate::common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn onboarding_names_the_main_bot_and_sets_its_model_and_the_defaults() {
    let app = TestApp::installed().await;
    let caps = app.owner("GET", "/v1/capabilities", None).await;
    assert_eq!(caps.body["onboarding_required"], true);
    let main = app.main_bot().await;
    let profile = json!({ "provider_id": "codex", "model": "gpt-6.1-sol",
                          "reasoning_effort": "high", "service_tier": "priority" });

    let bad = app
        .owner(
            "POST",
            "/v1/onboarding",
            Some(json!({ "name": "  ", "profile": profile })),
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let bad = app
        .owner(
            "POST",
            "/v1/onboarding",
            Some(
                json!({ "name": "Jarvis", "profile": { "provider_id": "codex",
                "model": "gpt-6.1-sol", "reasoning_effort": "ultra", "service_tier": "default" } }),
            ),
        )
        .await;
    assert_eq!(bad.body["error"]["code"], "invalid_profile");
    assert_eq!(
        app.owner("GET", "/v1/capabilities", None).await.body["onboarding_required"],
        true
    );

    let done = app
        .owner(
            "POST",
            "/v1/onboarding",
            Some(json!({ "name": " Jarvis ", "profile": profile })),
        )
        .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert_eq!(done.body["main_bot_id"], main);

    let caps = app.owner("GET", "/v1/capabilities", None).await;
    assert_eq!(caps.body["onboarding_required"], false);
    assert_eq!(caps.body["defaults"], profile);
    let bot = app.owner("GET", &format!("/v1/bots/{main}"), None).await;
    assert_eq!(bot.body["bot"]["name"], "Jarvis");
    let read = &bot.body["bot"]["read_state"];
    assert!(read["latest"].is_object(), "greeted: {read}");
    assert_eq!(read["unread"], false, "the greeting is not news");
    let own = app
        .owner("GET", &format!("/v1/bots/{main}/profile"), None)
        .await;
    assert_eq!(own.body["profile"]["own"], true);
    assert_eq!(own.body["profile"]["reasoning_effort"], "high");
    assert_eq!(own.body["profile"]["service_tier"], "priority");
}

const JARVIS_WELCOME: &str = "Hi, I'm Jarvis. Welcome to else.bot.

I'm your main assistant. Tell me what you need in your own words, whether it's a question, something you're working on, or a task you want me to handle.

I remember our conversations, so we can build on what you've already told me about your work, your life, and how you like things done. You can correct me whenever I get something wrong.

I have my own computer for browsing, writing code, and working with files. You can open its desktop to see what I'm doing. You can also create other bots for specific roles, and I can hand tasks to them when it helps.

To get acquainted, what should I call you? Tell me a little about yourself and what you'd like my help with first. We can figure out the rest as we go.";

fn codex() -> serde_json::Value {
    json!({ "provider_id": "codex", "model": "gpt-6.1-sol",
            "reasoning_effort": "high", "service_tier": "priority" })
}

async fn onboard(app: &TestApp, name: &str) -> common::Reply {
    app.owner(
        "POST",
        "/v1/onboarding",
        Some(json!({ "name": name, "profile": codex() })),
    )
    .await
}

async fn conversation(app: &TestApp, bot: &str) -> serde_json::Value {
    let reply = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.body
}

async fn memory(app: &TestApp, bot: &str) -> Vec<serde_json::Value> {
    app.owner("GET", &format!("/v1/bots/{bot}/memory"), None)
        .await
        .body["entries"]
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_onboarding_greets_the_owner_once_by_the_chosen_name() {
    let app = TestApp::installed().await;
    let main = app.main_bot().await;

    let (a, b) = tokio::join!(onboard(&app, " Jarvis "), onboard(&app, " Jarvis "));
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(b.status, StatusCode::OK, "{}", b.body);

    let snapshot = conversation(&app, &main).await;
    let items = snapshot["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{snapshot}");
    let item = &items[0];
    assert_eq!(item["kind"], "assistant");
    assert_eq!(item["text"], JARVIS_WELCOME);
    assert_eq!(item["seq"], 1);
    assert!(item["provider_id"].is_null() && item["request_id"].is_null());
    let turns = snapshot["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 1, "{snapshot}");
    let turn = &turns[0];
    assert_eq!(turn["id"], item["turn_id"]);
    assert_eq!(turn["turn_no"], 1);
    assert_eq!(turn["status"], "completed");
    assert_eq!(turn["provider_id"], "codex");
    assert!(turn["profile_revision"].is_null() && turn["error"].is_null());
    assert!(!turn["finished_at"].is_null());
    assert_eq!(snapshot["activity"]["phase"], "idle", "{snapshot}");

    let entries = memory(&app, &main).await;
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["kind"], "assistant_message");
    assert_eq!(entries[0]["body"], JARVIS_WELCOME);
    assert_eq!(entries[0]["source"]["item_id"], item["id"]);

    let again = onboard(&app, "Friday").await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    let bot = app.owner("GET", &format!("/v1/bots/{main}"), None).await;
    assert_eq!(bot.body["bot"]["name"], "Friday");
    assert_eq!(conversation(&app, &main).await["items"], snapshot["items"]);
    let events: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM bot_events WHERE bot_id = ?1 ORDER BY seq")
            .bind(main.parse::<uuid::Uuid>().unwrap())
            .fetch_all(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(
        events,
        [
            "item.upsert",
            "turn.updated",
            "bot.updated",
            "bot.updated",
            "bot.updated"
        ]
    );

    let app = app.restart().await;
    let after = conversation(&app, &main).await;
    assert_eq!(
        after["items"], snapshot["items"],
        "persisted across restart"
    );
    assert_eq!(after["turns"], snapshot["turns"]);
    assert_eq!(memory(&app, &main).await.len(), 1);
    let attempts: Vec<i64> = sqlx::query_scalar("SELECT attempt FROM turns")
        .fetch_all(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(attempts, [0], "no provider was asked for the greeting");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_onboarding_leaves_no_greeting_and_its_retry_greets_once() {
    let app = TestApp::installed().await;
    let main = app.main_bot().await;
    sqlx::query(
        "CREATE TRIGGER fail_events BEFORE INSERT ON bot_events
         BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
    )
    .execute(&app.db.pool)
    .await
    .unwrap();

    let failed = onboard(&app, "Jarvis").await;
    assert!(failed.status.is_server_error(), "{}", failed.body);
    let caps = app.owner("GET", "/v1/capabilities", None).await;
    assert_eq!(caps.body["onboarding_required"], true);
    let bot = app.owner("GET", &format!("/v1/bots/{main}"), None).await;
    assert_eq!(bot.body["bot"]["name"], "Assistant");
    let snapshot = conversation(&app, &main).await;
    assert_eq!(snapshot["items"], json!([]));
    assert_eq!(snapshot["turns"], json!([]));
    assert!(memory(&app, &main).await.is_empty());

    sqlx::query("DROP TRIGGER fail_events")
        .execute(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(onboard(&app, "Jarvis").await.status, StatusCode::OK);
    let items = conversation(&app, &main).await["items"].clone();
    assert_eq!(items.as_array().unwrap().len(), 1);
    assert_eq!(items[0]["text"], JARVIS_WELCOME);
}

#[tokio::test(flavor = "multi_thread")]
async fn used_installations_and_other_bots_are_not_greeted() {
    let app = TestApp::installed().await;
    let main = app.main_bot().await;
    let work = app.create_bot("Work").await;
    let sent = app
        .send(
            &main,
            uuid::Uuid::new_v4(),
            "Already talking.",
            Some("synthetic"),
        )
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED);
    app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let before = conversation(&app, &main).await;

    assert_eq!(onboard(&app, "Jarvis").await.status, StatusCode::OK);
    let caps = app.owner("GET", "/v1/capabilities", None).await;
    assert_eq!(caps.body["onboarding_required"], false);
    let after = conversation(&app, &main).await;
    assert_eq!(after["items"], before["items"]);
    assert_eq!(after["turns"], before["turns"]);
    assert_eq!(conversation(&app, &work).await["items"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_next_owner_message_carries_the_greeting_without_a_handoff() {
    use std::sync::Arc;
    use std::time::Duration;
    let db = common::TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let synthetic = Arc::new(else_api::providers::synthetic::Synthetic::new(
        Duration::from_millis(1),
    ));
    let registry =
        else_api::providers::Registry::new(vec![Arc::new(common::ProfiledFake), synthetic.clone()]);
    let app = TestApp::with_registry(Arc::new(db), 4, registry);
    let main = app.main_bot().await;
    assert_eq!(onboard(&app, "Jarvis").await.status, StatusCode::OK);

    let sent = app
        .send(
            &main,
            uuid::Uuid::new_v4(),
            "What can you do?",
            Some("synthetic"),
        )
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let turn = sent.body["turn"]["id"].as_str().unwrap();
    assert_eq!(
        app.wait_turn(turn, &["completed", "failed"]).await,
        "completed"
    );
    let packet = synthetic.packets().await.pop().unwrap();
    assert!(
        packet.contains(&format!("[turn 1] Assistant: {JARVIS_WELCOME}\n")),
        "{packet}"
    );
    assert!(
        packet.ends_with("Owner's latest message:\nWhat can you do?"),
        "{packet}"
    );
    assert!(!packet.contains("[else.bot handoff]"), "{packet}");
    let handoffs: i64 = sqlx::query_scalar("SELECT count(*) FROM handoffs")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(handoffs, 0);
}
