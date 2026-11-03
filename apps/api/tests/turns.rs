//! POC-005: durable messages, scheduling, the synthetic adapter, and saved
//! provider profiles, through the real routes and PostgreSQL.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::{TestApp, TestDb};
use serde_json::json;
use uuid::Uuid;

async fn items(app: &TestApp, bot: &str) -> Vec<(i64, String, String)> {
    sqlx::query_as(
        "SELECT i.seq, i.kind, i.text FROM items i JOIN conversations c ON c.id = i.conversation_id
         WHERE c.bot_id = $1 ORDER BY i.seq",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_all(&app.db.pool)
    .await
    .unwrap()
}

async fn turn_count(app: &TestApp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM turns")
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn send_retry_and_conflict_create_one_message() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let request = Uuid::new_v4();
    let first = app
        .send(&bot, request, "Hello there", Some("synthetic"))
        .await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{:?}", first.body);
    assert_eq!(first.body["item"]["kind"], "user");
    assert_eq!(first.body["item"]["text"], "Hello there");
    assert_eq!(first.body["item"]["request_id"], request.to_string());
    assert_eq!(first.body["turn"]["status"], "queued");
    assert_eq!(first.body["turn"]["provider_id"], "synthetic");

    // Double submission and a lost response yield the same message.
    let retry = app
        .send(&bot, request, "Hello there", Some("synthetic"))
        .await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["item"]["id"], first.body["item"]["id"]);
    assert_eq!(retry.body["turn"]["id"], first.body["turn"]["id"]);

    // Reuse with different text or provider fails without another message.
    for (text, provider) in [
        ("Changed", Some("synthetic")),
        ("Hello there", Some("codex")),
    ] {
        let conflict = app.send(&bot, request, text, provider).await;
        assert_eq!(conflict.status, StatusCode::CONFLICT);
        assert_eq!(conflict.body["error"]["code"], "request_conflict");
    }
    let lookup = app
        .owner(
            "GET",
            &format!("/v1/bots/{bot}/turns/by-request/{request}"),
            None,
        )
        .await;
    assert_eq!(lookup.status, StatusCode::OK);
    assert_eq!(lookup.body["turn"]["id"], first.body["turn"]["id"]);
    let missing = app
        .owner(
            "GET",
            &format!("/v1/bots/{bot}/turns/by-request/{}", Uuid::new_v4()),
            None,
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    let turn = first.body["turn"]["id"].as_str().unwrap();
    assert_eq!(app.wait_turn(turn, &["completed"]).await, "completed");
    let history = items(&app, &bot).await;
    assert_eq!(
        history.len(),
        2,
        "one owner bubble and one assistant bubble"
    );
    assert_eq!(history[0], (1, "user".into(), "Hello there".into()));
    assert_eq!(history[1].1, "assistant");
    assert_eq!(
        history[1].2,
        "Synthetic reply ✓ from Assistant (turn 1): Hello there"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_duplicate_sends_accept_once() {
    let app = Arc::new(TestApp::installed().await);
    let bot = app.main_bot().await;
    let request = Uuid::new_v4();
    let calls: Vec<_> = (0..6)
        .map(|_| {
            let (app, bot) = (app.clone(), bot.clone());
            tokio::spawn(async move {
                app.send(&bot, request, "once", Some("synthetic"))
                    .await
                    .status
            })
        })
        .collect();
    let mut statuses = Vec::new();
    for call in calls {
        statuses.push(call.await.unwrap());
    }
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::ACCEPTED)
            .count(),
        1,
        "{statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .all(|s| *s == StatusCode::ACCEPTED || *s == StatusCode::OK)
    );
    assert_eq!(turn_count(&app).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn bots_keep_separate_histories_and_order() {
    let app = TestApp::installed().await;
    let a = app.main_bot().await;
    let b = app.create_bot("Work", "Help with work").await;
    let mut turns = Vec::new();
    for (bot, text) in [(&a, "a1"), (&b, "b1"), (&a, "a2"), (&b, "b2"), (&a, "a3")] {
        let reply = app.send(bot, Uuid::new_v4(), text, Some("synthetic")).await;
        assert_eq!(reply.status, StatusCode::ACCEPTED);
        turns.push(reply.body["turn"]["id"].as_str().unwrap().to_owned());
    }
    for turn in &turns {
        app.wait_turn(turn, &["completed"]).await;
    }
    let texts = |history: Vec<(i64, String, String)>| -> Vec<String> {
        history
            .into_iter()
            .map(|(_, kind, text)| format!("{kind}:{text}"))
            .collect()
    };
    // History `seq` is arrival order: queued owner messages can precede
    // earlier replies. Each reply belongs to its own turn.
    let a_history = texts(items(&app, &a).await);
    assert_eq!(a_history.len(), 6);
    let a_users: Vec<_> = a_history
        .iter()
        .filter(|l| l.starts_with("user:"))
        .collect();
    assert_eq!(a_users, vec!["user:a1", "user:a2", "user:a3"]);
    for (n, text) in [(1, "a1"), (2, "a2"), (3, "a3")] {
        let reply = format!("assistant:Synthetic reply ✓ from Assistant (turn {n}): {text}");
        assert!(a_history.contains(&reply), "{a_history:?}");
    }
    let b_history = texts(items(&app, &b).await);
    assert_eq!(b_history.len(), 4);
    assert!(
        b_history
            .iter()
            .all(|line| !line.contains(": a") && !line.contains(":a"))
    );
    assert!(b_history.contains(&"assistant:Synthetic reply ✓ from Work (turn 2): b2".to_owned()));
}

#[tokio::test(flavor = "multi_thread")]
async fn one_active_turn_per_bot_and_queue_order() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let mut turns = Vec::new();
    for i in 0..3 {
        let reply = app
            .send(
                &bot,
                Uuid::new_v4(),
                &format!("[synthetic:slow] m{i}"),
                Some("synthetic"),
            )
            .await;
        turns.push(reply.body["turn"]["id"].as_str().unwrap().to_owned());
    }
    let pool = app.db.pool.clone();
    let monitor = tokio::spawn(async move {
        let mut max_active = 0i64;
        for _ in 0..200 {
            let active: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM turns WHERE status IN ('preparing','sending','running','cancelling')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            max_active = max_active.max(active);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        max_active
    });
    for turn in &turns {
        app.wait_turn(turn, &["completed"]).await;
    }
    assert_eq!(
        monitor.await.unwrap(),
        1,
        "never more than one active turn for the bot"
    );
    let finished: Vec<i64> = sqlx::query_scalar("SELECT turn_no FROM turns ORDER BY finished_at")
        .fetch_all(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(finished, vec![1, 2, 3], "queue order holds");
}

#[tokio::test(flavor = "multi_thread")]
async fn different_bots_run_concurrently_within_the_global_limit() {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let app = TestApp::with_options(db, 2);
    let bots = [
        app.main_bot().await,
        app.create_bot("B", "b").await,
        app.create_bot("C", "c").await,
    ];
    for bot in &bots {
        app.send(bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
            .await;
    }
    let mut counts = (0, 0);
    for _ in 0..120 {
        counts = sqlx::query_as::<_, (i64, i64)>(
            "SELECT count(*) FILTER (WHERE status = 'running'),
                    count(*) FILTER (WHERE status = 'queued') FROM turns",
        )
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
        if counts == (2, 1) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(counts, (2, 1));
    // The limit holds over time, not just at one sample.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM turns WHERE status = 'queued'")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(queued, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_are_terminal_outcomes_with_partial_text() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let failed = app
        .send(&bot, Uuid::new_v4(), "[synthetic:fail]", Some("synthetic"))
        .await;
    let refused = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:refuse]",
            Some("synthetic"),
        )
        .await;
    let failed_id = failed.body["turn"]["id"].as_str().unwrap();
    let refused_id = refused.body["turn"]["id"].as_str().unwrap();
    assert_eq!(app.wait_turn(failed_id, &["failed"]).await, "failed");
    assert_eq!(app.wait_turn(refused_id, &["failed"]).await, "failed");
    let lookup = app
        .owner(
            "GET",
            &format!(
                "/v1/bots/{bot}/turns/by-request/{}",
                failed.body["turn"]["request_id"].as_str().unwrap()
            ),
            None,
        )
        .await;
    assert_eq!(lookup.body["turn"]["error"]["code"], "provider_failed");
    let history = items(&app, &bot).await;
    let mut kinds: Vec<_> = history
        .iter()
        .map(|(_, kind, text)| (kind.as_str(), text.as_str()))
        .collect();
    kinds.sort();
    assert_eq!(
        kinds,
        vec![
            ("assistant", "Partial output before failure. "),
            ("user", "[synthetic:fail]"),
            ("user", "[synthetic:refuse]"),
        ],
        "partial text is kept; the refused turn produced no output"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_overflow_is_bounded_and_retryable() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let first = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    app.wait_turn(first.body["turn"]["id"].as_str().unwrap(), &["running"])
        .await;
    for i in 0..4 {
        let reply = app
            .send(
                &bot,
                Uuid::new_v4(),
                &format!("wait {i}"),
                Some("synthetic"),
            )
            .await;
        assert_eq!(reply.status, StatusCode::ACCEPTED);
    }
    let before = turn_count(&app).await;
    let overflow = app
        .send(&bot, Uuid::new_v4(), "one too many", Some("synthetic"))
        .await;
    assert_eq!(overflow.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(overflow.body["error"]["code"], "queue_full");
    assert_eq!(overflow.body["error"]["retryable"], true);
    assert!(overflow.headers.contains_key("retry-after"));
    assert_eq!(turn_count(&app).await, before);
    let listing = app.owner("GET", &format!("/v1/bots/{bot}"), None).await;
    assert_eq!(listing.body["bot"]["activity"]["phase"], "working");
    assert_eq!(listing.body["bot"]["activity"]["queued_count"], 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_acceptance_publishes_nothing() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    // Fail the owner-item insert after the turn row was written.
    sqlx::raw_sql(
        "CREATE FUNCTION fail_item() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF NEW.text = 'trigger-failure' THEN RAISE EXCEPTION 'injected'; END IF;
         RETURN NEW; END $$;
         CREATE TRIGGER fail_item BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION fail_item();",
    )
    .execute(&app.db.pool)
    .await
    .unwrap();
    let request = Uuid::new_v4();
    let reply = app
        .send(&bot, request, "trigger-failure", Some("synthetic"))
        .await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.body["error"]["code"], "internal");
    assert_eq!(turn_count(&app).await, 0);
    assert!(items(&app, &bot).await.is_empty());
    let (last_turn, last_seq, selected): (i64, i64, Option<String>) = sqlx::query_as(
        "SELECT c.last_turn_no, c.last_item_seq, b.selected_provider_id
         FROM conversations c JOIN bots b ON b.id = c.bot_id WHERE b.id = $1",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(
        (last_turn, last_seq, selected),
        (0, 0, None),
        "no counter or picker change"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_and_rejected_sends_create_nothing() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    for (text, provider, status) in [
        ("   ", Some("synthetic"), StatusCode::BAD_REQUEST),
        (
            &*"x".repeat(32 * 1024 + 1),
            Some("synthetic"),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        ("nul\u{0}", Some("synthetic"), StatusCode::BAD_REQUEST),
        ("hi", Some("nope"), StatusCode::BAD_REQUEST),
    ] {
        let reply = app.send(&bot, Uuid::new_v4(), text, provider).await;
        assert_eq!(reply.status, status);
    }
    let unknown = app
        .send(
            &Uuid::new_v4().to_string(),
            Uuid::new_v4(),
            "hi",
            Some("synthetic"),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);

    let deleted = app.create_bot("Gone", "temp").await;
    app.owner("DELETE", &format!("/v1/bots/{deleted}"), None)
        .await;
    let reply = app
        .send(&deleted, Uuid::new_v4(), "hello?", Some("synthetic"))
        .await;
    assert_eq!(reply.status, StatusCode::GONE);
    assert_eq!(reply.body["error"]["code"], "bot_deleted");
    assert_eq!(turn_count(&app).await, 0);
}

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

    // Hold the bot busy so the next message stays queued across the update.
    let busy = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    app.wait_turn(busy.body["turn"]["id"].as_str().unwrap(), &["running"])
        .await;
    let queued = app
        .send(&bot, Uuid::new_v4(), "queued before update", Some("codex"))
        .await;
    assert_eq!(queued.body["turn"]["profile_revision"], 1);

    // Unsupported values are rejected without changing the profile.
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
    assert_eq!(caps.body["default_provider_id"], "synthetic");
    let ids: Vec<_> = caps.body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, vec!["synthetic", "codex"]);
    assert_eq!(caps.body["providers"][0]["label"], "Synthetic (test)");
    assert_eq!(caps.body["limits"]["max_queued_turns"], 4);
    assert_eq!(caps.body["limits"]["max_text_bytes"], 32768);
    // The owner picks each bot's model: the defaults and each provider's
    // exact values are offered.
    assert_eq!(caps.body["defaults"]["provider_id"], "codex");
    assert_eq!(caps.body["defaults"]["model"], "gpt-6.1-sol");
    assert_eq!(caps.body["providers"][1]["cli_available"], true);
    assert!(caps.body["providers"][1]["catalog"].is_object());
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_requeues_undelivered_and_marks_delivered_uncertain() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let hang = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let hang_id = hang.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&hang_id, &["running"]).await;
    let waiting = app
        .send(&bot, Uuid::new_v4(), "after restart", Some("synthetic"))
        .await;
    let waiting_id = waiting.body["turn"]["id"].as_str().unwrap().to_owned();
    // Simulate a crash: the process vanishes without recording outcomes.
    sqlx::query("UPDATE turns SET status = 'running' WHERE id = $1")
        .bind(hang_id.parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
    let pool = app.db.pool.clone();
    let db = app.db.clone();
    // Abandon the old scheduler's in-memory state, then reconcile as startup does.
    drop(app);
    // Let the old scheduler finish draining (the hang takes 200 ms to stop)
    // before simulating that the crash left the row running.
    let id = hang_id.parse::<Uuid>().unwrap();
    for _ in 0..200 {
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        if status != "running" && status != "cancelling" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    sqlx::query(
        "UPDATE turns SET status = 'running', finished_at = NULL, error_code = NULL WHERE id = $1",
    )
    .bind(hang_id.parse::<Uuid>().unwrap())
    .execute(&pool)
    .await
    .unwrap();
    let (requeued, uncertain) = else_api::db::turns::recover_after_restart(&pool)
        .await
        .unwrap();
    assert_eq!((requeued, uncertain), (0, 1));
    let app = TestApp::with_options(db, 4);
    let status = app.wait_turn(&hang_id, &["uncertain"]).await;
    assert_eq!(status, "uncertain");
    app.wait_turn(&waiting_id, &["completed"]).await;
    let owner_messages: i64 = sqlx::query_scalar("SELECT count(*) FROM items WHERE kind = 'user'")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(owner_messages, 2, "nothing was replayed or duplicated");
}

#[tokio::test(flavor = "multi_thread")]
async fn transient_text_write_failure_keeps_the_text() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    // Fail the first assistant insert once, as a transient database error.
    sqlx::raw_sql(
        "CREATE SEQUENCE fail_once;
         CREATE FUNCTION fail_assistant_once() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           -- Sequences are not rolled back, so this fails exactly once.
           IF NEW.kind = 'assistant' AND nextval('fail_once') = 1 THEN
             RAISE EXCEPTION 'transient';
           END IF;
           RETURN NEW;
         END $$;
         CREATE TRIGGER fail_assistant_once BEFORE INSERT ON items
           FOR EACH ROW EXECUTE FUNCTION fail_assistant_once();",
    )
    .execute(&app.db.pool)
    .await
    .unwrap();
    let sent = app
        .send(&bot, Uuid::new_v4(), "keep me", Some("synthetic"))
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap();
    assert_eq!(
        app.wait_turn(turn, &["completed", "failed", "interrupted"])
            .await,
        "completed"
    );
    let history = items(&app, &bot).await;
    assert_eq!(history.len(), 2);
    assert_eq!(
        history[1].2,
        "Synthetic reply ✓ from Assistant (turn 1): keep me"
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn retry_succeeds_after_provider_removed_and_patch_needs_adapter() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let request = Uuid::new_v4();
    let first = app
        .send(&bot, request, "before removal", Some("synthetic"))
        .await;
    app.wait_turn(first.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let db = app.db.clone();
    drop(app);
    // Restart with no providers enabled, like the production default.
    let app = TestApp::with_registry(db, 4, else_api::providers::Registry::new(vec![]));
    let retry = app
        .send(&bot, request, "before removal", Some("synthetic"))
        .await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["turn"]["id"], first.body["turn"]["id"]);
    let fresh = app
        .send(&bot, Uuid::new_v4(), "new", Some("synthetic"))
        .await;
    assert_eq!(fresh.status, StatusCode::BAD_REQUEST);
    let none = app.send(&bot, Uuid::new_v4(), "new", None).await;
    assert_eq!(none.status, StatusCode::SERVICE_UNAVAILABLE);
    let patch = app
        .owner(
            "PATCH",
            "/v1/settings/providers/codex",
            Some(json!({ "reasoning_effort": "high" })),
        )
        .await;
    assert_eq!(patch.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(patch.body["error"]["code"], "provider_unavailable");
    let get = app.owner("GET", "/v1/settings/providers/codex", None).await;
    assert_eq!(get.body["profile"]["reasoning_effort"], "medium");
}

#[tokio::test(flavor = "multi_thread")]
async fn bounded_turn_actor_preserves_utf8_across_repeated_flushes() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let message = "é🙂終".repeat(900);
    let sent = app
        .send(&bot, Uuid::new_v4(), &message, Some("synthetic"))
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap();
    assert_eq!(
        app.wait_turn(turn, &["completed", "failed", "interrupted"])
            .await,
        "completed"
    );
    let history = items(&app, &bot).await;
    assert_eq!(history.len(), 2);
    assert_eq!(
        history[1].2,
        format!("Synthetic reply ✓ from Assistant (turn 1): {message}")
    );
}
