use crate::common;

use axum::http::StatusCode;
use common::TestApp;
use common::recorder::{GATE, harness, tool};
use else_api::grants::Tool;
use serde_json::{Value, json};
use uuid::Uuid;

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
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let c = connection(app).await;
    let (_, token) = h.running(&main, GATE).await;
    let set = tool(
        app,
        &token,
        "social_trigger_set",
        json!({ "connection_id": c, "destination": "100", "wake": "mentions",
                "instructions": "Answer release questions and draft replies for my approval." }),
    )
    .await;
    let trigger = set.result["trigger"].clone();
    assert_eq!(trigger["destination_name"], "Team", "{}", set.body);
    assert_eq!(trigger["username"], "proliquid_bot");
    let id = trigger["id"].as_str().unwrap();

    let card = &shown(app, &main).await[0];
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
    assert_eq!(paused.body["trigger"]["id"], id);
    assert_eq!(paused.body["trigger"]["wake"], "mentions");
    assert_eq!(paused.body["trigger"]["last_woke_at"], Value::Null);
    assert_eq!(shown(app, &main).await[0]["enabled"], false);

    let deleted = app
        .owner(
            "DELETE",
            &format!("/v1/bots/{main}/social-triggers/{id}"),
            None,
        )
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert!(shown(app, &main).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn only_work_that_may_manage_routines_can_set_a_trigger() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let c = connection(app).await;
    let (turn, _) = h.running(&main, GATE).await;
    let token = h.grant_with(turn, vec![Tool::Social]).await;
    let refused = tool(
        app,
        &token,
        "social_trigger_set",
        json!({ "connection_id": c, "wake": "all", "instructions": "Log everything." }),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert!(shown(app, &main).await.is_empty());
}
