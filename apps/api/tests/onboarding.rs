//! The welcome screen's step: name the main bot and choose its provider
//! and model, which become the defaults.

mod common;

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

    // Refused input changes nothing.
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
    let own = app
        .owner("GET", &format!("/v1/bots/{main}/profile"), None)
        .await;
    assert_eq!(own.body["profile"]["own"], true);
    assert_eq!(own.body["profile"]["reasoning_effort"], "high");
    assert_eq!(own.body["profile"]["service_tier"], "priority");
}
