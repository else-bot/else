//! How a turn ends, what survives a restart, and how its status is stored.

use super::*;

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
    let db = common::until("the old app to stop", app.stop()).await;
    let pool = &db.pool;
    // Put back the running row a crash leaves, which a clean stop interrupts.
    sqlx::query(
        "UPDATE turns SET status = 'running', finished_at = NULL, error_code = NULL WHERE id = ?1",
    )
    .bind(hang_id.parse::<Uuid>().unwrap())
    .execute(pool)
    .await
    .unwrap();
    let (requeued, uncertain) = else_api::db::turns::lifecycle::recover_after_restart(pool)
        .await
        .unwrap();
    assert_eq!((requeued, uncertain), (0, 1));
    let app = TestApp::builder().db(db).start().await;
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

#[tokio::test]
async fn turn_status_matches_the_stored_vocabulary_and_sql_lists() {
    use else_api::db::turns::{ACTIVE_STATUSES, ENDED_STATUSES, UNFINISHED_STATUSES};
    let db = TestDb::migrated().await;
    let table: String =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'turns'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let allowed = &table[table.find("turns_status_check").unwrap()..];
    let allowed = &allowed[..allowed.find(')').unwrap()];
    let slot_index: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'turns_one_active_per_bot'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let mut stored = Vec::new();
    for status in TurnStatus::ALL {
        let text: String = sqlx::query_scalar("SELECT ?1")
            .bind(status)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let back: TurnStatus = sqlx::query_scalar("SELECT ?1")
            .bind(&text)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(back, status);
        let lists: (bool, bool, bool) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT ?1 IN {ACTIVE_STATUSES}, ?1 IN {ENDED_STATUSES}, ?1 IN {UNFINISHED_STATUSES}"
        )))
        .bind(status)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            lists,
            (status.is_active(), status.is_ended(), !status.is_ended()),
            "{text} disagrees with the SQL status lists"
        );
        assert_eq!(serde_json::to_value(status).unwrap(), json!(text));
        let quoted = format!("'{text}'");
        assert!(allowed.contains(&quoted), "{text} is not a stored status");
        assert_eq!(
            slot_index.contains(&quoted),
            status.is_active(),
            "{text} disagrees with the active-slot index"
        );
        stored.push(text);
    }
    assert_eq!(allowed.matches('\'').count(), 2 * stored.len());
    let ended: Vec<&str> = stored
        .iter()
        .zip(TurnStatus::ALL)
        .filter(|(_, status)| status.is_ended())
        .map(|(text, _)| text.as_str())
        .collect();
    assert_eq!(ended, ["completed", "interrupted", "failed", "uncertain"]);
    let unknown: Result<TurnStatus, _> = sqlx::query_scalar("SELECT 'paused'")
        .fetch_one(&db.pool)
        .await;
    assert!(unknown.is_err());
}
