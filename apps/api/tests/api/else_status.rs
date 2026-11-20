use crate::common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::{TestApp, TestDb};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::grok::Grok;
use serde_json::{Value, json};
use uuid::Uuid;

async fn grant(app: &TestApp, tools: Vec<Tool>) -> String {
    let bot = app.main_bot().await;
    let reply = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn = &reply.body["turn"];
    let turn: Uuid = turn["steered_into"]
        .as_str()
        .or(turn["id"].as_str())
        .unwrap()
        .parse()
        .unwrap();
    app.wait_turn(&turn.to_string(), &["running"]).await;
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    app.scheduler
        .grants()
        .issue(bot.parse().unwrap(), conversation, turn, tools, DEFAULT_TTL)
        .await
}

async fn else_status(app: &TestApp, token: &str) -> common::Reply {
    app.call(
        "POST",
        "/tools/v1/mcp",
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                     "params": { "name": "else_status", "arguments": {} } })),
        Some(token),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_reads_the_reason_a_provider_is_unavailable() {
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let registry = else_api::providers::Registry::new(vec![
        Arc::new(else_api::providers::synthetic::Synthetic::new(
            std::time::Duration::from_millis(5),
        )),
        Arc::new(Grok::detect(None, None, None)),
    ]);
    let app = TestApp::with_registry(Arc::new(db), 4, registry);

    let token = grant(&app, vec![Tool::Diagnose]).await;
    let reply = else_status(&app, &token).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let text = reply.body["result"]["content"][0]["text"].as_str().unwrap();
    let status: Value = serde_json::from_str(text).unwrap();
    assert_eq!(status["version"], else_api::VERSION, "{status}");
    let grok = status["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "grok")
        .unwrap_or_else(|| panic!("grok missing from {status}"));
    assert_eq!(grok["cli_available"], false, "{grok}");
    assert_eq!(
        grok["cli_unavailable_reason"], "Grok CLI is not configured on this host.",
        "{grok}"
    );

    let token = grant(&app, vec![Tool::ReadMemory]).await;
    let reply = else_status(&app, &token).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_tool_is_refused_without_consulting_the_grant() {
    let app = TestApp::installed().await;
    let token = grant(&app, vec![]).await;
    let reply = app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": "no_such_tool", "arguments": {} } })),
            Some(&token),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["result"]["isError"], true, "{}", reply.body);
    assert_eq!(
        reply.body["result"]["content"][0]["text"], "Unknown tool.",
        "{}",
        reply.body
    );
}

fn provider(status: &Value, id: &str) -> Value {
    status["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("{id} missing from {status}"))
}

async fn status_of(app: &TestApp, token: &str) -> Value {
    let reply = else_status(app, token).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    serde_json::from_str(reply.body["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_a_bot_runs_on_its_own_model_is_available_without_a_saved_default() {
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    sqlx::query("DELETE FROM provider_profiles WHERE provider_id = 'codex'")
        .execute(&db.pool)
        .await
        .unwrap();
    let registry = else_api::providers::Registry::new(vec![
        Arc::new(else_api::providers::synthetic::Synthetic::new(
            std::time::Duration::from_millis(5),
        )),
        Arc::new(common::ProfiledFake),
    ]);
    let app = TestApp::with_registry(Arc::new(db), 4, registry);
    let token = grant(&app, vec![Tool::Diagnose]).await;

    let codex = provider(&status_of(&app, &token).await, "codex");
    assert_eq!(codex["available"], false, "{codex}");
    assert!(
        codex["unavailable_reason"]
            .as_str()
            .is_some_and(|why| why.contains("model")),
        "{codex}"
    );

    let (bot, conversation, turn): (Uuid, Uuid, Uuid) =
        sqlx::query_as("SELECT bot_id, conversation_id, id FROM turns LIMIT 1")
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO routines (id, bot_id, conversation_id, name, prompt, cron, timezone,
             created_turn_id, created_name, request_hash, model, enabled)
         VALUES (?1, ?2, ?3, 'Digest', 'Summarize.', '0 9 * * *', 'UTC', ?4, 'Digest',
             zeroblob(32), ?5, false)",
    )
    .bind(Uuid::new_v4())
    .bind(bot)
    .bind(conversation)
    .bind(turn)
    .bind(
        json!({ "provider_id": "codex", "model": "gpt-6.1-sol",
                "reasoning_effort": "medium", "service_tier": "priority" })
        .to_string(),
    )
    .execute(&app.db.pool)
    .await
    .unwrap();
    let codex = provider(&status_of(&app, &token).await, "codex");
    assert_eq!(codex["available"], true, "{codex}");
    assert_eq!(codex["routines_with_own_model"], 1, "{codex}");
    assert_eq!(codex["bots_with_own_model"], 0, "{codex}");

    sqlx::query("DELETE FROM routines")
        .execute(&app.db.pool)
        .await
        .unwrap();
    let other: Uuid = app.create_bot("Scout").await.parse().unwrap();
    let own = else_api::app::providers::Profile {
        model: "gpt-6.1-sol".to_owned(),
        reasoning_effort: "medium".to_owned(),
        service_tier: "priority".to_owned(),
    };
    else_api::db::bot_profiles::put(&app.db.pool, other, "codex", &own)
        .await
        .unwrap();

    let codex = provider(&status_of(&app, &token).await, "codex");
    assert_eq!(codex["available"], true, "{codex}");
    assert_eq!(codex["unavailable_reason"], Value::Null, "{codex}");
    assert_eq!(codex["profile_configured"], false, "{codex}");
    assert_eq!(codex["bots_with_own_model"], 1, "{codex}");
    assert_eq!(codex["routines_with_own_model"], 0, "{codex}");
}
