use crate::common;
use else_api::db::turns::TurnStatus;

use std::time::Duration;

use axum::http::StatusCode;
use common::recorder::{self, GATE, GATE_AFTER_OPENING};
use common::{ApiProcess, TestApp, TestDb};
use reqwest::Method;
use serde_json::json;
use uuid::Uuid;

async fn stop(app: &TestApp, bot: &str, request: Uuid) -> common::Reply {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/cancel"),
        Some(json!({ "request_id": request })),
    )
    .await
}

async fn status_of(pool: &sqlx::SqlitePool, turn: &str) -> (String, Option<String>, bool) {
    sqlx::query_as("SELECT status, error_code, effect_uncertain FROM turns WHERE id = ?1")
        .bind(turn.parse::<Uuid>().unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn wait_receipt(app: &TestApp, bot: &str, request: Uuid) -> common::Reply {
    common::until(
        "cancellation to complete",
        common::poll(|| async {
            let reply = stop(app, bot, request).await;
            (reply.body["state"] == "done").then_some(reply)
        }),
    )
    .await
}

fn turn_id(reply: &common::Reply) -> String {
    reply.body["turn"]["id"].as_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_work_cancels_through_its_boundary_only() {
    let h = recorder::harness(TestApp::builder()).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    h.recorder.hold_hangs(true);
    let running = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    app.wait_turn(&turn_id(&running), &["running"]).await;
    let queued_a = app
        .send(&bot, Uuid::new_v4(), "queued a", Some("synthetic"))
        .await;
    let queued_b = app
        .send(&bot, Uuid::new_v4(), "queued b", Some("synthetic"))
        .await;

    let request = Uuid::new_v4();
    let first = stop(app, &bot, request).await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{:?}", first.body);
    assert_eq!(first.body["state"], "pending");
    let rejected = app
        .send(&bot, Uuid::new_v4(), "too early", Some("synthetic"))
        .await;
    assert_eq!(rejected.status, StatusCode::CONFLICT);
    assert_eq!(rejected.body["error"]["code"], "cancellation_pending");
    let bot_view = app.owner("GET", &format!("/v1/bots/{bot}"), None).await;
    assert_eq!(bot_view.body["bot"]["activity"]["phase"], "cancelling");
    h.recorder.hold_hangs(false);
    let done = wait_receipt(app, &bot, request).await;
    assert_eq!(done.status, StatusCode::OK);
    for turn in [&running, &queued_a, &queued_b] {
        let (status, _, uncertain) = status_of(&app.db.pool, &turn_id(turn)).await;
        assert_eq!(status, "interrupted");
        assert!(!uncertain);
    }
    let bot_view = app.owner("GET", &format!("/v1/bots/{bot}"), None).await;
    assert_eq!(
        bot_view.body["bot"]["sidebar_status"],
        json!({ "kind": "stopped" })
    );
    let user_items: i64 = sqlx::query_scalar("SELECT count(*) FROM items WHERE kind = 'user'")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        user_items, 3,
        "history is retained; the rejected send added nothing"
    );

    let (after, _) = h.running(&bot, &format!("{GATE} after stop")).await;
    let retry = stop(app, &bot, request).await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["state"], "done");
    h.release(after).await;
    assert_eq!(
        app.wait_turn(after, &["completed", "interrupted"]).await,
        "completed"
    );
    let other = app.create_bot("Other").await;
    assert_eq!(
        stop(app, &other, request).await.status,
        StatusCode::CONFLICT
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_with_no_work_completes_at_once_and_keeps_partial_text() {
    let h = recorder::harness(TestApp::builder()).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let idle = stop(app, &bot, Uuid::new_v4()).await;
    assert_eq!(
        (idle.status, idle.body["state"].clone()),
        (StatusCode::OK, json!("done"))
    );

    let (turn, _) = h
        .running(&bot, &format!("{GATE_AFTER_OPENING} partial"))
        .await;
    common::steer::wait_for_text(app, turn, 0).await;
    let request = Uuid::new_v4();
    stop(app, &bot, request).await;
    wait_receipt(app, &bot, request).await;
    assert_eq!(
        status_of(&app.db.pool, &turn.to_string()).await.0,
        "interrupted"
    );
    let text: String = sqlx::query_scalar("SELECT text FROM items WHERE kind = 'assistant'")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert!(
        !text.is_empty() && !text.ends_with("partial"),
        "partial output kept: {text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_that_ignores_interruption_ends_uncertain() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let stuck = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:ignore-cancel]",
            Some("synthetic"),
        )
        .await;
    app.wait_turn(&turn_id(&stuck), &["running"]).await;
    let request = Uuid::new_v4();
    assert_eq!(stop(&app, &bot, request).await.status, StatusCode::ACCEPTED);
    common::never_polled(
        "the retried stop to stop reporting pending during the interruption grace period",
        Duration::from_millis(300),
        || async {
            let reply = stop(&app, &bot, request).await;
            (reply.body["state"] != "pending").then_some(reply.body)
        },
    )
    .await;
    wait_receipt(&app, &bot, request).await;
    let (status, code, uncertain) = status_of(&app.db.pool, &turn_id(&stuck)).await;
    assert_eq!(
        (status.as_str(), code.as_deref(), uncertain),
        ("uncertain", Some("delivery_uncertain"), true)
    );
    let bot_view = app.owner("GET", &format!("/v1/bots/{bot}"), None).await;
    assert_eq!(
        bot_view.body["bot"]["sidebar_status"],
        json!({ "kind": "uncertain" })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deletion_during_work_blocks_late_output_and_never_reactivates() {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Busy").await;
    let stuck = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:ignore-cancel]",
            Some("synthetic"),
        )
        .await;
    app.wait_turn(&turn_id(&stuck), &["running"]).await;
    let queued = app
        .send(&bot, Uuid::new_v4(), "never runs", Some("synthetic"))
        .await;

    let deleted = app.owner("DELETE", &format!("/v1/bots/{bot}"), None).await;
    assert_eq!(deleted.status, StatusCode::ACCEPTED);
    assert_eq!(deleted.body["bot"]["cleanup"]["state"], "pending");
    let listing = app
        .owner("GET", "/v1/bots?include_deleting=true", None)
        .await;
    assert!(
        listing.body["bots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["id"] == bot.as_str())
    );
    let listing = app.owner("GET", "/v1/bots", None).await;
    assert!(
        listing.body["bots"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["id"] != bot.as_str())
    );
    assert_eq!(
        app.send(&bot, Uuid::new_v4(), "hi", Some("synthetic"))
            .await
            .status,
        StatusCode::GONE
    );
    assert_eq!(
        stop(&app, &bot, Uuid::new_v4()).await.status,
        StatusCode::GONE
    );

    common::until(
        "the deleted bot's cleanup to finish",
        common::poll(|| async {
            let receipt = app.owner("GET", &format!("/v1/bots/{bot}"), None).await;
            (receipt.body["bot"]["cleanup"]["state"] == "done").then_some(())
        }),
    )
    .await;
    assert_eq!(
        status_of(&app.db.pool, &turn_id(&queued)).await.0,
        "interrupted"
    );
    assert_eq!(
        status_of(&app.db.pool, &turn_id(&stuck)).await.0,
        "uncertain"
    );
    let assistant: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM items i JOIN conversations c ON c.id = i.conversation_id
         WHERE c.bot_id = ?1 AND i.kind = 'assistant'",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(assistant, 0, "no output appended after deletion");

    let app = app.restart().await;
    else_api::db::installation::setup(&app.db.pool)
        .await
        .unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
        .bind(bot.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "deleted");
}

#[tokio::test(flavor = "multi_thread")]
async fn late_output_after_deletion_is_fenced() {
    let h = recorder::harness(TestApp::builder()).await;
    let app = &h.app;
    let bot = app.create_bot("Streaming").await;
    let (turn, _) = h.running(&bot, &format!("{GATE} a long reply")).await;
    app.owner("DELETE", &format!("/v1/bots/{bot}"), None).await;
    let count = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT coalesce(group_concat(text, ''), '') FROM items WHERE kind = 'assistant'",
        )
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
    };
    let at_delete = count().await;
    h.release(turn).await;
    app.wait_turn(turn, &["interrupted"]).await;
    assert_eq!(
        count().await,
        at_delete,
        "nothing appended after the tombstone"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
        .bind(bot.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "deleted");
}

async fn wait_status(pool: &sqlx::SqlitePool, turn: &str, want: &str) {
    let reached = common::poll(|| async { (status_of(pool, turn).await.0 == want).then_some(()) });
    if tokio::time::timeout(common::SETTLE, reached).await.is_err() {
        panic!(
            "turn {turn} never reached {want}: {:?}",
            status_of(pool, turn).await
        );
    }
}

async fn send(api: &ApiProcess, bot: &str, text: &str) -> String {
    let (status, body) = api
        .call(
            Method::POST,
            &format!("/v1/bots/{bot}/turns"),
            Some(json!({ "request_id": Uuid::new_v4(), "text": text, "provider_id": "synthetic" })),
        )
        .await;
    assert_eq!(status, 202, "{body}");
    body["turn"]["id"].as_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn killing_the_api_at_each_boundary_reconciles_without_replay() {
    let db = TestDb::migrated().await;
    let dir = tempfile::tempdir().unwrap();
    let api = ApiProcess::start(&db, dir.path()).await;
    let (_, bots) = api.call(Method::GET, "/v1/bots", None).await;
    let main = bots["default_bot_id"].as_str().unwrap().to_owned();
    let (_, created) = api
        .call(
            Method::POST,
            "/v1/bots",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "Second" })),
        )
        .await;
    let second = created["bot"]["id"].as_str().unwrap().to_owned();
    let (_, created) = api
        .call(
            Method::POST,
            "/v1/bots",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "Third" })),
        )
        .await;
    let third = created["bot"]["id"].as_str().unwrap().to_owned();

    let sending = send(&api, &main, "[synthetic:stall-send]").await;
    wait_status(&db.pool, &sending, "sending").await;
    let running = send(&api, &second, "[synthetic:hang]").await;
    wait_status(&db.pool, &running, "running").await;
    let queued = send(&api, &third, "queued during crash").await;
    common::never_polled(
        "the third turn to leave the queue before the API restarts",
        Duration::from_millis(200),
        || async {
            let status = status_of(&db.pool, &queued).await.0;
            (status != "queued").then_some(status)
        },
    )
    .await;
    let owner_items = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM items WHERE kind = 'user'")
            .fetch_one(&db.pool)
            .await
            .unwrap()
    };
    assert_eq!(owner_items().await, 3);
    api.kill();

    let ids_before: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, status FROM bots ORDER BY id")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    for _ in 0..2 {
        let api = ApiProcess::start(&db, dir.path()).await;
        wait_status(&db.pool, &queued, "completed").await;
        let (status, code, uncertain) = status_of(&db.pool, &sending).await;
        assert_eq!(
            (status.as_str(), code.as_deref(), uncertain),
            ("uncertain", Some("api_restarted"), true)
        );
        let (status, _, uncertain) = status_of(&db.pool, &running).await;
        assert_eq!((status.as_str(), uncertain), ("uncertain", true));
        let (_, bots) = api.call(Method::GET, "/v1/bots", None).await;
        assert_eq!(bots["default_bot_id"], main.as_str());
        assert_eq!(bots["bots"].as_array().unwrap().len(), 3);
        drop(api);
    }
    assert_eq!(
        owner_items().await,
        3,
        "no owner message replayed or duplicated"
    );
    let assistant_for_uncertain: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM items WHERE kind = 'assistant' AND lower(hex(turn_id)) IN (SELECT replace(value, '-', '') FROM json_each(?1))",
    )
    .bind(sqlx::types::Json(vec![
        sending.parse::<Uuid>().unwrap(),
        running.parse::<Uuid>().unwrap(),
    ]))
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(assistant_for_uncertain, 0, "uncertain work was not resent");
    let ids_after: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, status FROM bots ORDER BY id")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert_eq!(ids_after, ids_before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_api_cannot_take_over_runtime_storage_or_database() {
    let db = TestDb::migrated().await;
    let dir = tempfile::tempdir().unwrap();
    let api = ApiProcess::start(&db, dir.path()).await;
    let (_, bots) = api.call(Method::GET, "/v1/bots", None).await;
    let main = bots["default_bot_id"].as_str().unwrap().to_owned();
    let hanging = send(&api, &main, "[synthetic:hang]").await;
    wait_status(&db.pool, &hanging, "running").await;

    let output = common::api_command(&db, dir.path(), 0)
        .arg("serve")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already running"), "{stderr}");

    let other = tempfile::tempdir().unwrap();
    std::fs::copy(
        dir.path().join("owner-token"),
        other.path().join("owner-token"),
    )
    .unwrap();
    std::fs::copy(
        dir.path().join("secret-key"),
        other.path().join("secret-key"),
    )
    .unwrap();
    let output = common::api_command(&db, other.path(), 0)
        .arg("serve")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already using this database"), "{stderr}");

    assert_eq!(status_of(&db.pool, &hanging).await.0, "running");
    let (status, body) = api
        .call(
            Method::POST,
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(status == 202 || status == 200, "{body}");
    wait_status(&db.pool, &hanging, "interrupted").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_waiting_on_a_send_includes_that_send() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let bot_id: Uuid = bot.parse().unwrap();
    let mut tx = app.db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("SELECT 1 FROM bots WHERE id = ?1")
        .bind(bot_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    let request = Uuid::new_v4();
    let mut stop_task = {
        let router = app.router.clone();
        tokio::spawn(async move {
            use tower::ServiceExt;
            let request = axum::http::Request::post(format!("/v1/bots/{bot_id}/cancel"))
                .header("host", "127.0.0.1")
                .header("authorization", format!("Bearer {}", common::TOKEN))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({ "request_id": request }).to_string(),
                ))
                .unwrap();
            router.oneshot(request).await.unwrap().status()
        })
    };
    common::never(
        "the stop to answer while the send holds the write lock",
        Duration::from_millis(300),
        &mut stop_task,
    )
    .await;
    let turn_id = common::seed::turn(bot_id).insert(&mut *tx).await;
    tx.commit().await.unwrap();
    assert_eq!(
        common::until("the stop to finish after the send commits", stop_task)
            .await
            .unwrap(),
        StatusCode::OK
    );
    let (status, code, _) = status_of(&app.db.pool, &turn_id.to_string()).await;
    assert_eq!(
        (status.as_str(), code.as_deref()),
        ("interrupted", Some("cancelled"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lost_interrupts_are_redelivered_and_orphans_settle() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let hang = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    app.wait_turn(&turn_id(&hang), &["running"]).await;
    // A stop whose interrupt never reached the running provider.
    common::seed::force_status(
        &app.db.pool,
        turn_id(&hang).parse::<Uuid>().unwrap(),
        TurnStatus::Cancelling,
    )
    .await;
    assert_eq!(
        app.wait_turn(&turn_id(&hang), &["interrupted"]).await,
        "interrupted"
    );

    let other = app.create_bot("Orphan").await;
    let queued = app
        .send(
            &other,
            Uuid::new_v4(),
            "[synthetic:hang]",
            Some("synthetic"),
        )
        .await;
    app.wait_turn(&turn_id(&queued), &["running"]).await;
    app.scheduler
        .interrupt(turn_id(&queued).parse().unwrap())
        .await;
    app.wait_turn(&turn_id(&queued), &["interrupted"]).await;
    // A running row with no live provider task, which orphan settlement ends.
    sqlx::query("UPDATE turns SET status = 'running', finished_at = NULL WHERE id = ?1")
        .bind(turn_id(&queued).parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        app.wait_turn(&turn_id(&queued), &["uncertain"]).await,
        "uncertain"
    );
    let (_, code, uncertain) = status_of(&app.db.pool, &turn_id(&queued)).await;
    assert_eq!((code.as_deref(), uncertain), (Some("orphaned"), true));
    let next = app
        .send(&other, Uuid::new_v4(), "after orphan", Some("synthetic"))
        .await;
    assert_eq!(
        app.wait_turn(&turn_id(&next), &["completed"]).await,
        "completed"
    );
}
