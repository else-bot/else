//! Migrations that carry existing routines forward.

use super::*;

#[tokio::test]
async fn the_routines_migration_preserves_existing_data() {
    let db = TestDb::empty().await;
    let dir = tempfile::tempdir().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for name in [
        "0001_application.sql",
        "0002_secret_requests.sql",
        "0003_remove_bot_roles.sql",
    ] {
        std::fs::copy(source.join(name), dir.path().join(name)).unwrap();
    }
    sqlx::migrate::Migrator::new(dir.path())
        .await
        .unwrap()
        .run(&db.pool)
        .await
        .unwrap();
    let main = Uuid::new_v4();
    for sql in [
        "INSERT INTO bots (id, name) VALUES (?1, 'Assistant')",
        "INSERT INTO conversations (id, bot_id) VALUES (randomblob(16), ?1)",
        "INSERT INTO installation (default_bot_id) VALUES (?1)",
    ] {
        sqlx::query(sql).bind(main).execute(&db.pool).await.unwrap();
    }
    let root = Uuid::new_v4();
    let owner_item = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, status)
         SELECT ?1, ?2, c.id, ?1, ?3, 1, ?4, 'synthetic', 'waiting'
         FROM conversations c WHERE c.bot_id = ?2 AND c.kind = 'owner'",
    )
    .bind(root)
    .bind(main)
    .bind(vec![1u8; 32])
    .bind(owner_item)
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
         SELECT ?1, conversation_id, 1, id, 'user', 'plan the week' FROM turns WHERE id = ?2",
    )
    .bind(owner_item)
    .bind(root)
    .execute(&db.pool)
    .await
    .unwrap();
    let leaf = Uuid::new_v4();
    let task_conversation = Uuid::new_v4();
    let child = Uuid::new_v4();
    let child_item = Uuid::new_v4();
    let hash = vec![7u8; 32];
    sqlx::query("INSERT INTO bots (id, name) VALUES (?1, 'Leaf')")
        .bind(leaf)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations (id, bot_id, kind) VALUES (?1, ?2, 'task')")
        .bind(task_conversation)
        .bind(leaf)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, status)
         VALUES (?1, ?2, ?3, ?1, ?4, 1, ?5, 'synthetic', 'running')",
    )
    .bind(child)
    .bind(leaf)
    .bind(task_conversation)
    .bind(&hash)
    .bind(child_item)
    .execute(&db.pool)
    .await
    .unwrap();
    for (seq, kind, text) in [(1, "user", "task text"), (2, "assistant", "child result")] {
        sqlx::query(
            "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(if seq == 1 { child_item } else { Uuid::new_v4() })
        .bind(task_conversation)
        .bind(seq)
        .bind(child)
        .bind(kind)
        .bind(text)
        .execute(&db.pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO delegations (id, root_turn_id, parent_attempt, parent_bot_id, target_bot_id,
             task, task_hash, conversation_id, child_turn_id, status)
         VALUES (randomblob(16), ?1, 1, ?2, ?3, 'task text', ?4, ?5, ?6, 'running')",
    )
    .bind(root)
    .bind(main)
    .bind(leaf)
    .bind(&hash)
    .bind(task_conversation)
    .bind(child)
    .execute(&db.pool)
    .await
    .unwrap();
    let mut conn = db.pool.acquire().await.unwrap();
    let saved_items =
        "SELECT id, conversation_id, seq, turn_id, kind, text, provider_id, created_at FROM items";
    let before = common::rows(&mut conn, saved_items).await;
    drop(conn);

    else_api::db::migrate(&db.pool).await.unwrap();

    let mut conn = db.pool.acquire().await.unwrap();
    assert_eq!(common::rows(&mut conn, saved_items).await, before);
    drop(conn);
    let avatars: Vec<(Uuid, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT id, avatar_shape, avatar_color FROM bots ORDER BY id = ?1 DESC")
            .bind(main)
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        avatars,
        [
            (main, Some("star".into()), Some("blue".into())),
            (leaf, None, None)
        ],
        "the main bot becomes the blue star; others keep a derived avatar"
    );
    let index: Option<String> =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name = 'items_turn'")
            .fetch_optional(&db.pool)
            .await
            .unwrap();
    assert!(index.is_some());
    let violations: Vec<(String,)> =
        sqlx::query_as("SELECT \"table\" FROM pragma_foreign_key_check")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    // The task's turn ends; the delegation trigger settles its task.
    sqlx::query("UPDATE turns SET status = 'completed' WHERE id = ?1")
        .bind(child)
        .execute(&db.pool)
        .await
        .unwrap();
    let (status, result): (String, Option<String>) =
        sqlx::query_as("SELECT status, result FROM delegations WHERE child_turn_id = ?1")
            .bind(child)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        (status.as_str(), result.as_deref()),
        ("completed", Some("child result"))
    );
    let root_status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(root)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(root_status, "queued");
    sqlx::query("INSERT INTO items (id, conversation_id, seq, turn_id, kind, text) VALUES (randomblob(16), ?1, 3, ?2, 'routine', 'p')")
        .bind(task_conversation)
        .bind(child)
        .execute(&db.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_failures_migration_keeps_a_failed_last_run_visible_and_never_reviews_history() {
    let db = TestDb::empty().await;
    let dir = tempfile::tempdir().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let migrate = |dir: &std::path::Path| {
        let dir = dir.to_owned();
        let pool = db.pool.clone();
        async move {
            sqlx::migrate::Migrator::new(dir.as_path())
                .await
                .unwrap()
                .run(&pool)
                .await
                .unwrap();
        }
    };
    let mut names: Vec<_> = std::fs::read_dir(&source)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".sql") && name.as_str() < "0031")
        .collect();
    names.sort();
    for name in &names {
        std::fs::copy(source.join(name), dir.path().join(name)).unwrap();
    }
    migrate(dir.path()).await;
    let bot = Uuid::new_v4();
    for sql in [
        "INSERT INTO bots (id, name) VALUES (?1, 'Assistant')",
        "INSERT INTO conversations (id, bot_id) VALUES (randomblob(16), ?1)",
    ] {
        sqlx::query(sql).bind(bot).execute(&db.pool).await.unwrap();
    }
    for (turn_no, status, routine) in [
        (1, "completed", "Fine"),
        (2, "failed", "Broken"),
        (3, "queued", "Pending"),
    ] {
        let turn = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
                 owner_item_id, provider_id, status)
             SELECT ?1, ?2, c.id, ?1, ?3, ?4, randomblob(16), 'synthetic', ?5
             FROM conversations c WHERE c.bot_id = ?2",
        )
        .bind(turn)
        .bind(bot)
        .bind(vec![1u8; 32])
        .bind(turn_no)
        .bind(status)
        .execute(&db.pool)
        .await
        .unwrap();
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO routines (id, bot_id, conversation_id, name, prompt, cron, timezone,
                 created_turn_id, created_name, request_hash)
             SELECT ?1, ?2, c.id, ?3, 'do it', '0 * * * *', 'UTC', ?4, ?3, ?5
             FROM conversations c WHERE c.bot_id = ?2",
        )
        .bind(id)
        .bind(bot)
        .bind(routine)
        .bind(turn)
        .bind(vec![2u8; 32])
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO routine_runs (id, routine_id, due_at, name, turn_id)
             VALUES (randomblob(16), ?1, '2026-11-08T03:00:00.000+00:00', ?2, ?3)",
        )
        .bind(id)
        .bind(routine)
        .bind(turn)
        .execute(&db.pool)
        .await
        .unwrap();
    }
    std::fs::copy(
        source.join("0031_routine_failures.sql"),
        dir.path().join("0031_routine_failures.sql"),
    )
    .unwrap();
    migrate(dir.path()).await;
    let failed: Vec<(String, i64)> =
        sqlx::query_as("SELECT name, failed_runs FROM routines ORDER BY name")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        failed,
        [
            ("Broken".into(), 1),
            ("Fine".into(), 0),
            ("Pending".into(), 0)
        ]
    );
    let unreviewed: Vec<String> =
        sqlx::query_scalar("SELECT name FROM routine_runs WHERE NOT reviewed")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        unreviewed,
        ["Pending"],
        "an unfinished run is reviewed when it ends"
    );
}
