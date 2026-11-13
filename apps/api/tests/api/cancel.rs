use crate::common;

use std::time::Duration;

use axum::http::StatusCode;
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
    for _ in 0..200 {
        let reply = stop(app, bot, request).await;
        if reply.body["state"] == "done" {
            return reply;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("cancellation did not complete");
}

fn turn_id(reply: &common::Reply) -> String {
    reply.body["turn"]["id"].as_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_work_cancels_through_its_boundary_only() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
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
    let first = stop(&app, &bot, request).await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{:?}", first.body);
    assert_eq!(first.body["state"], "pending");
    let rejected = app
        .send(&bot, Uuid::new_v4(), "too early", Some("synthetic"))
        .await;
    assert_eq!(rejected.status, StatusCode::CONFLICT);
    assert_eq!(rejected.body["error"]["code"], "cancellation_pending");
    let bot_view = app.owner("GET", &format!("/v1/bots/{bot}"), None).await;
    assert_eq!(bot_view.body["bot"]["activity"]["phase"], "cancelling");
    let done = wait_receipt(&app, &bot, request).await;
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

    let after = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:slow] after stop",
            Some("synthetic"),
        )
        .await;
    assert_eq!(after.status, StatusCode::ACCEPTED);
    app.wait_turn(&turn_id(&after), &["running"]).await;
    let retry = stop(&app, &bot, request).await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["state"], "done");
    assert_eq!(
        app.wait_turn(&turn_id(&after), &["completed", "interrupted"])
            .await,
        "completed"
    );
    let other = app.create_bot("Other").await;
    assert_eq!(
        stop(&app, &other, request).await.status,
        StatusCode::CONFLICT
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_with_no_work_completes_at_once_and_keeps_partial_text() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let idle = stop(&app, &bot, Uuid::new_v4()).await;
    assert_eq!(
        (idle.status, idle.body["state"].clone()),
        (StatusCode::OK, json!("done"))
    );

    let slow = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:slow] partial",
            Some("synthetic"),
        )
        .await;
    for _ in 0..200 {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM items WHERE kind = 'assistant'")
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
        if n > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let request = Uuid::new_v4();
    stop(&app, &bot, request).await;
    wait_receipt(&app, &bot, request).await;
    assert_eq!(
        status_of(&app.db.pool, &turn_id(&slow)).await.0,
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
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(stop(&app, &bot, request).await.body["state"], "pending");
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

    for _ in 0..200 {
        let receipt = app.owner("GET", &format!("/v1/bots/{bot}"), None).await;
        if receipt.body["bot"]["cleanup"]["state"] == "done" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let receipt = app.owner("GET", &format!("/v1/bots/{bot}"), None).await;
    assert_eq!(receipt.body["bot"]["cleanup"]["state"], "done");
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
    let app = TestApp::installed().await;
    let bot = app.create_bot("Streaming").await;
    let slow = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:slow] a long reply",
            Some("synthetic"),
        )
        .await;
    app.wait_turn(&turn_id(&slow), &["running"]).await;
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
    tokio::time::sleep(Duration::from_millis(800)).await;
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

async fn wait_status(api: &ApiProcess, pool: &sqlx::SqlitePool, turn: &str, want: &str) {
    let _ = api;
    for _ in 0..200 {
        if status_of(pool, turn).await.0 == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "turn {turn} never reached {want}: {:?}",
        status_of(pool, turn).await
    );
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
    wait_status(&api, &db.pool, &sending, "sending").await;
    let running = send(&api, &second, "[synthetic:hang]").await;
    wait_status(&api, &db.pool, &running, "running").await;
    let queued = send(&api, &third, "queued during crash").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(status_of(&db.pool, &queued).await.0, "queued");
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
        wait_status(&api, &db.pool, &queued, "completed").await;
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
    wait_status(&api, &db.pool, &hanging, "running").await;

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
    wait_status(&api, &db.pool, &hanging, "interrupted").await;
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
    let stop_task = {
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
    tokio::time::sleep(Duration::from_millis(300)).await;
    let turn_id = Uuid::new_v4();
    let conversation: Uuid = sqlx::query_scalar(
        "UPDATE conversations SET last_turn_no = last_turn_no + 1, last_item_seq = last_item_seq + 1
         WHERE bot_id = ?1 RETURNING id",
    )
    .bind(bot_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id)
         VALUES (?1, ?2, ?3, ?4, zeroblob(32), 1, ?5, 'synthetic')",
    )
    .bind(turn_id)
    .bind(bot_id)
    .bind(conversation)
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(stop_task.await.unwrap(), StatusCode::OK);
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
    sqlx::query("UPDATE turns SET status = 'cancelling' WHERE id = ?1")
        .bind(turn_id(&hang).parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
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
