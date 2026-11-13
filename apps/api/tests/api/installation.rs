use crate::common;

use common::TestDb;
use else_api::db::{self, installation};
use installation::{InstallError, MainBot};
use uuid::Uuid;

async fn counts(pool: &sqlx::SqlitePool) -> (i64, i64, i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM bots), (SELECT count(*) FROM conversations),
                (SELECT count(*) FROM installation), (SELECT count(*) FROM provider_profiles)",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn fresh_setup_creates_main_bot_and_default_profile_once() {
    let db = TestDb::migrated().await;
    assert!(matches!(
        installation::check_installed(&db.pool).await,
        Err(InstallError::NotInstalled)
    ));
    assert_eq!(counts(&db.pool).await, (0, 0, 0, 0));

    let report = installation::setup(&db.pool).await.unwrap();
    let MainBot::Created(main_id) = report.main_bot else {
        panic!("expected main bot creation, got {:?}", report.main_bot);
    };
    assert_eq!(report.seeded_profiles, vec!["codex"]);
    assert_eq!(
        installation::check_installed(&db.pool).await.unwrap(),
        Some(main_id)
    );

    let profile: (String, String, String, i64) = sqlx::query_as(
        "SELECT model, reasoning_effort, service_tier, revision
         FROM provider_profiles WHERE provider_id = 'codex'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        profile,
        ("gpt-6.1-sol".into(), "medium".into(), "priority".into(), 1)
    );
    let conversation_bot: Uuid = sqlx::query_scalar("SELECT bot_id FROM conversations")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(conversation_bot, main_id);
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_migrate_setup_and_startup_are_stable() {
    let db = TestDb::migrated().await;
    let first = installation::setup(&db.pool).await.unwrap();
    let MainBot::Created(main_id) = first.main_bot else {
        panic!()
    };
    let snapshot = |pool: sqlx::SqlitePool| async move {
        let ids: Vec<(Uuid, Uuid)> =
            sqlx::query_as("SELECT b.id, c.id FROM bots b JOIN conversations c ON c.bot_id = b.id")
                .fetch_all(&pool)
                .await
                .unwrap();
        let settings: Vec<(String, String, String, String, i64)> = sqlx::query_as(
            "SELECT provider_id, model, reasoning_effort, service_tier, revision FROM provider_profiles",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        (ids, settings, counts(&pool).await)
    };
    let before = snapshot(db.pool.clone()).await;
    for _ in 0..2 {
        db::migrate(&db.pool).await.unwrap();
        let again = installation::setup(&db.pool).await.unwrap();
        assert_eq!(again.main_bot, MainBot::AlreadyInstalled(Some(main_id)));
        assert!(again.seeded_profiles.is_empty());
        assert_eq!(
            installation::check_installed(&db.pool).await.unwrap(),
            Some(main_id)
        );
    }
    assert_eq!(snapshot(db.pool.clone()).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn saved_owner_profile_changes_survive_setup() {
    let db = TestDb::migrated().await;
    installation::setup(&db.pool).await.unwrap();
    sqlx::query(
        "UPDATE provider_profiles SET reasoning_effort = 'high', revision = revision + 1
         WHERE provider_id = 'codex'",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    installation::setup(&db.pool).await.unwrap();
    db::migrate(&db.pool).await.unwrap();
    let saved: (String, i64) = sqlx::query_as(
        "SELECT reasoning_effort, revision FROM provider_profiles WHERE provider_id = 'codex'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(saved, ("high".into(), 2));
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_profile_is_seeded_without_recreating_bots() {
    let db = TestDb::migrated().await;
    installation::setup(&db.pool).await.unwrap();
    sqlx::query("DELETE FROM provider_profiles")
        .execute(&db.pool)
        .await
        .unwrap();
    let report = installation::setup(&db.pool).await.unwrap();
    assert_eq!(report.seeded_profiles, vec!["codex"]);
    assert_eq!(counts(&db.pool).await, (1, 1, 1, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn cleared_default_is_preserved_by_setup_and_startup() {
    let db = TestDb::migrated().await;
    installation::setup(&db.pool).await.unwrap();
    sqlx::query("UPDATE bots SET status = 'deleted', deleted_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE installation SET default_bot_id = NULL")
        .execute(&db.pool)
        .await
        .unwrap();
    for _ in 0..2 {
        let report = installation::setup(&db.pool).await.unwrap();
        assert_eq!(report.main_bot, MainBot::AlreadyInstalled(None));
        assert_eq!(installation::check_installed(&db.pool).await.unwrap(), None);
    }
    assert_eq!(counts(&db.pool).await, (1, 1, 1, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn partial_or_inconsistent_installations_are_refused() {
    let db = TestDb::migrated().await;
    installation::setup(&db.pool).await.unwrap();
    sqlx::query("DELETE FROM installation")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(
        installation::setup(&db.pool).await,
        Err(InstallError::MissingInstallationRecord)
    ));
    assert!(matches!(
        installation::check_installed(&db.pool).await,
        Err(InstallError::MissingInstallationRecord)
    ));
    assert_eq!(counts(&db.pool).await, (1, 1, 0, 1), "nothing was created");

    let db = TestDb::migrated().await;
    installation::setup(&db.pool).await.unwrap();
    sqlx::query("UPDATE bots SET status = 'deleted', deleted_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(
        installation::check_installed(&db.pool).await,
        Err(InstallError::InactiveDefault(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_setups_create_one_main_bot() {
    let db = TestDb::migrated().await;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(4));
    let runs: Vec<_> = Vec::from_fn(4, |_| {
        let pool = db.pool.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            installation::setup(&pool).await.unwrap()
        })
    });
    let created = futures_count(runs.into_iter()).await;
    assert_eq!(created, 1);
    assert_eq!(counts(&db.pool).await, (1, 1, 1, 1));
}

async fn futures_count(
    handles: impl Iterator<Item = tokio::task::JoinHandle<installation::SetupReport>>,
) -> usize {
    let mut created = 0;
    for handle in handles {
        if matches!(handle.await.unwrap().main_bot, MainBot::Created(_)) {
            created += 1;
        }
    }
    created
}

#[tokio::test(flavor = "multi_thread")]
async fn constraints_and_rollback_hold_in_sqlite() {
    let db = TestDb::migrated().await;
    installation::setup(&db.pool).await.unwrap();

    assert!(
        sqlx::query("INSERT INTO installation (default_bot_id) VALUES (NULL)")
            .execute(&db.pool)
            .await
            .is_err()
    );
    for (name, status, deleted) in [
        ("", "active", false),
        (" padded ", "active", false),
        (&*"n".repeat(81), "active", false),
        ("ok", "gone", false),
        ("ok", "deleted", false),
        ("ok", "active", true),
    ] {
        let result = sqlx::query(
            "INSERT INTO bots (id, name, status, deleted_at)
             VALUES (?1, ?2, ?3, CASE WHEN ?4 THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') END)",
        )
        .bind(Uuid::new_v4())
        .bind(name)
        .bind(status)
        .bind(deleted)
        .execute(&db.pool)
        .await;
        assert!(result.is_err(), "accepted {name:?} {status} {deleted}");
    }

    let before = counts(&db.pool).await;
    let existing_bot: Uuid = sqlx::query_scalar("SELECT id FROM bots")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let name = else_api::app::bots::BotName::parse("Work").unwrap();
    let avatar = else_api::app::bots::Avatar::DEFAULT;
    installation::insert_bot(&mut tx, Uuid::new_v4(), &name, avatar)
        .await
        .unwrap();
    let err = sqlx::query("INSERT INTO conversations (id, bot_id) VALUES (?1, ?2)")
        .bind(Uuid::new_v4())
        .bind(existing_bot)
        .execute(&mut *tx)
        .await;
    assert!(err.is_err());
    drop(tx);
    assert_eq!(counts(&db.pool).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_database_is_a_clear_error() {
    let url = else_api::config::DatabaseUrl::new(
        "postgresql://else:secret-canary@127.0.0.1:1/else".to_owned(),
    );
    let err = db::connect(&url).await.unwrap_err().to_string();
    assert!(err.contains("SQLite"), "{err}");
    assert!(!err.contains("secret-canary"));
    let err = db::connect(&else_api::config::DatabaseUrl::new("not a url".into()))
        .await
        .unwrap_err();
    assert!(matches!(err, db::DbError::InvalidUrl));
}
