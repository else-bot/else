//! Provider profiles and the capabilities the API reports.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn profiles_validate_snapshot_and_survive_restart() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let profile = app.owner("GET", "/v1/settings/providers/codex", None).await;
    assert_eq!(profile.status, StatusCode::OK);
    assert_eq!(
        profile.body["profile"],
        json!({
            "provider_id": "codex", "model": "gpt-6.1-sol", "reasoning_effort": "medium",
            "service_tier": "priority", "revision": 1,
            "updated_at": profile.body["profile"]["updated_at"],
        })
    );

    let busy = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    app.wait_turn(busy.body["turn"]["id"].as_str().unwrap(), &["running"])
        .await;
    let queued = app
        .send(&bot, Uuid::new_v4(), "queued before update", Some("codex"))
        .await;
    assert_eq!(queued.body["turn"]["profile_revision"], 1);

    for body in [
        json!({ "model": "gpt-4" }),
        json!({ "service_tier": "turbo" }),
        json!({ "reasoning_effort": "" }),
    ] {
        let reply = app
            .owner("PATCH", "/v1/settings/providers/codex", Some(body))
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.body["error"]["code"], "invalid_profile");
    }
    let unauthenticated = app
        .call(
            "PATCH",
            "/v1/settings/providers/codex",
            Some(json!({ "reasoning_effort": "high" })),
            None,
        )
        .await;
    assert_eq!(unauthenticated.status, StatusCode::UNAUTHORIZED);
    let synthetic = app
        .owner(
            "PATCH",
            "/v1/settings/providers/synthetic",
            Some(json!({ "model": "x" })),
        )
        .await;
    assert_eq!(synthetic.status, StatusCode::NOT_FOUND);

    let updated = app
        .owner(
            "PATCH",
            "/v1/settings/providers/codex",
            Some(json!({ "reasoning_effort": "high", "expected_revision": 1 })),
        )
        .await;
    assert_eq!(updated.status, StatusCode::OK);
    assert_eq!(updated.body["profile"]["revision"], 2);
    let stale = app
        .owner(
            "PATCH",
            "/v1/settings/providers/codex",
            Some(json!({ "reasoning_effort": "low", "expected_revision": 1 })),
        )
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT);

    let after = app
        .send(&bot, Uuid::new_v4(), "accepted after update", Some("codex"))
        .await;
    assert_eq!(after.body["turn"]["profile_revision"], 2);
    let snapshots: Vec<(i64, String)> = sqlx::query_as(
        "SELECT profile_revision, profile->>'reasoning_effort' FROM turns
         WHERE provider_id = 'codex' ORDER BY turn_no",
    )
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(snapshots, vec![(1, "medium".into()), (2, "high".into())]);

    let app = app.restart().await;
    let profile = app.owner("GET", "/v1/settings/providers/codex", None).await;
    assert_eq!(profile.body["profile"]["reasoning_effort"], "high");
    assert_eq!(profile.body["profile"]["revision"], 2);
    else_api::db::installation::setup(&app.db.pool)
        .await
        .unwrap();
    let profile = app.owner("GET", "/v1/settings/providers/codex", None).await;
    assert_eq!(
        profile.body["profile"]["revision"], 2,
        "setup preserves the saved profile"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn capabilities_list_providers_and_limits() {
    let app = TestApp::installed().await;
    let caps = app.owner("GET", "/v1/capabilities", None).await;
    assert_eq!(caps.body["api_version"], "v1");
    assert_eq!(caps.body["answers_notice"], true);
    assert_eq!(caps.body["bot_avatar_on_create"], true);
    assert!(
        caps.body["server_instance_id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .is_ok()
    );
    assert_eq!(caps.body["server"], json!({ "name": null, "icon": null }));
    assert_eq!(caps.body["default_provider_id"], "synthetic");
    let ids: Vec<_> = caps.body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, vec!["synthetic", "codex"]);
    assert_eq!(
        caps.body["providers"][0],
        json!({
            "id": "synthetic", "label": "Synthetic (test)", "available": true,
            "unavailable_reason": null, "cli_available": true, "cli_unavailable_reason": null,
            "requires_profile": false, "profile_configured": false,
            "catalog": { "models": [], "featured_models": [], "efforts": [], "tiers": [], "details": [], "listed_at": null, "refresh_error": null },
        })
    );
    assert_eq!(
        caps.body["limits"],
        json!({ "max_text_bytes": 32768, "max_bot_name_chars": 80 })
    );
    assert_eq!(caps.body["defaults"]["provider_id"], "codex");
    assert_eq!(caps.body["defaults"]["model"], "gpt-6.1-sol");
    assert_eq!(caps.body["providers"][1]["cli_available"], true);
    assert!(caps.body["providers"][1]["catalog"].is_object());
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_partial_profile_updates_both_apply() {
    let app = Arc::new(TestApp::installed().await);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let calls: Vec<_> = [
        json!({ "reasoning_effort": "high" }),
        json!({ "service_tier": "default" }),
    ]
    .into_iter()
    .map(|body| {
        let (app, barrier) = (app.clone(), barrier.clone());
        tokio::spawn(async move {
            barrier.wait().await;
            app.owner("PATCH", "/v1/settings/providers/codex", Some(body))
                .await
                .status
        })
    })
    .collect();
    for call in calls {
        assert_eq!(call.await.unwrap(), StatusCode::OK);
    }
    let profile = app.owner("GET", "/v1/settings/providers/codex", None).await;
    assert_eq!(profile.body["profile"]["reasoning_effort"], "high");
    assert_eq!(profile.body["profile"]["service_tier"], "default");
    assert_eq!(profile.body["profile"]["revision"], 3);
}
