//! POC-004: bot creation, listing, and soft deletion through the real routes.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::json;
use uuid::Uuid;

async fn create(app: &TestApp, request_id: Uuid, name: &str) -> common::Reply {
    app.owner(
        "POST",
        "/v1/bots",
        Some(json!({ "request_id": request_id, "name": name })),
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
    let first = create(&app, request, "Work").await;
    assert_eq!(first.status, StatusCode::CREATED);
    let bot = &first.body["bot"];
    let id = bot["id"].as_str().unwrap();
    assert_eq!(first.headers["location"], format!("/v1/bots/{id}"));
    assert_eq!(bot["name"], "Work");
    assert!(bot.get("role").is_none(), "bots have no description");
    assert_eq!(bot["activity"]["phase"], "idle");
    assert!(bot["conversation_id"].is_string());

    // A lost response followed by the same request returns the same bot.
    let retry = create(&app, request, "Work").await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["bot"]["id"], id);
    assert_eq!(retry.body["bot"]["conversation_id"], bot["conversation_id"]);

    // Changed input under the same request ID conflicts and creates nothing.
    let before = row_counts(&app).await;
    let conflict = create(&app, request, "Other").await;
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
                create(&app, request, "Family").await
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
    for name in ["", "   ", &*"n".repeat(81), "a\0b"] {
        let reply = create(&app, Uuid::new_v4(), name).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{name:?}");
        assert_eq!(reply.body["error"]["code"], "invalid_input");
    }
    let reply = app
        .owner(
            "POST",
            "/v1/bots",
            Some(json!({ "request_id": "nope", "name": "a" })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = app
        .owner(
            "POST",
            "/v1/bots",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "a", "is_main": true })),
        )
        .await;
    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "unknown fields are rejected"
    );
    let reply = app
        .owner(
            "POST",
            "/v1/bots",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "a", "role": "Help" })),
        )
        .await;
    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "descriptions are no longer accepted"
    );
    assert_eq!(row_counts(&app).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_select_and_duplicate_names() {
    let app = TestApp::installed().await;
    let a = create(&app, Uuid::new_v4(), "Twin").await.body["bot"]["id"].clone();
    let b = create(&app, Uuid::new_v4(), "Twin").await.body["bot"]["id"].clone();
    assert_ne!(a, b);
    let listing = app.owner("GET", "/v1/bots", None).await;
    assert_eq!(listing.status, StatusCode::OK);
    let bots = listing.body["bots"].as_array().unwrap();
    assert_eq!(bots.len(), 3);
    let main = listing.body["default_bot_id"].clone();
    assert_eq!(bots[0]["id"], main, "main bot is oldest");
    assert_eq!(bots[1]["id"], a);
    assert_eq!(bots[2]["id"], b);
    for id in [&a, &b] {
        let one = app
            .owner("GET", &format!("/v1/bots/{}", id.as_str().unwrap()), None)
            .await;
        assert_eq!(one.status, StatusCode::OK);
        assert_eq!(one.body["bot"]["name"], "Twin");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_retains_data_and_is_repeatable() {
    let app = TestApp::installed().await;
    let id = create(&app, Uuid::new_v4(), "Work").await.body["bot"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let before = row_counts(&app).await;
    let first = app.owner("DELETE", &format!("/v1/bots/{id}"), None).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.body["bot"]["cleanup"]["state"], "done");
    assert!(first.body["bot"]["deleted_at"].is_string());
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
    let new = create(&app, Uuid::new_v4(), "Work").await;
    assert_ne!(new.body["bot"]["id"], id);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_main_bot_cannot_be_deleted() {
    let app = TestApp::installed().await;
    let listing = app.owner("GET", "/v1/bots", None).await;
    let main = listing.body["default_bot_id"].as_str().unwrap().to_owned();
    let other = create(&app, Uuid::new_v4(), "Work").await.body["bot"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Deleting another bot leaves the main bot alone.
    app.owner("DELETE", &format!("/v1/bots/{other}"), None)
        .await;
    let listing = app.owner("GET", "/v1/bots", None).await;
    assert_eq!(listing.body["default_bot_id"], main);

    // The main bot is refused, and stays the active main bot.
    let refused = app.owner("DELETE", &format!("/v1/bots/{main}"), None).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], "main_bot");
    let listing = app.owner("GET", "/v1/bots", None).await;
    assert_eq!(listing.body["default_bot_id"], main);
    let bot = app.owner("GET", &format!("/v1/bots/{main}"), None).await;
    assert_eq!(bot.body["bot"]["deleted_at"], serde_json::Value::Null);
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
            Some(json!({ "request_id": Uuid::new_v4(), "name": "x" })),
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

async fn update(app: &TestApp, id: &str, body: serde_json::Value) -> common::Reply {
    app.owner("PATCH", &format!("/v1/bots/{id}"), Some(body))
        .await
}

async fn updated_events(app: &TestApp, id: &str) -> Vec<serde_json::Value> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT data FROM bot_events WHERE bot_id = ?1 AND kind = 'bot.updated' ORDER BY seq",
    )
    .bind(id.parse::<Uuid>().unwrap())
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    rows.iter()
        .map(|row| serde_json::from_str(row).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn metadata_update_saves_once_and_keeps_everything_else() {
    let app = TestApp::installed().await;
    let category = app
        .owner(
            "POST",
            "/v1/categories",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "Work" })),
        )
        .await
        .body["category"]["id"]
        .clone();
    let id = app.create_bot("Work").await;
    app.owner(
        "PATCH",
        &format!("/v1/bots/{id}/category"),
        Some(json!({ "category_id": category })),
    )
    .await;
    let before = app.owner("GET", &format!("/v1/bots/{id}"), None).await.body["bot"].clone();
    let main = app.main_bot().await;
    let journaled = updated_events(&app, &id).await.len();

    let saved = update(&app, &id, json!({ "name": "  Planner  " })).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    let bot = &saved.body["bot"];
    assert_eq!(bot["name"], "Planner", "the name is trimmed");
    assert_eq!(bot["category_id"], category);
    assert_eq!(bot["conversation_id"], before["conversation_id"]);
    let events = updated_events(&app, &id).await;
    assert_eq!(
        events.len(),
        journaled + 1,
        "one bot.updated for the change"
    );
    let last = events.last().unwrap();
    assert_eq!(last["bot"]["name"], "Planner");

    let again = update(&app, &id, json!({ "name": "Planner" })).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body, saved.body);
    assert_eq!(updated_events(&app, &id).await.len(), journaled + 1);

    let listing = app.owner("GET", "/v1/bots", None).await;
    assert_eq!(listing.body["default_bot_id"], main, "main bot unchanged");
    let listed = listing.body["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == id)
        .unwrap()
        .clone();
    assert_eq!(listed, saved.body["bot"]);

    let renamed = update(&app, &main, json!({ "name": "Home" })).await;
    assert_eq!(renamed.status, StatusCode::OK);
    let listing = app.owner("GET", "/v1/bots", None).await;
    assert_eq!(listing.body["default_bot_id"], main);
}

#[tokio::test(flavor = "multi_thread")]
async fn metadata_update_validates_and_refuses_deleted_bots() {
    let app = TestApp::installed().await;
    let id = app.create_bot("Work").await;
    for body in [
        json!({ "name": " " }),
        json!({ "name": "n".repeat(81) }),
        json!({}),
        json!({ "name": "ok", "role": "Help" }),
        json!({ "name": "ok", "category_id": null }),
    ] {
        let reply = update(&app, &id, body.clone()).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
    }
    let bot = app.owner("GET", &format!("/v1/bots/{id}"), None).await;
    assert_eq!(
        bot.body["bot"]["name"], "Work",
        "rejected input changes nothing"
    );
    assert!(updated_events(&app, &id).await.is_empty());

    let unknown = update(&app, &Uuid::new_v4().to_string(), json!({ "name": "a" })).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);

    app.owner("DELETE", &format!("/v1/bots/{id}"), None).await;
    let gone = update(&app, &id, json!({ "name": "Back" })).await;
    assert_eq!(gone.status, StatusCode::GONE);
    assert_eq!(gone.body["error"]["code"], "bot_deleted");
    let receipt = app.owner("GET", &format!("/v1/bots/{id}"), None).await;
    assert_eq!(receipt.body["bot"]["cleanup"]["state"], "done");
    assert_eq!(receipt.body["bot"]["name"], "Work");

    let unauthorized = app
        .call(
            "PATCH",
            &format!("/v1/bots/{id}"),
            Some(json!({ "name": "a" })),
            None,
        )
        .await;
    assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED);
}
