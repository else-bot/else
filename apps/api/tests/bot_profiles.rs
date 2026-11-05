mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::json;
use uuid::Uuid;

fn choice(effort: &str, tier: &str) -> serde_json::Value {
    json!({ "provider_id": "codex", "model": "gpt-6.1-sol", "reasoning_effort": effort, "service_tier": tier })
}

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
    let reply = app.send(bot, Uuid::new_v4(), "hello", None).await;
    assert!(reply.status.is_success(), "{}", reply.body);
    reply.body["turn"]["id"].as_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn bots_use_their_own_model_or_the_defaults() {
    let app = TestApp::installed().await;

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

    let own = app
        .owner(
            "POST",
            "/v1/bots",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "Deep",
                         "profile": choice("high", "priority") })),
        )
        .await;
    assert_eq!(own.status, StatusCode::CREATED, "{}", own.body);
    let own = own.body["bot"]["id"].as_str().unwrap().to_owned();
    let plain = app.create_bot("Plain").await;
    let refused = app
        .owner(
            "POST",
            "/v1/bots",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "Bad",
                         "profile": choice("ultra", "default") })),
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

    let turn = send(&app, &own).await;
    assert_eq!(turn_profile(&app, &turn).await["reasoning_effort"], "high");
    assert_eq!(turn_profile(&app, &turn).await["service_tier"], "priority");
    let turn = send(&app, &plain).await;
    assert_eq!(turn_profile(&app, &turn).await["reasoning_effort"], "low");

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

#[tokio::test(flavor = "multi_thread")]
async fn creation_is_atomic_and_retries_never_overwrite_later_choices() {
    let app = TestApp::installed().await;
    let category = app
        .owner(
            "POST",
            "/v1/categories",
            Some(json!({
                "request_id": Uuid::new_v4(), "name": "Work"
            })),
        )
        .await
        .body["category"]["id"]
        .clone();
    let request_id = Uuid::new_v4();
    let payload = json!({
        "request_id": request_id, "name": "Created",
        "profile": choice("high", "priority"), "category_id": category,
        "avatar": { "shape": "cloud", "color": "pink" }
    });
    sqlx::query(
        "CREATE TRIGGER reject_profile BEFORE INSERT ON bot_profiles
                 BEGIN SELECT RAISE(ABORT, 'test profile failure'); END",
    )
    .execute(&app.db.pool)
    .await
    .unwrap();
    let failed = app.owner("POST", "/v1/bots", Some(payload.clone())).await;
    assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM bots WHERE name = 'Created'")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let receipts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM request_receipts WHERE request_id = ?1")
            .bind(request_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(receipts, 0);
    sqlx::query("DROP TRIGGER reject_profile")
        .execute(&app.db.pool)
        .await
        .unwrap();

    let created = app.owner("POST", "/v1/bots", Some(payload.clone())).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.body["bot"]["category_id"], category);
    assert_eq!(created.body["bot"]["avatar"], payload["avatar"]);
    let bot = created.body["bot"]["id"].as_str().unwrap();
    let path = format!("/v1/bots/{bot}/profile");
    assert_eq!(
        app.owner("GET", &path, None).await.body["profile"]["reasoning_effort"],
        "high"
    );
    assert_eq!(
        app.owner("PUT", &path, Some(choice("low", "default")))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.owner(
            "PATCH",
            &format!("/v1/bots/{bot}/category"),
            Some(json!({"category_id": null}))
        )
        .await
        .status,
        StatusCode::OK
    );

    let retry = app.owner("POST", "/v1/bots", Some(payload.clone())).await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["bot"]["id"], bot);
    assert!(retry.body["bot"]["category_id"].is_null());
    assert_eq!(
        app.owner("GET", &path, None).await.body["profile"]["reasoning_effort"],
        "low"
    );
    for (field, value) in [
        ("profile", choice("medium", "default")),
        ("category_id", json!(null)),
    ] {
        let mut changed = payload.clone();
        changed[field] = value;
        let conflict = app.owner("POST", "/v1/bots", Some(changed)).await;
        assert_eq!(conflict.status, StatusCode::CONFLICT);
        assert_eq!(conflict.body["error"]["code"], "request_conflict");
    }
    let mut missing = payload;
    missing["request_id"] = json!(Uuid::new_v4());
    missing["category_id"] = json!(Uuid::new_v4());
    let refused = app.owner("POST", "/v1/bots", Some(missing)).await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND);
    assert_eq!(refused.body["error"]["code"], "category_not_found");
}
