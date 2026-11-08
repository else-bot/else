mod common;

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
    let turn: Uuid = reply.body["turn"]["id"].as_str().unwrap().parse().unwrap();
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
