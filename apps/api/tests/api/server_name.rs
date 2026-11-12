use crate::common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

async fn advertised(app: &TestApp) -> Value {
    app.owner("GET", "/v1/capabilities", None).await.body["server"]["name"].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owner_names_the_server_for_every_desktop_and_restart() {
    let app = TestApp::installed().await;
    assert_eq!(
        advertised(&app).await,
        Value::Null,
        "a new server has no name"
    );

    let named = app
        .owner(
            "PUT",
            "/v1/settings/server",
            Some(json!({ "name": "  Studio  " })),
        )
        .await;
    assert_eq!(named.status, StatusCode::OK, "{:?}", named.body);
    assert_eq!(named.body["server"]["name"], "Studio");
    assert_eq!(advertised(&app).await, "Studio");

    for name in [String::new(), "   ".into(), "a\nb".into(), "x".repeat(81)] {
        let refused = app
            .owner("PUT", "/v1/settings/server", Some(json!({ "name": name })))
            .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{name:?}");
    }
    let longest = "é".repeat(80);
    let accepted = app
        .owner(
            "PUT",
            "/v1/settings/server",
            Some(json!({ "name": longest })),
        )
        .await;
    assert_eq!(accepted.status, StatusCode::OK, "80 characters fit");

    let carried = app
        .owner(
            "PUT",
            "/v1/settings/server",
            Some(json!({ "name": "Laptop", "only_if_unnamed": true })),
        )
        .await;
    assert_eq!(carried.status, StatusCode::OK, "{:?}", carried.body);
    assert_eq!(
        carried.body["server"]["name"],
        longest.as_str(),
        "a carried name keeps the name another desktop gave"
    );

    let app = app.restart().await;
    assert_eq!(advertised(&app).await, longest.as_str());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_carried_name_names_an_unnamed_server() {
    let app = TestApp::installed().await;
    for (name, kept) in [("Laptop", "Laptop"), ("Desk", "Laptop")] {
        let carried = app
            .owner(
                "PUT",
                "/v1/settings/server",
                Some(json!({ "name": name, "only_if_unnamed": true })),
            )
            .await;
        assert_eq!(carried.body["server"]["name"], kept);
    }
    assert_eq!(advertised(&app).await, "Laptop");
}
