//! POC-004: bot creation, listing, and soft deletion through the real routes.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::json;
use uuid::Uuid;

async fn create(app: &TestApp, request_id: Uuid, name: &str, role: &str) -> common::Reply {
    app.owner(
        "POST",
        "/v1/bots",
        Some(json!({ "request_id": request_id, "name": name, "role": role })),
    )
    .await
}

async fn row_counts(app: &TestApp) -> (i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM bots), (SELECT count(*) FROM conversations)")
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn create_retry_and_conflict() {
    let app = TestApp::installed().await;
    let request = Uuid::new_v4();
    let first = create(&app, request, "Work", "Help with my work").await;
    assert_eq!(first.status, StatusCode::CREATED);
    let bot = &first.body["bot"];
    let id = bot["id"].as_str().unwrap();
    assert_eq!(first.headers["location"], format!("/v1/bots/{id}"));
    assert_eq!(bot["name"], "Work");
    assert_eq!(bot["activity"]["phase"], "idle");
    assert!(bot["conversation_id"].is_string());

    // A lost response followed by the same request returns the same bot.
    let retry = create(&app, request, "Work", "Help with my work").await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["bot"]["id"], id);
    assert_eq!(retry.body["bot"]["conversation_id"], bot["conversation_id"]);

    // Changed input under the same request ID conflicts and creates nothing.
    let before = row_counts(&app).await;
    let conflict = create(&app, request, "Work", "Different role").await;
    assert_eq!(conflict.status, StatusCode::CONFLICT);
    assert_eq!(conflict.body["error"]["code"], "request_conflict");
    assert_eq!(conflict.body["error"]["request_id"], request.to_string());
    assert_eq!(row_counts(&app).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_retries_create_one_bot() {
    let app = Arc::new(TestApp::installed().await);
    let request = Uuid::new_v4();
    // Spawn every request before awaiting any, released together.
    let barrier = Arc::new(tokio::sync::Barrier::new(6));
    let calls: Vec<_> = (0..6)
        .map(|_| {
            let (app, barrier) = (app.clone(), barrier.clone());
            tokio::spawn(async move {
                barrier.wait().await;
                create(&app, request, "Family", "Family logistics").await
            })
        })
        .collect();
    let mut ids = Vec::new();
    let mut created = 0;
    for call in calls {
        let reply = call.await.unwrap();
        if reply.status == StatusCode::CREATED {
            created += 1;
        }
        ids.push(reply.body["bot"]["id"].as_str().unwrap().to_owned());
    }
    assert_eq!(created, 1);
    ids.dedup();
    assert_eq!(ids.len(), 1);
    assert_eq!(row_counts(&app).await, (2, 2));
}

#[tokio::test(flavor = "multi_thread")]
async fn validation_errors_create_nothing() {
    let app = TestApp::installed().await;
    let before = row_counts(&app).await;
    for (name, role) in [
        ("", "role"),
        ("   ", "role"),
        (&*"n".repeat(81), "role"),
        ("ok", " "),
    ] {
        let reply = create(&app, Uuid::new_v4(), name, role).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{name:?}/{role:?}");
        assert_eq!(reply.body["error"]["code"], "invalid_input");
    }
    let reply = app
        .owner(
            "POST",
            "/v1/bots",
            Some(json!({ "request_id": "nope", "name": "a", "role": "b" })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = app
        .owner(
            "POST",
            "/v1/bots",
            Some(
                json!({ "request_id": Uuid::new_v4(), "name": "a", "role": "b", "is_main": true }),
            ),
        )
        .await;
    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "unknown fields are rejected"
    );
    assert_eq!(row_counts(&app).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_select_and_duplicate_names() {
    let app = TestApp::installed().await;
    let a = create(&app, Uuid::new_v4(), "Twin", "First twin")
        .await
        .body["bot"]["id"]
        .clone();
    let b = create(&app, Uuid::new_v4(), "Twin", "Second twin")
        .await
        .body["bot"]["id"]
        .clone();
    assert_ne!(a, b);
    let listing = app.owner("GET", "/v1/bots", None).await;
    assert_eq!(listing.status, StatusCode::OK);
    let bots = listing.body["bots"].as_array().unwrap();
    assert_eq!(bots.len(), 3);
    let main = listing.body["default_bot_id"].clone();
    assert_eq!(bots[0]["id"], main, "main bot is oldest");
    assert_eq!(bots[1]["id"], a);
    assert_eq!(bots[2]["id"], b);
    for (id, role) in [(&a, "First twin"), (&b, "Second twin")] {
        let one = app
            .owner("GET", &format!("/v1/bots/{}", id.as_str().unwrap()), None)
            .await;
        assert_eq!(one.status, StatusCode::OK);
        assert_eq!(one.body["bot"]["role"], role);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_retains_data_and_is_repeatable() {
    let app = TestApp::installed().await;
    let id = create(&app, Uuid::new_v4(), "Work", "Help").await.body["bot"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let before = row_counts(&app).await;
    let first = app.owner("DELETE", &format!("/v1/bots/{id}"), None).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.body["bot"]["cleanup"]["state"], "done");
    assert!(first.body["bot"]["deleted_at"].is_string());
    assert!(
        first.body["bot"].get("role").is_none(),
        "receipt omits retained data"
    );
    let again = app.owner("DELETE", &format!("/v1/bots/{id}"), None).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body, first.body, "retry returns the same receipt");

    let listing = app
        .owner("GET", "/v1/bots?include_deleting=true", None)
        .await;
    assert!(
        listing.body["bots"]
            .as_array()
            .unwrap()
            .iter()
            .all(|bot| bot["id"] != id)
    );
    let receipt = app.owner("GET", &format!("/v1/bots/{id}"), None).await;
    assert_eq!(receipt.status, StatusCode::OK);
    assert_eq!(receipt.body["bot"]["cleanup"]["state"], "done");
    // Records and the conversation are retained.
    assert_eq!(row_counts(&app).await, before);
    // Creating again with a new request ID makes a new identity.
    let new = create(&app, Uuid::new_v4(), "Work", "Help").await;
    assert_ne!(new.body["bot"]["id"], id);
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_main_and_last_bot_clears_default_permanently() {
    let app = TestApp::installed().await;
    let listing = app.owner("GET", "/v1/bots", None).await;
    let main = listing.body["default_bot_id"].as_str().unwrap().to_owned();
    let other = create(&app, Uuid::new_v4(), "Work", "Help").await.body["bot"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Deleting a non-default bot leaves the default alone.
    app.owner("DELETE", &format!("/v1/bots/{other}"), None)
        .await;
    let listing = app.owner("GET", "/v1/bots", None).await;
    assert_eq!(listing.body["default_bot_id"], main);

    app.owner("DELETE", &format!("/v1/bots/{main}"), None).await;
    let listing = app.owner("GET", "/v1/bots", None).await;
    assert_eq!(listing.body["default_bot_id"], serde_json::Value::Null);
    assert_eq!(listing.body["bots"], json!([]));

    // Repeated setup and startup respect the empty installation.
    for _ in 0..2 {
        else_api::db::migrate(&app.db.pool).await.unwrap();
        let report = else_api::db::installation::setup(&app.db.pool)
            .await
            .unwrap();
        assert_eq!(
            report.main_bot,
            else_api::db::installation::MainBot::AlreadyInstalled(None)
        );
        assert_eq!(
            else_api::db::installation::check_installed(&app.db.pool)
                .await
                .unwrap(),
            None
        );
    }
    let listing = app.owner("GET", "/v1/bots", None).await;
    assert_eq!(listing.body["bots"], json!([]));
    assert_eq!(row_counts(&app).await, (2, 2), "tombstoned data retained");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_ids_and_authorization() {
    let app = TestApp::installed().await;
    let unknown = Uuid::new_v4();
    for (method, path) in [
        ("GET", format!("/v1/bots/{unknown}")),
        ("DELETE", format!("/v1/bots/{unknown}")),
        ("GET", "/v1/bots/not-a-uuid".to_owned()),
    ] {
        let reply = app.owner(method, &path, None).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {path}");
        assert_eq!(reply.body["error"]["code"], "not_found");
    }
    // Knowing a real bot ID does not bypass owner authentication.
    let main = app.owner("GET", "/v1/bots", None).await.body["default_bot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    for (method, path, body) in [
        ("GET", "/v1/bots".to_owned(), None),
        ("GET", format!("/v1/bots/{main}"), None),
        ("DELETE", format!("/v1/bots/{main}"), None),
        (
            "POST",
            "/v1/bots".to_owned(),
            Some(json!({ "request_id": Uuid::new_v4(), "name": "x", "role": "y" })),
        ),
    ] {
        for token in [None, Some("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB")] {
            let reply = app.call(method, &path, body.clone(), token).await;
            assert_eq!(
                reply.status,
                StatusCode::UNAUTHORIZED,
                "{method} {path} {token:?}"
            );
        }
    }
    let still = app.owner("GET", &format!("/v1/bots/{main}"), None).await;
    assert!(
        still.body["bot"]["cleanup"].is_null(),
        "unauthorized delete had no effect"
    );
    assert_eq!(row_counts(&app).await, (1, 1));
}
