use crate::common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::{TestApp, TestDb};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::synthetic::Synthetic;
use serde_json::{Value, json};
use uuid::Uuid;

const SLOW: &str = "[synthetic:slow] wake up when someone mentions you in the team chat.";

async fn app() -> TestApp {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let synthetic = Arc::new(Synthetic::new(Duration::from_millis(5)));
    TestApp::with_registry(db, 4, else_api::providers::Registry::new(vec![synthetic]))
}

async fn connection(app: &TestApp) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO social_connections(id,provider,remote_id,display_name,username,credential_salt,credential)
         VALUES(?1,'telegram','42','Proliquid','proliquid_bot',x'00',x'00')",
    )
    .bind(id)
    .execute(&app.db.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO social_destinations(connection_id,id,info) VALUES(?1,'100',?2)")
        .bind(id)
        .bind(json!({"id":"100","name":"Team","kind":"group","can_send":true}).to_string())
        .execute(&app.db.pool)
        .await
        .unwrap();
    id
}

async fn running(app: &TestApp, bot: &str, tools: Vec<Tool>) -> String {
    let sent = app.send(bot, Uuid::new_v4(), SLOW, Some("synthetic")).await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["running"]).await;
    let turn: Uuid = turn.parse().unwrap();
    let (bot, conversation): (Uuid, Uuid) =
        sqlx::query_as("SELECT bot_id, conversation_id FROM turns WHERE id = ?1")
            .bind(turn)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    app.scheduler
        .grants()
        .issue(bot, conversation, turn, tools, DEFAULT_TTL)
        .await
}

async fn tool(app: &TestApp, token: &str, name: &str, args: Value) -> common::Reply {
    app.call(
        "POST",
        "/tools/v1/mcp",
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                     "params": { "name": name, "arguments": args } })),
        Some(token),
    )
    .await
}

fn result(reply: &common::Reply) -> Value {
    let text = reply.body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    serde_json::from_str(text).unwrap_or_else(|_| json!(text))
}

async fn shown(app: &TestApp, bot: &str) -> Vec<Value> {
    app.owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body["social_triggers"]
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_watches_a_chat_and_the_owner_pauses_and_deletes_it_from_its_sidebar() {
    let app = app().await;
    let main = app.main_bot().await;
    let c = connection(&app).await;
    let token = running(&app, &main, vec![Tool::Routines, Tool::Social]).await;
    let set = tool(
        &app,
        &token,
        "social_trigger_set",
        json!({ "connection_id": c, "destination": "100", "wake": "mentions",
                "instructions": "Answer release questions and draft replies for my approval." }),
    )
    .await;
    let trigger = result(&set)["trigger"].clone();
    assert_eq!(trigger["destination_name"], "Team", "{}", set.body);
    assert_eq!(trigger["username"], "proliquid_bot");
    let id = trigger["id"].as_str().unwrap();

    let card = &shown(&app, &main).await[0];
    assert_eq!(
        (card["id"].as_str(), card["enabled"].as_bool()),
        (Some(id), Some(true))
    );

    let paused = app
        .owner(
            "PATCH",
            &format!("/v1/bots/{main}/social-triggers/{id}"),
            Some(json!({ "enabled": false })),
        )
        .await;
    assert_eq!(paused.status, StatusCode::OK, "{}", paused.body);
    assert_eq!(shown(&app, &main).await[0]["enabled"], false);

    let deleted = app
        .owner(
            "DELETE",
            &format!("/v1/bots/{main}/social-triggers/{id}"),
            None,
        )
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert!(shown(&app, &main).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn only_work_that_may_manage_routines_can_set_a_trigger() {
    let app = app().await;
    let main = app.main_bot().await;
    let c = connection(&app).await;
    let token = running(&app, &main, vec![Tool::Social]).await;
    let refused = tool(
        &app,
        &token,
        "social_trigger_set",
        json!({ "connection_id": c, "wake": "all", "instructions": "Log everything." }),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert!(shown(&app, &main).await.is_empty());
}
