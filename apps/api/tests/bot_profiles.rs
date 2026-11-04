//! Each bot's own provider and model, and the installation's defaults that
//! bots without one use.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::json;
use uuid::Uuid;

fn choice(effort: &str, tier: &str) -> serde_json::Value {
    json!({ "provider_id": "codex", "model": "gpt-6.1-sol", "reasoning_effort": effort, "service_tier": tier })
}

/// The profile snapshot a turn was accepted with.
async fn turn_profile(app: &TestApp, turn: &str) -> serde_json::Value {
    sqlx::query_scalar::<_, sqlx::types::Json<serde_json::Value>>(
        "SELECT profile FROM turns WHERE id = ?1",
    )
    .bind(turn.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap()
    .0
}

async fn send(app: &TestApp, bot: &str) -> String {
    // No provider named: the bot's own (or the defaults) apply.
    let reply = app.send(bot, Uuid::new_v4(), "hello", None).await;
    assert!(reply.status.is_success(), "{}", reply.body);
    reply.body["turn"]["id"].as_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn bots_use_their_own_model_or_the_defaults() {
    let app = TestApp::installed().await;

    // Setup seeds Codex's default; the owner changes it.
    let defaults = app.owner("GET", "/v1/settings/defaults", None).await;
    assert_eq!(defaults.body["defaults"]["provider_id"], "codex");
    assert_eq!(defaults.body["defaults"]["reasoning_effort"], "medium");
    let set = app
        .owner(
            "PUT",
            "/v1/settings/defaults",
            Some(choice("low", "default")),
        )
        .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);
    let caps = app.owner("GET", "/v1/capabilities", None).await;
    assert_eq!(caps.body["defaults"], choice("low", "default"));
    let codex = caps.body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "codex")
        .unwrap()
        .clone();
    assert_eq!(codex["cli_available"], true);
    assert!(codex["catalog"]["models"].is_array());

    // Values the adapter does not apply exactly are refused.
    let invalid = app
        .owner(
            "PUT",
            "/v1/settings/defaults",
            Some(choice("ultra", "default")),
        )
        .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid.body["error"]["code"], "invalid_profile");
    let unknown = app
        .owner(
            "PUT",
            "/v1/settings/defaults",
            Some(json!({ "provider_id": "nope", "model": "x", "reasoning_effort": "low", "service_tier": "default" })),
        )
        .await;
    assert_eq!(unknown.body["error"]["code"], "unknown_provider");

    // A bot created with its own profile, and one without.
    let own = app
        .owner(
            "POST",
            "/v1/bots",
            Some(
                json!({ "request_id": Uuid::new_v4(), "name": "Deep", "role": "thinks",
                         "profile": choice("high", "priority") }),
            ),
        )
        .await;
    assert_eq!(own.status, StatusCode::CREATED, "{}", own.body);
    let own = own.body["bot"]["id"].as_str().unwrap().to_owned();
    let plain = app.create_bot("Plain", "uses defaults").await;
    let refused = app
        .owner(
            "POST",
            "/v1/bots",
            Some(
                json!({ "request_id": Uuid::new_v4(), "name": "Bad", "role": "x",
                         "profile": choice("ultra", "default") }),
            ),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "nothing created");
    assert_eq!(
        app.owner("GET", "/v1/bots", None).await.body["bots"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["name"] == "Bad")
            .count(),
        0
    );

    let shown = app
        .owner("GET", &format!("/v1/bots/{own}/profile"), None)
        .await;
    assert_eq!(shown.body["profile"]["own"], true);
    assert_eq!(shown.body["profile"]["reasoning_effort"], "high");
    let shown = app
        .owner("GET", &format!("/v1/bots/{plain}/profile"), None)
        .await;
    assert_eq!(shown.body["profile"]["own"], false);
    assert_eq!(shown.body["profile"]["reasoning_effort"], "low");

    // Turns snapshot the bot's own profile, else the defaults.
    let turn = send(&app, &own).await;
    assert_eq!(turn_profile(&app, &turn).await["reasoning_effort"], "high");
    assert_eq!(turn_profile(&app, &turn).await["service_tier"], "priority");
    let turn = send(&app, &plain).await;
    assert_eq!(turn_profile(&app, &turn).await["reasoning_effort"], "low");

    // Changing a bot's profile applies to its next message only.
    let changed = app
        .owner(
            "PUT",
            &format!("/v1/bots/{plain}/profile"),
            Some(choice("xhigh", "default")),
        )
        .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.body);
    assert_eq!(changed.body["profile"]["own"], true);
    let next = send(&app, &plain).await;
    assert_eq!(turn_profile(&app, &next).await["reasoning_effort"], "xhigh");
    assert_eq!(turn_profile(&app, &turn).await["reasoning_effort"], "low");

    let missing = app
        .owner(
            "PUT",
            &format!("/v1/bots/{}/profile", Uuid::new_v4()),
            Some(choice("low", "default")),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}
