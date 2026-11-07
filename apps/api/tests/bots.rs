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

    let retry = create(&app, request, "Work").await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["bot"]["id"], id);
    assert_eq!(retry.body["bot"]["conversation_id"], bot["conversation_id"]);

    let before = row_counts(&app).await;
    let conflict = create(&app, request, "Other").await;
    assert_eq!(conflict.status, StatusCode::CONFLICT);
    assert_eq!(conflict.body["error"]["code"], "request_conflict");
    assert_eq!(conflict.body["error"]["request_id"], request.to_string());
    assert_eq!(row_counts(&app).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn avatar_is_saved_with_creation_and_kept_on_retry() {
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
    let request_id = Uuid::new_v4();
    let avatar = json!({ "shape": "cloud", "color": "pink" });
    let payload = json!({
        "request_id": request_id,
        "name": "Research",
        "category_id": category,
        "avatar": avatar,
    });
    let created = app.owner("POST", "/v1/bots", Some(payload.clone())).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.body["bot"]["avatar"], avatar);
    assert_eq!(created.body["bot"]["category_id"], category);
    let id = created.body["bot"]["id"].as_str().unwrap();
    let path = format!("/v1/bots/{id}");
    let (shown, listing) = tokio::join!(
        app.owner("GET", &path, None),
        app.owner("GET", "/v1/bots", None),
    );
    assert_eq!(shown.body["bot"]["avatar"], avatar);
    assert_eq!(listed(&listing, id)["avatar"], avatar);
    assert!(updated_events(&app, id).await.is_empty());

    let retry = app.owner("POST", "/v1/bots", Some(payload.clone())).await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body, created.body);
    let before = row_counts(&app).await;
    for replacement in [
        Some(json!({ "shape": "gem", "color": "pink" })),
        Some(json!({ "shape": "cloud", "color": "teal" })),
        None,
    ] {
        let mut changed = payload.clone();
        match replacement {
            Some(avatar) => changed["avatar"] = avatar,
            None => {
                changed.as_object_mut().unwrap().remove("avatar");
            }
        }
        let conflict = app.owner("POST", "/v1/bots", Some(changed)).await;
        assert_eq!(conflict.status, StatusCode::CONFLICT);
        assert_eq!(conflict.body["error"]["code"], "request_conflict");
    }
    assert_eq!(row_counts(&app).await, before);

    let later = json!({ "shape": "gem", "color": "teal" });
    update(&app, id, json!({ "avatar": later })).await;
    let retry = app.owner("POST", "/v1/bots", Some(payload)).await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["bot"]["avatar"], later);
    let id = id.to_owned();
    let app = app.restart().await;
    let listing = app.owner("GET", "/v1/bots", None).await;
    assert_eq!(listed(&listing, &id)["avatar"], later);
}

#[tokio::test(flavor = "multi_thread")]
async fn creation_without_avatar_replays_legacy_receipts() {
    let app = TestApp::installed().await;
    let request_id = Uuid::new_v4();
    let created = create(&app, request_id, "Work").await;
    assert_eq!(created.status, StatusCode::CREATED);
    let legacy_hash: &[u8] = &[
        179, 184, 144, 166, 100, 102, 252, 163, 106, 38, 150, 227, 229, 119, 21, 82, 157, 228, 90,
        56, 165, 15, 56, 135, 41, 43, 232, 115, 5, 209, 129, 198,
    ];
    sqlx::query("UPDATE request_receipts SET payload_hash = ?2 WHERE request_id = ?1")
        .bind(request_id)
        .bind(legacy_hash)
        .execute(&app.db.pool)
        .await
        .unwrap();
    let retry = create(&app, request_id, "Work").await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body, created.body);
    let null_avatar = app
        .owner(
            "POST",
            "/v1/bots",
            Some(json!({ "request_id": request_id, "name": "Work", "avatar": null })),
        )
        .await;
    assert_eq!(null_avatar.status, StatusCode::OK);
    assert_eq!(null_avatar.body, created.body);
    assert_eq!(row_counts(&app).await, (2, 2));
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_avatar_on_create_leaves_no_bot() {
    let app = TestApp::installed().await;
    let before = row_counts(&app).await;
    for avatar in [
        json!({ "shape": "dragon", "color": "blue" }),
        json!({ "shape": "star", "color": "#3b82f6" }),
        json!({ "shape": "star" }),
        json!({ "shape": "star", "color": "blue", "image_url": "x" }),
        json!("star"),
    ] {
        let reply = app
            .owner(
                "POST",
                "/v1/bots",
                Some(json!({ "request_id": Uuid::new_v4(), "name": "Work", "avatar": avatar })),
            )
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{avatar}");
    }
    assert_eq!(row_counts(&app).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_retries_create_one_bot() {
    let app = Arc::new(TestApp::installed().await);
    let request = Uuid::new_v4();
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
    assert_eq!(row_counts(&app).await, before);
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

    app.owner("DELETE", &format!("/v1/bots/{other}"), None)
        .await;
    let listing = app.owner("GET", "/v1/bots", None).await;
    assert_eq!(listing.body["default_bot_id"], main);

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
    assert!(
        bot["metadata_revision"].as_i64().unwrap() > before["metadata_revision"].as_i64().unwrap()
    );
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
    assert_eq!(last["bot"]["metadata_revision"], bot["metadata_revision"]);

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
async fn muting_persists_journals_once_and_keeps_the_name() {
    let app = TestApp::installed().await;
    let id = app.create_bot("Watcher").await;
    let before = app.owner("GET", &format!("/v1/bots/{id}"), None).await.body["bot"].clone();
    assert_eq!(before["muted"], false, "bots start unmuted");
    let journaled = updated_events(&app, &id).await.len();

    let muted = update(&app, &id, json!({ "muted": true })).await;
    assert_eq!(muted.status, StatusCode::OK, "{}", muted.body);
    assert_eq!(muted.body["bot"]["muted"], true);
    assert_eq!(muted.body["bot"]["name"], "Watcher");
    let events = updated_events(&app, &id).await;
    assert_eq!(events.len(), journaled + 1);
    assert_eq!(events.last().unwrap()["bot"]["muted"], true);

    let again = update(&app, &id, json!({ "muted": true })).await;
    assert_eq!(again.body, muted.body, "muting twice changes nothing");
    assert_eq!(updated_events(&app, &id).await.len(), journaled + 1);

    let renamed = update(&app, &id, json!({ "name": "Watch" })).await;
    assert_eq!(
        renamed.body["bot"]["muted"], true,
        "a rename keeps the mute"
    );

    let unmuted = update(&app, &id, json!({ "muted": false })).await;
    assert_eq!(unmuted.body["bot"]["muted"], false);
    let listed = app.owner("GET", "/v1/bots", None).await.body["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == id)
        .unwrap()
        .clone();
    assert_eq!(listed, unmuted.body["bot"]);

    let invalid = update(&app, &id, json!({ "muted": "yes" })).await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
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

fn listed(listing: &common::Reply, id: &str) -> serde_json::Value {
    listing.body["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == id)
        .unwrap()
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn avatars_persist_with_the_name_and_journal_only_changes() {
    let app = TestApp::installed().await;
    let main = app.main_bot().await;
    let shown = app.owner("GET", &format!("/v1/bots/{main}"), None).await;
    assert_eq!(
        shown.body["bot"]["avatar"],
        json!({ "shape": "star", "color": "blue" }),
        "the main bot starts as the default"
    );
    let id = app.create_bot("Work").await;
    let created = app.owner("GET", &format!("/v1/bots/{id}"), None).await;
    let initial = created.body["bot"]["avatar"].clone();
    assert!(initial["shape"].is_string() && initial["color"].is_string());

    let changed_avatar = if initial["shape"] == "cloud" {
        json!({ "shape": "star", "color": "pink" })
    } else {
        json!({ "shape": "cloud", "color": "pink" })
    };
    let saved = update(&app, &id, json!({ "avatar": changed_avatar })).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["bot"]["avatar"], changed_avatar);
    assert_eq!(
        saved.body["bot"]["name"], "Work",
        "an avatar-only save keeps the name"
    );
    let events = updated_events(&app, &id).await;
    assert_eq!(events.len(), 1, "one bot.updated for the change");
    assert_eq!(events[0]["bot"]["avatar"], changed_avatar);

    let again = update(&app, &id, json!({ "avatar": changed_avatar })).await;
    assert_eq!(again.body, saved.body, "a retry returns the same bot");
    assert_eq!(
        updated_events(&app, &id).await.len(),
        1,
        "and journals nothing"
    );

    let renamed = update(&app, &id, json!({ "name": "Planner" })).await;
    assert_eq!(
        renamed.body["bot"]["avatar"], changed_avatar,
        "a name-only save keeps the avatar"
    );

    let teal_gem = json!({ "shape": "gem", "color": "teal" });
    let both = update(&app, &id, json!({ "name": "Scout", "avatar": teal_gem })).await;
    assert_eq!(both.status, StatusCode::OK);
    assert_eq!(both.body["bot"]["name"], "Scout");
    assert_eq!(both.body["bot"]["avatar"], teal_gem);
    let events = updated_events(&app, &id).await;
    assert_eq!(events.len(), 3, "each change journals once");
    assert_eq!(events[2]["bot"]["name"], "Scout");
    assert_eq!(events[2]["bot"]["avatar"], teal_gem);

    let app = app.restart().await;
    let listing = app.owner("GET", "/v1/bots", None).await;
    assert_eq!(
        listed(&listing, &id),
        both.body["bot"],
        "listed as saved after a restart"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bots_saved_before_avatars_get_a_stable_one() {
    let app = TestApp::installed().await;
    let id = app.create_bot("Old").await;
    sqlx::query("UPDATE bots SET avatar_shape = NULL, avatar_color = NULL WHERE id = ?1")
        .bind(id.parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
    let first = app.owner("GET", "/v1/bots", None).await;
    let second = app.owner("GET", "/v1/bots", None).await;
    let derived = listed(&first, &id)["avatar"].clone();
    assert!(derived["shape"].is_string());
    assert_eq!(
        listed(&second, &id)["avatar"],
        derived,
        "the same avatar on every read"
    );

    let same = update(&app, &id, json!({ "avatar": derived })).await;
    assert_eq!(same.status, StatusCode::OK);
    assert!(
        updated_events(&app, &id).await.is_empty(),
        "saving the avatar it already shows journals nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn avatar_updates_validate_and_refuse_deleted_bots() {
    let app = TestApp::installed().await;
    let id = app.create_bot("Work").await;
    let before = app.owner("GET", &format!("/v1/bots/{id}"), None).await.body["bot"].clone();
    for body in [
        json!({ "avatar": { "shape": "dragon", "color": "blue" } }),
        json!({ "avatar": { "shape": "star", "color": "#3b82f6" } }),
        json!({ "avatar": { "shape": "star" } }),
        json!({ "avatar": { "shape": "star", "color": "blue", "image_url": "x" } }),
        json!({ "avatar": "star" }),
        json!({ "avatar": null }),
        json!({ "name": " ", "avatar": { "shape": "star", "color": "blue" } }),
    ] {
        let reply = update(&app, &id, body.clone()).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
    }
    let after = app.owner("GET", &format!("/v1/bots/{id}"), None).await.body["bot"].clone();
    assert_eq!(after, before, "rejected input changes nothing");
    assert!(updated_events(&app, &id).await.is_empty());

    let avatar = json!({ "avatar": { "shape": "egg", "color": "green" } });
    let unknown = update(&app, &Uuid::new_v4().to_string(), avatar.clone()).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let unauthorized = app
        .call(
            "PATCH",
            &format!("/v1/bots/{id}"),
            Some(avatar.clone()),
            None,
        )
        .await;
    assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED);

    app.owner("DELETE", &format!("/v1/bots/{id}"), None).await;
    let gone = update(&app, &id, avatar).await;
    assert_eq!(gone.status, StatusCode::GONE);
    assert_eq!(gone.body["error"]["code"], "bot_deleted");
    let receipt = app.owner("GET", &format!("/v1/bots/{id}"), None).await;
    assert_eq!(
        receipt.body["bot"]["avatar"], before["avatar"],
        "never revived or changed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn last_message_at_follows_messages_not_metadata_edits() {
    let app = TestApp::installed().await;
    let id = app.create_bot("Quiet").await;
    let read = async |app: &TestApp, id: String| {
        app.owner("GET", &format!("/v1/bots/{id}"), None).await.body["bot"].clone()
    };
    let fresh = read(&app, id.clone()).await;
    assert!(fresh["last_message_at"].is_null(), "never messaged");
    assert!(fresh["created_at"].is_string());

    update(&app, &id, json!({ "name": "Renamed" })).await;
    assert!(
        read(&app, id.clone()).await["last_message_at"].is_null(),
        "a rename is not a message"
    );

    let reply = app
        .send(&id, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    let turn = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["completed"]).await;
    let history = app
        .owner("GET", &format!("/v1/bots/{id}/conversation"), None)
        .await;
    let newest = history.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|item| item["kind"] == "assistant")
        .unwrap()["created_at"]
        .as_str()
        .unwrap()
        .parse::<chrono::DateTime<chrono::Utc>>()
        .unwrap();
    let messaged = read(&app, id.clone()).await;
    let at = |bot: &serde_json::Value| {
        bot["last_message_at"]
            .as_str()
            .unwrap()
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap()
    };
    assert_eq!(
        at(&messaged),
        newest,
        "the bot's reply is the newest message"
    );

    update(
        &app,
        &id,
        json!({ "avatar": { "shape": "egg", "color": "green" } }),
    )
    .await;
    assert_eq!(
        at(&read(&app, id.clone()).await),
        newest,
        "an avatar change keeps the recency"
    );
    let listed = app.owner("GET", "/v1/bots", None).await.body["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == id.as_str())
        .cloned()
        .unwrap();
    assert_eq!(at(&listed), newest, "the list carries it too");
    let main = app.main_bot().await;
    assert!(
        read(&app, main).await["last_message_at"].is_null(),
        "other bots are unaffected"
    );
}
