use crate::common;

use else_api::db::memory::{self, Kind, NewEntry};
use std::os::unix::fs::PermissionsExt;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn files_are_private_durable_and_every_connection_enforces_foreign_keys() {
    let db = common::TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let path = db.pool.connect_options().get_filename().to_owned();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let mut connections = Vec::new();
    for _ in 0..8 {
        let mut conn = db.pool.acquire().await.unwrap();
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        assert!(
            sqlx::query("INSERT INTO conversations (id, bot_id) VALUES (?1, ?2)")
                .bind(Uuid::new_v4())
                .bind(Uuid::new_v4())
                .execute(&mut *conn)
                .await
                .is_err()
        );
        connections.push(conn);
    }
    drop(connections);
    db.pool.close().await;
    let reopened = else_api::db::connect(&db.url).await.unwrap();
    assert!(
        else_api::db::installation::check_installed(&reopened)
            .await
            .unwrap()
            .is_some()
    );
    else_api::db::migrate(&reopened).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
            .fetch_one(&reopened)
            .await
            .unwrap(),
        "ok"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lexical_search_accepts_phrases_alternatives_exclusions_and_punctuation() {
    let app = common::TestApp::installed().await;
    let bot: Uuid = app.main_bot().await.parse().unwrap();
    for (key, body) in [
        ("a", "red fox at the café"),
        ("b", "red slow fox"),
        ("c", "blue bird"),
    ] {
        let mut tx = app.db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        memory::append(
            &mut tx,
            bot,
            &NewEntry {
                request_key: key,
                kind: Kind::AssistantMessage,
                body,
                source: serde_json::json!({}),
                supersedes_seq: None,
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    for (query, count) in [
        ("\"red fox\"", 1),
        ("fox OR bird", 3),
        ("red -slow", 1),
        ("café", 1),
        ("", 0),
        ("\"", 0),
        ("{}():*", 0),
        ("fox\" OR (", 2),
    ] {
        let result = memory::search(&app.db.pool, bot, query, false, None, 0)
            .await
            .unwrap();
        assert_eq!(result.hits.len(), count, "{query}");
    }
}
