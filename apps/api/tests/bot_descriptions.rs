mod common;

use std::borrow::Cow;

use else_api::db::{self, memory};
use sqlx::migrate::Migrator;
use uuid::Uuid;

const GITHUB: &str = "Maintain the owner's organization repositories.\nReport stale branches \
                      before deleting anything ✂";
const ARCHIVED: &str = "Book the family trips.";

async fn baseline() -> (common::TestDb, Uuid, Uuid, Uuid) {
    let db = common::TestDb::empty().await;
    let first = Migrator {
        migrations: Cow::Owned(db::MIGRATOR.migrations[..1].to_vec()),
        ..Migrator::DEFAULT
    };
    first.run(&db.pool).await.unwrap();
    let (main, github, gone) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    for (id, name, role, deleted) in [
        (main, "Assistant", "Main assistant.", false),
        (github, "GitHub", GITHUB, false),
        (gone, "Trips", ARCHIVED, true),
    ] {
        sqlx::query(
            "INSERT INTO bots (id, name, role, status, deleted_at)
             VALUES (?1, ?2, ?3, CASE WHEN ?4 THEN 'deleted' ELSE 'active' END,
                     CASE WHEN ?4 THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') END)",
        )
        .bind(id)
        .bind(name)
        .bind(role)
        .bind(deleted)
        .execute(&db.pool)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO installation (default_bot_id) VALUES (?1)")
        .bind(main)
        .execute(&db.pool)
        .await
        .unwrap();
    (db, main, github, gone)
}

async fn entries(db: &common::TestDb, bot: Uuid) -> Vec<(String, String, String)> {
    sqlx::query_as("SELECT kind, authority, body FROM memory_entry WHERE bot_id = ?1 ORDER BY seq")
        .bind(bot)
        .fetch_all(&db.pool)
        .await
        .unwrap()
}

async fn staged(db: &common::TestDb) -> Vec<(Uuid, String)> {
    sqlx::query_as("SELECT bot_id, description FROM legacy_bot_descriptions ORDER BY bot_id")
        .fetch_all(&db.pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn upgrading_moves_each_active_description_into_memory_once() {
    let (db, main, github, gone) = baseline().await;
    db::migrate(&db.pool).await.unwrap();
    db::migrate(&db.pool).await.unwrap();

    let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('bots')")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert!(!columns.iter().any(|c| c == "role"), "{columns:?}");

    let saved = entries(&db, github).await;
    assert_eq!(saved.len(), 1, "imported exactly once: {saved:?}");
    let (kind, authority, body) = &saved[0];
    assert_eq!(kind, "tool_activity");
    assert_eq!(authority, "tool", "never the owner's authority");
    assert!(body.ends_with(GITHUB), "the description is kept exactly");
    assert_eq!(entries(&db, main).await.len(), 1);

    let found = memory::search(&db.pool, github, "stale branches", false, None, 0)
        .await
        .unwrap();
    assert_eq!(found.hits.len(), 1);
    assert_eq!(found.hits[0].authority, "tool");

    assert!(entries(&db, gone).await.is_empty());
    assert_eq!(
        staged(&db).await,
        [(gone, ARCHIVED.to_owned())],
        "a deleted bot's description stays archived"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_import_interrupted_after_the_schema_change_finishes_on_the_next_run() {
    let (db, _, github, gone) = baseline().await;
    db::MIGRATOR.run(&db.pool).await.unwrap();
    assert_eq!(staged(&db).await.len(), 3);
    assert!(entries(&db, github).await.is_empty());

    db::migrate(&db.pool).await.unwrap();
    db::migrate(&db.pool).await.unwrap();
    assert_eq!(entries(&db, github).await.len(), 1);
    assert_eq!(staged(&db).await, [(gone, ARCHIVED.to_owned())]);
}
