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
         WHERE c.bot_id = ?1 ORDER BY i.seq",
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
    assert!(first.body["turn"]["started_at"].is_null());
    assert_eq!(first.body["turn"]["provider_id"], "synthetic");

    let retry = app
        .send(&bot, request, "Hello there", Some("synthetic"))
        .await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["item"]["id"], first.body["item"]["id"]);
    assert_eq!(retry.body["turn"]["id"], first.body["turn"]["id"]);

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
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    let settled = snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn)
        .unwrap();
    let started: chrono::DateTime<chrono::Utc> =
        settled["started_at"].as_str().unwrap().parse().unwrap();
    let finished: chrono::DateTime<chrono::Utc> =
        settled["finished_at"].as_str().unwrap().parse().unwrap();
    assert!(finished >= started);
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
async fn each_agent_message_is_its_own_item() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let sent = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:messages]",
            Some("synthetic"),
        )
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap();
    assert_eq!(app.wait_turn(turn, &["completed"]).await, "completed");
    let history = items(&app, &bot).await;
    let replies: Vec<&str> = history[1..]
        .iter()
        .map(|(_, _, text)| text.as_str())
        .collect();
    assert_eq!(
        replies,
        vec![
            "Synthetic reply ✓ from Assistant (turn 1): [synthetic:messages]",
            else_api::providers::synthetic::SECOND_MESSAGE,
        ],
        "one assistant item per agent message"
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
    let b = app.create_bot("Work").await;
    for round in [[(&a, "a1"), (&b, "b1")], [(&a, "a2"), (&b, "b2")]] {
        let mut turns = Vec::new();
        for (bot, text) in round {
            let reply = app.send(bot, Uuid::new_v4(), text, Some("synthetic")).await;
            assert_eq!(reply.status, StatusCode::ACCEPTED);
            turns.push(reply.body["turn"]["id"].as_str().unwrap().to_owned());
        }
        for turn in &turns {
            app.wait_turn(turn, &["completed"]).await;
        }
    }
    let last = app.send(&a, Uuid::new_v4(), "a3", Some("synthetic")).await;
    app.wait_turn(last.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let texts = |history: Vec<(i64, String, String)>| -> Vec<String> {
        history
            .into_iter()
            .map(|(_, kind, text)| format!("{kind}:{text}"))
            .collect()
    };
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
async fn one_active_turn_per_bot_while_newer_messages_steer() {
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
        assert_eq!(reply.status, StatusCode::ACCEPTED);
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
    assert_eq!(app.wait_turn(&turns[2], &["completed"]).await, "completed");
    for turn in &turns[..2] {
        assert_eq!(app.wait_turn(turn, &["interrupted"]).await, "interrupted");
    }
    assert_eq!(
        monitor.await.unwrap(),
        1,
        "never more than one active turn for the bot"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rapid_corrections_are_all_accepted_and_reach_the_newest_reply() {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let synthetic = Arc::new(else_api::providers::synthetic::Synthetic::new(
        Duration::from_millis(5),
    ));
    let registry = else_api::providers::Registry::new(vec![synthetic.clone()]);
    let app = TestApp::with_registry(db, 4, registry);
    let bot = app.main_bot().await;
    let other = app.create_bot("Other").await;
    let elsewhere = app
        .send(
            &other,
            Uuid::new_v4(),
            "[synthetic:hang] elsewhere",
            Some("synthetic"),
        )
        .await;
    let elsewhere = elsewhere.body["turn"]["id"].as_str().unwrap().to_owned();
    let hang = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:hang] publish",
            Some("synthetic"),
        )
        .await;
    let hang = hang.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&hang, &["running"]).await;
    app.wait_turn(&elsewhere, &["running"]).await;

    let corrections: Vec<String> = (0..6).map(|i| format!("correction {i}")).collect();
    let mut requests = Vec::new();
    for text in &corrections {
        let request = Uuid::new_v4();
        let reply = app.send(&bot, request, text, Some("synthetic")).await;
        assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.body);
        requests.push((
            request,
            reply.body["turn"]["id"].as_str().unwrap().to_owned(),
        ));
    }
    let listing = app.owner("GET", &format!("/v1/bots/{bot}"), None).await;
    assert!(
        listing.body["bot"]["activity"]["queued_count"]
            .as_i64()
            .unwrap()
            <= 1,
        "{}",
        listing.body
    );

    let (_, newest) = requests.last().unwrap();
    assert_eq!(app.wait_turn(newest, &["completed"]).await, "completed");
    assert_eq!(app.wait_turn(&hang, &["interrupted"]).await, "interrupted");
    for (request, turn) in &requests[..requests.len() - 1] {
        assert_eq!(app.wait_turn(turn, &["interrupted"]).await, "interrupted");
        let lookup = app
            .owner(
                "GET",
                &format!("/v1/bots/{bot}/turns/by-request/{request}"),
                None,
            )
            .await;
        assert_eq!(lookup.body["turn"]["error"]["code"], "superseded");
    }
    let packet = synthetic.packets().await.last().unwrap().clone();
    for text in &corrections {
        assert!(
            packet.contains(text.as_str()),
            "{text} missing from {packet}"
        );
    }
    let owner_messages = items(&app, &bot)
        .await
        .into_iter()
        .filter(|(_, kind, _)| kind == "user")
        .count();
    assert_eq!(owner_messages, 7, "every message is kept in history");
    let other_status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(elsewhere.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(other_status, "running", "another bot's work is untouched");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_of_an_older_message_does_not_preempt_the_newer_one() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let first_request = Uuid::new_v4();
    let first = app
        .send(
            &bot,
            first_request,
            "[synthetic:hang] first",
            Some("synthetic"),
        )
        .await;
    let first = first.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&first, &["running"]).await;
    let second = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:hang] second",
            Some("synthetic"),
        )
        .await;
    let second = second.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&second, &["running"]).await;
    assert_eq!(app.wait_turn(&first, &["interrupted"]).await, "interrupted");

    let retry = app
        .send(
            &bot,
            first_request,
            "[synthetic:hang] first",
            Some("synthetic"),
        )
        .await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["turn"]["id"], first.as_str());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(second.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "running");
}

#[tokio::test(flavor = "multi_thread")]
async fn different_bots_run_concurrently_within_the_global_limit() {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let app = TestApp::with_options(db, 2);
    let bots = [
        app.main_bot().await,
        app.create_bot("B").await,
        app.create_bot("C").await,
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
    let failed_id = failed.body["turn"]["id"].as_str().unwrap();
    assert_eq!(app.wait_turn(failed_id, &["failed"]).await, "failed");
    let refused = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:refuse]",
            Some("synthetic"),
        )
        .await;
    let refused_id = refused.body["turn"]["id"].as_str().unwrap();
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
async fn failed_acceptance_publishes_nothing() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    sqlx::raw_sql(
        "CREATE TRIGGER fail_item BEFORE INSERT ON items WHEN NEW.text = 'trigger-failure' BEGIN SELECT RAISE(ABORT, 'injected'); END;",
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
         FROM conversations c JOIN bots b ON b.id = c.bot_id WHERE b.id = ?1",
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

    let deleted = app.create_bot("Gone").await;
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
    assert_eq!(caps.body["default_provider_id"], "synthetic");
    let ids: Vec<_> = caps.body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, vec!["synthetic", "codex"]);
    assert_eq!(caps.body["providers"][0]["label"], "Synthetic (test)");
    assert_eq!(caps.body["limits"]["max_text_bytes"], 32768);
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
    sqlx::query("UPDATE turns SET status = 'running' WHERE id = ?1")
        .bind(hang_id.parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
    let pool = app.db.pool.clone();
    let db = app.db.clone();
    drop(app);
    let id = hang_id.parse::<Uuid>().unwrap();
    for _ in 0..200 {
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
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
        "UPDATE turns SET status = 'running', finished_at = NULL, error_code = NULL WHERE id = ?1",
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
    sqlx::raw_sql("CREATE TRIGGER fail_assistant_once BEFORE INSERT ON items WHEN NEW.kind = 'assistant' BEGIN SELECT RAISE(ABORT, 'transient'); END;")
        .execute(&app.db.pool).await.unwrap();
    let pool = app.db.pool.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        sqlx::query("DROP TRIGGER fail_assistant_once")
            .execute(&pool)
            .await
            .unwrap();
    });
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

async fn react(
    app: &TestApp,
    bot: &str,
    request: Uuid,
    item: &serde_json::Value,
    kind: &str,
) -> common::Reply {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/turns"),
        Some(json!({
            "request_id": request,
            "text": "👍 Feedback on your earlier message",
            "provider_id": "synthetic",
            "reaction": { "item_id": item, "kind": kind },
        })),
    )
    .await
}

async fn exchange(app: &TestApp, bot: &str) -> (serde_json::Value, serde_json::Value) {
    let sent = app
        .send(bot, Uuid::new_v4(), "Hello there", Some("synthetic"))
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap();
    assert_eq!(app.wait_turn(turn, &["completed"]).await, "completed");
    let assistant: Uuid = sqlx::query_scalar(
        "SELECT id FROM items WHERE turn_id = ?1 AND kind = 'assistant' ORDER BY seq",
    )
    .bind(turn.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    (sent.body["item"]["id"].clone(), json!(assistant))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_is_saved_with_its_feedback_message() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let (_, assistant) = exchange(&app, &bot).await;
    let request = Uuid::new_v4();
    let sent = react(&app, &bot, request, &assistant, "thumbs_up").await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{:?}", sent.body);
    let expected = json!({ "item_id": assistant, "kind": "thumbs_up" });
    assert_eq!(sent.body["item"]["reaction"], expected);
    assert_eq!(sent.body["turn"]["reaction"], expected);
    let feedback = sent.body["item"]["id"].clone();
    let turn = sent.body["turn"]["id"].as_str().unwrap();
    assert_eq!(app.wait_turn(turn, &["completed"]).await, "completed");

    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    let items = snapshot.body["items"].as_array().unwrap();
    let saved = items.iter().find(|i| i["id"] == feedback).unwrap();
    assert_eq!(saved["reaction"], expected);
    assert_eq!(saved["text"], "👍 Feedback on your earlier message");
    let page = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation?limit=1"), None)
        .await;
    assert_eq!(page.body["items"].as_array().unwrap().len(), 1);
    assert_eq!(page.body["items"][0]["kind"], "assistant");
    assert_eq!(page.body["turns"][0]["reaction"], expected);
    assert!(
        items
            .iter()
            .filter(|i| i["id"] != feedback)
            .all(|i| i["reaction"].is_null()),
        "only the feedback message carries a reaction"
    );

    let upsert: String = sqlx::query_scalar(
        "SELECT data FROM bot_events WHERE kind = 'item.upsert' AND json_extract(data, '$.item.id') = ?1",
    )
    .bind(feedback.as_str().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    let upsert: serde_json::Value = serde_json::from_str(&upsert).unwrap();
    assert_eq!(upsert["item"]["reaction"], expected);

    let lookup = app
        .owner(
            "GET",
            &format!("/v1/bots/{bot}/turns/by-request/{request}"),
            None,
        )
        .await;
    assert_eq!(lookup.body["item"]["reaction"], expected);
    assert_eq!(lookup.body["turn"]["reaction"], expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_retry_must_repeat_the_same_reaction() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let (_, assistant) = exchange(&app, &bot).await;
    let request = Uuid::new_v4();
    let first = react(&app, &bot, request, &assistant, "thumbs_down").await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{:?}", first.body);

    let retry = react(&app, &bot, request, &assistant, "thumbs_down").await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["item"]["id"], first.body["item"]["id"]);
    assert_eq!(retry.body["item"]["reaction"]["kind"], "thumbs_down");

    let changed = react(&app, &bot, request, &assistant, "thumbs_up").await;
    assert_eq!(changed.status, StatusCode::CONFLICT);
    assert_eq!(changed.body["error"]["code"], "request_conflict");
    let without = app
        .send(
            &bot,
            request,
            "👍 Feedback on your earlier message",
            Some("synthetic"),
        )
        .await;
    assert_eq!(without.status, StatusCode::CONFLICT);
    assert_eq!(without.body["error"]["code"], "request_conflict");
    assert_eq!(turn_count(&app).await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_must_target_an_assistant_message_of_the_same_bot() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let other = app.create_bot("Other").await;
    let (owner_item, _) = exchange(&app, &bot).await;
    let (_, others_reply) = exchange(&app, &other).await;
    let before = turn_count(&app).await;

    for target in [owner_item, others_reply, json!(Uuid::new_v4())] {
        let refused = react(&app, &bot, Uuid::new_v4(), &target, "thumbs_up").await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{target}");
        assert_eq!(refused.body["error"]["code"], "invalid_reaction");
    }
    let unknown_kind = react(&app, &bot, Uuid::new_v4(), &json!(Uuid::new_v4()), "heart").await;
    assert_eq!(unknown_kind.status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown_kind.body["error"]["code"], "invalid_body");
    assert_eq!(turn_count(&app).await, before, "nothing was accepted");
}

#[tokio::test]
async fn a_turn_still_finishing_its_last_attempt_is_not_claimed_and_holds_its_bots_queue() {
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let mut queued = Vec::new();
    for (bot, name) in [(Uuid::new_v4(), "First"), (Uuid::new_v4(), "Second")] {
        sqlx::query("INSERT INTO bots (id, name) VALUES (?1, ?2)")
            .bind(bot)
            .bind(name)
            .execute(&db.pool)
            .await
            .unwrap();
        let conversation = Uuid::new_v4();
        sqlx::query("INSERT INTO conversations (id, bot_id) VALUES (?1, ?2)")
            .bind(conversation)
            .bind(bot)
            .execute(&db.pool)
            .await
            .unwrap();
        for turn_no in [1, 2] {
            let turn = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
                                    owner_item_id, provider_id, status, created_at)
                 VALUES (?1, ?2, ?3, ?1, zeroblob(32), ?4, ?5, 'synthetic', 'queued', ?6)",
            )
            .bind(turn)
            .bind(bot)
            .bind(conversation)
            .bind(turn_no)
            .bind(Uuid::new_v4())
            .bind(format!("2026-01-01T00:00:0{}.000+00:00", queued.len()))
            .execute(&db.pool)
            .await
            .unwrap();
            queued.push(turn);
        }
    }
    let busy = [queued[0]];
    assert_eq!(
        else_api::db::turns::claim_next(&db.pool, &busy)
            .await
            .unwrap(),
        Some(queued[2]),
        "another bot's turn starts"
    );
    assert_eq!(
        else_api::db::turns::claim_next(&db.pool, &busy)
            .await
            .unwrap(),
        None,
        "the finishing turn and the later turn of its bot both wait"
    );
    sqlx::query("UPDATE turns SET status = 'completed' WHERE id = ?1")
        .bind(queued[2])
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        else_api::db::turns::claim_next(&db.pool, &[])
            .await
            .unwrap(),
        Some(queued[0]),
        "once its task is gone the turn starts first in its bot's queue"
    );
}
