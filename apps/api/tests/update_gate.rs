mod common;

use axum::http::StatusCode;
use common::TestApp;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn an_update_is_refused_while_bots_work_and_holds_new_work_once_idle() {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Worker").await;

    let health = app.call("GET", "/healthz", None, None).await;
    assert_eq!(health.body["version"], else_api::VERSION);

    let sent = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["running"]).await;
    let busy = app.owner("GET", "/v1/maintenance/update", None).await;
    assert_eq!(busy.status, StatusCode::OK, "{}", busy.body);
    assert_eq!(busy.body["active_turns"], 1);
    assert_eq!(busy.body["running_commands"], 0);
    let refused = app.owner("POST", "/v1/maintenance/update", None).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], "busy");
    let renamed = app
        .owner(
            "POST",
            "/v1/categories",
            Some(serde_json::json!({ "request_id": Uuid::new_v4(), "name": "Still open" })),
        )
        .await;
    assert_eq!(renamed.status, StatusCode::CREATED, "the gate reopened");
    app.wait_turn(&turn, &["running"]).await;

    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/cancel"),
        Some(serde_json::json!({ "request_id": Uuid::new_v4() })),
    )
    .await;
    app.wait_turn(&turn, &["interrupted"]).await;
    let ready = app.owner("POST", "/v1/maintenance/update", None).await;
    assert_eq!(ready.status, StatusCode::OK, "{}", ready.body);
    assert_eq!(ready.body["ready"], true);
    assert_eq!(ready.body["version"], else_api::VERSION);
    assert_eq!(ready.body["background_jobs"], 0);
    let again = app.owner("POST", "/v1/maintenance/update", None).await;
    assert_eq!(again.status, StatusCode::OK);

    let held_report = app.owner("GET", "/v1/maintenance/update", None).await;
    assert_eq!(held_report.status, StatusCode::OK, "{}", held_report.body);
    assert_eq!(held_report.body["active_turns"], 0);

    let held = app
        .send(&bot, Uuid::new_v4(), "after the update", Some("synthetic"))
        .await;
    assert_eq!(held.status, StatusCode::ACCEPTED, "{}", held.body);
    assert_eq!(
        held.body["turn"]["status"], "queued",
        "a message waits for the update instead of failing"
    );
    let rename = app
        .owner(
            "PATCH",
            &format!("/v1/bots/{bot}"),
            Some(serde_json::json!({ "name": "Renamed" })),
        )
        .await;
    assert_eq!(
        rename.body["error"]["code"], "maintenance",
        "{}",
        rename.body
    );
    assert_eq!(
        app.owner("GET", "/v1/bots", None).await.status,
        StatusCode::OK
    );

    let reopened = app.owner("DELETE", "/v1/maintenance/update", None).await;
    assert_eq!(reopened.body["reopened"], true);
    let accepted = app
        .send(&bot, Uuid::new_v4(), "hello again", Some("synthetic"))
        .await;
    assert!(accepted.status.is_success(), "{}", accepted.body);

    for path in [
        "/v1/maintenance/update",
        "/v1/maintenance/update/installation",
    ] {
        for method in ["GET", "POST"] {
            let anonymous = app.call(method, path, None, None).await;
            assert_eq!(
                anonymous.status,
                StatusCode::UNAUTHORIZED,
                "{method} {path}"
            );
        }
    }
}

#[tokio::test]
async fn servers_outside_the_installer_report_why_remote_updates_are_unavailable() {
    let app = TestApp::installed().await;
    let report = app
        .owner("GET", "/v1/maintenance/update/installation", None)
        .await;
    assert_eq!(report.status, StatusCode::OK);
    assert!(
        report.body["unavailable"]
            .as_str()
            .unwrap()
            .contains("deployment")
    );
    let refused = app
        .owner(
            "POST",
            "/v1/maintenance/update/installation",
            Some(serde_json::json!({"request_id": Uuid::new_v4(), "version": "999.0.0"})),
        )
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.body["error"]["code"], "update_unavailable");
}
