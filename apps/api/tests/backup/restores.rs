//! Restoring a backup set into an installation.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_backup_restores_into_a_new_database_with_matching_rows() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
    let worker = worker(&app, key(1), config(dir.path()));
    let summary = worker.run_once().await.unwrap();
    let manifest = &summary.manifest;
    assert!(
        !manifest.coverage.whole_assistant,
        "a database-only set says so"
    );
    assert_eq!(manifest.coverage.vm, Covered::Omitted);
    assert!(
        manifest
            .bots
            .iter()
            .all(|b| b.vm == VmState::None && b.computer.is_none())
    );
    assert_eq!(manifest.bots.len(), 2, "main bot and Planner");
    assert!(manifest.application.migration_version >= 1);
    let mut names: Vec<String> = std::fs::read_dir(&summary.path)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [".else-backup", "complete.json", "index.json.age"],
        "the artifacts live in the shared chunk store"
    );
    use std::os::unix::fs::PermissionsExt;
    let chunk_dirs = std::fs::read_dir(summary.path.parent().unwrap().join("chunks"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(chunk_dirs.len(), 1, "one key's chunks");
    let chunk_files: Vec<PathBuf> = std::fs::read_dir(&chunk_dirs[0])
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert!(!chunk_files.is_empty());
    for path in names
        .iter()
        .map(|n| summary.path.join(n))
        .chain(chunk_dirs.iter().cloned())
        .chain(chunk_files.iter().cloned())
    {
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{} is private", path.display());
    }
    let (_whole, whole) = common::rebuilt_set(&summary.path, &key(1));
    let text = key_text(1);
    let raw = base64::engine::general_purpose::STANDARD
        .decode(&text)
        .unwrap();
    let (_, _, identity) = else_api::backup::restore::open_set(&whole, &key(1)).unwrap();
    let dump = std::fs::read(whole.join(backup::DATABASE)).unwrap();
    let plain = backup::open_small(&dump, &identity, 1 << 30).unwrap();
    assert!(
        plain.starts_with(b"SQLite format 3\0"),
        "a SQLite database image"
    );
    for bytes in names
        .iter()
        .map(|n| std::fs::read(summary.path.join(n)).unwrap())
        .chain(chunk_files.iter().map(|p| std::fs::read(p).unwrap()))
        .chain([plain.clone()])
    {
        assert!(!bytes.windows(text.len()).any(|w| w == text.as_bytes()));
        assert!(!bytes.windows(raw.len()).any(|w| w == raw.as_slice()));
    }
    let complete = std::fs::read_to_string(summary.path.join("complete.json")).unwrap();
    assert!(
        !complete.contains("Planner"),
        "the bootstrap reveals no content"
    );

    let staging = dir.path().join("staging");
    std::fs::create_dir(&staging).unwrap();
    std::fs::write(staging.join("keep.txt"), "untouched").unwrap();
    let target = Target::new(&app);
    let report = restore_into(&app, &summary.path, &key(1), &target, &staging)
        .await
        .unwrap();
    assert_eq!(report.set_id, summary.set_id);
    assert_eq!(
        report.migration_version,
        manifest.application.migration_version
    );
    for sql in COMPARED {
        let source = rows(app.db.url.expose(), sql).await;
        let restored = rows(&target.url(), sql).await;
        assert_eq!(source, restored, "{sql}");
        assert!(!source.is_empty(), "{sql}");
    }
    let profile = rows(
        &target.url(),
        "SELECT reasoning_effort, service_tier, revision FROM provider_profiles WHERE provider_id = 'codex'",
    )
    .await;
    assert_eq!(profile, [r#"["high","default",2]"#]);
    assert_eq!(
        std::fs::read_to_string(staging.join("keep.txt")).unwrap(),
        "untouched"
    );
    assert_eq!(entries(&staging), 1, "decrypted staging files are removed");
    let again = restore_into(&app, &summary.path, &key(1), &target, &staging).await;
    assert!(matches!(again, Err(BackupError::Refused(_))), "{again:?}");
    assert_eq!(
        rows(&target.url(), COMPARED[0]).await,
        rows(app.db.url.expose(), COMPARED[0]).await
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_keys_damage_and_existing_databases_never_produce_a_restore() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
    let staging = dir.path().join("staging");
    let summary = worker(&app, key(1), config(dir.path()))
        .run_once()
        .await
        .unwrap();

    let target = Target::new(&app);
    let wrong = restore_into(&app, &summary.path, &key(2), &target, &staging).await;
    assert!(
        matches!(wrong, Err(BackupError::WrongKey { .. })),
        "{wrong:?}"
    );
    assert!(!target.exists().await);

    let (_whole, whole) = common::rebuilt_set(&summary.path, &key(1));
    let damaged = |name: &str, change: &dyn Fn(&Path)| {
        let root = dir.path().join(name);
        std::fs::create_dir(&root).unwrap();
        let copy = root.join(&summary.set_id);
        std::fs::create_dir(&copy).unwrap();
        for file in ["complete.json", backup::DATABASE, backup::MANIFEST] {
            std::fs::copy(whole.join(file), copy.join(file)).unwrap();
        }
        change(&copy);
        copy
    };
    let truncated = damaged("truncated", &|set| {
        let path = set.join(backup::DATABASE);
        let len = std::fs::metadata(&path).unwrap().len();
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(len - 100).unwrap();
    });
    let corrupted = damaged("corrupted", &|set| {
        let path = set.join(backup::DATABASE);
        let mut bytes = std::fs::read(&path).unwrap();
        let at = bytes.len() / 2;
        bytes[at] ^= 0x40;
        std::fs::write(&path, bytes).unwrap();
    });
    let bad_manifest = damaged("manifest", &|set| {
        let path = set.join(backup::MANIFEST);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 5;
        bytes[last] ^= 1;
        std::fs::write(&path, bytes).unwrap();
    });
    let no_completion = damaged("unfinished", &|set| {
        std::fs::remove_file(set.join("complete.json")).unwrap();
    });
    let swapped_salt = damaged("salt", &|set| {
        let path = set.join("complete.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["encryption"]["salt_hex"] = json!("00".repeat(32));
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    });
    for (what, set) in [
        ("truncated", &truncated),
        ("corrupted", &corrupted),
        ("manifest", &bad_manifest),
        ("unfinished", &no_completion),
        ("salt", &swapped_salt),
    ] {
        let target = Target::new(&app);
        let result = restore_into(&app, set, &key(1), &target, &staging).await;
        assert!(result.is_err(), "{what} was restored");
        assert!(
            !matches!(result, Err(BackupError::WrongKey { .. })),
            "{what}: {result:?}"
        );
        assert!(!target.exists().await, "{what} created a database");
    }
    let chunks = summary.path.parent().unwrap().join("chunks");
    let key_dir = std::fs::read_dir(&chunks)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let chunk = std::fs::read_dir(&key_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let original = std::fs::read(&chunk).unwrap();
    let mut flipped = original.clone();
    let at = flipped.len() / 2;
    flipped[at] ^= 0x40;
    std::fs::write(&chunk, &flipped).unwrap();
    let target = Target::new(&app);
    let result = restore_into(&app, &summary.path, &key(1), &target, &staging).await;
    assert!(
        matches!(&result, Err(err) if err.to_string().contains("is damaged")),
        "a damaged chunk: {result:?}"
    );
    assert!(!target.exists().await, "a damaged chunk created a database");
    std::fs::write(&chunk, &original).unwrap();

    let existing = Target::new(&app);
    {
        let mut db =
            sqlx::SqliteConnection::connect_with(&else_api::db::options(&existing.url()).unwrap())
                .await
                .unwrap();
        for sql in [
            "CREATE TABLE precious (v text)",
            "INSERT INTO precious VALUES ('keep me')",
        ] {
            sqlx::query(sql).execute(&mut db).await.unwrap();
        }
    }
    let refused = restore_into(&app, &summary.path, &key(1), &existing, &staging).await;
    assert!(
        matches!(refused, Err(BackupError::Refused(_))),
        "{refused:?}"
    );
    assert_eq!(
        rows(&existing.url(), "SELECT v FROM precious").await,
        [r#"["keep me"]"#]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_ends_every_lease_claim_and_native_session() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let (bot, conversation): (Uuid, Uuid) =
        sqlx::query_as("SELECT bot_id, id FROM conversations ORDER BY created_at LIMIT 1")
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    for sql in [
        "INSERT INTO bot_computers (bot_id, runtime_name, image, cpus, memory_mib, observed_state)
         VALUES (?1, 'restore-leases', 'image', 1, 512, 'running')",
        "INSERT INTO computer_leases (id, bot_id, holder, generation, expires_at)
         VALUES (randomblob(16), ?1, 'test:held', 0, strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '+1 hours'))",
        "INSERT OR IGNORE INTO memory_store (bot_id) VALUES (?1)",
        "INSERT OR IGNORE INTO memory_generation (bot_id, generation, recipe) VALUES (?1, 1, '{}')",
        "INSERT INTO memory_job (bot_id, generation, start_seq, span, lease_token, lease_until)
         VALUES (?1, 1, 1024, 1, randomblob(16), strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '+1 hours'))",
        "INSERT INTO provider_sessions (id, bot_id, conversation_id, provider_id, native_session_id, fingerprint)
         VALUES (randomblob(16), ?1, ?2, 'synthetic', 'native', 'fingerprint')",
    ] {
        sqlx::query(sql)
            .bind(bot)
            .bind(conversation)
            .execute(&app.db.pool)
            .await
            .unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let summary = worker(&app, key(1), config(dir.path()))
        .run_once()
        .await
        .unwrap();
    let target = Target::new(&app);
    restore_into(&app, &summary.path, &key(1), &target, &dir.path().join("s"))
        .await
        .unwrap();
    for (sql, expected) in [
        (
            "SELECT holder || ':' || end_reason FROM computer_leases WHERE released_at IS NOT NULL",
            vec![r#"["test:held:restored"]"#],
        ),
        (
            "SELECT holder FROM computer_leases WHERE released_at IS NULL",
            vec![],
        ),
        (
            "SELECT start_seq FROM memory_job WHERE lease_token IS NOT NULL",
            vec![],
        ),
        (
            "SELECT start_seq FROM memory_job WHERE start_seq = 1024",
            vec!["[1024]"],
        ),
        (
            "SELECT status FROM provider_sessions WHERE native_session_id = 'native'",
            vec![r#"["retired"]"#],
        ),
        (
            "SELECT observed_state FROM bot_computers",
            vec![r#"["missing"]"#],
        ),
    ] {
        assert_eq!(rows(&target.url(), sql).await, expected, "{sql}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_stops_the_tasks_of_the_email_turns_it_ends() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    app.create_bot("Helper").await;
    let db = app.stop().await;
    let conversation = |name: &'static str| {
        sqlx::query_as::<_, (Uuid, Uuid)>(
            "SELECT c.bot_id, c.id FROM conversations c JOIN bots b ON b.id = c.bot_id
             WHERE b.name = ?1 AND c.kind = 'owner'",
        )
        .bind(name)
        .fetch_one(&db.pool)
    };
    let (parent, _) = conversation("Planner").await.unwrap();
    let (target, target_conversation) = conversation("Helper").await.unwrap();
    let (account, event, root, child) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    common::seed::turn(parent)
        .id(root)
        .turn_no(100)
        .status(TurnStatus::Waiting)
        .insert(&db.pool)
        .await;
    common::seed::turn(target)
        .id(child)
        .turn_no(100)
        .insert(&db.pool)
        .await;
    common::seed::delegation(root, parent, target, target_conversation, child)
        .task("Summarize the email")
        .insert(&db.pool)
        .await;
    for sql in [
        "INSERT INTO connector_accounts (id, address, config, state, removed_at)
         VALUES (?1, 'inbox@example.test', '{}', 'removed', strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))",
        "INSERT INTO connector_events (id, account_id, folder, uid_validity, uid, generation)
         VALUES (?2, ?1, 'INBOX', 1, 1, 1)",
        "INSERT INTO connector_deliveries (event_id, bot_id, account_id, binding_id, status, turn_id, settled_at)
         VALUES (?2, ?3, ?1, randomblob(16), 'delivered', ?4, strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))",
    ] {
        sqlx::query(sql)
            .bind(account)
            .bind(event)
            .bind(parent)
            .bind(root)
            .execute(&db.pool)
            .await
            .unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let summary = BackupWorker::new(db.pool.clone(), Arc::new(key(1)), config(dir.path()))
        .run_once()
        .await
        .unwrap();
    let target_db = Target::beside(&db);
    restore_from(
        &db,
        &summary.path,
        &key(1),
        &target_db,
        &dir.path().join("s"),
    )
    .await
    .unwrap();
    for (sql, expected) in [
        (
            format!(
                "SELECT status, error_code FROM turns WHERE id = x'{}'",
                root.simple()
            ),
            r#"["uncertain","connector_restored"]"#,
        ),
        (
            format!("SELECT status FROM turns WHERE id = x'{}'", child.simple()),
            r#"["interrupted"]"#,
        ),
        (
            format!(
                "SELECT status FROM delegations WHERE child_turn_id = x'{}'",
                child.simple()
            ),
            r#"["cancelled"]"#,
        ),
    ] {
        assert_eq!(rows(&target_db.url(), &sql).await, [expected], "{sql}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restored_profile_cannot_enable_native_agents() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    sqlx::query(
        "UPDATE provider_profiles SET reasoning_effort = 'ultra' WHERE provider_id = 'codex'",
    )
    .execute(&app.db.pool)
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let summary = worker(&app, key(1), config(dir.path()))
        .run_once()
        .await
        .unwrap();
    let target = Target::new(&app);
    let result = restore_into(&app, &summary.path, &key(1), &target, &dir.path().join("s")).await;
    assert!(
        matches!(result, Err(BackupError::Invalid(ref m)) if m.contains("not allowed")),
        "{result:?}"
    );
    assert!(
        !target.exists().await,
        "the refused restore left no database"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_imports_descriptions_its_backup_still_held() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let planner: Uuid = sqlx::query_scalar("SELECT id FROM bots WHERE name = 'Planner'")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO legacy_bot_descriptions (bot_id, description) VALUES (?1, ?2)")
        .bind(planner)
        .bind("Plans trips ✈")
        .execute(&app.db.pool)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let summary = worker(&app, key(1), config(dir.path()))
        .run_once()
        .await
        .unwrap();
    let target = Target::new(&app);
    restore_into(&app, &summary.path, &key(1), &target, &dir.path().join("s"))
        .await
        .unwrap();
    let imported = rows(
        &target.url(),
        &format!(
            "SELECT authority, substr(body, -13) FROM memory_entry
             WHERE bot_id = x'{}' AND body LIKE '%Plans trips ✈'",
            planner.simple()
        ),
    )
    .await;
    assert_eq!(imported, [r#"["tool","Plans trips ✈"]"#]);
    assert!(
        rows(&target.url(), "SELECT bot_id FROM legacy_bot_descriptions")
            .await
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_cleans_stale_staging_and_bounds_its_size() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
    let summary = worker(&app, key(1), config(dir.path()))
        .run_once()
        .await
        .unwrap();
    let staging = dir.path().join("staging");
    std::fs::create_dir(&staging).unwrap();
    let stale = staging.join(".restore-stale");
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join(backup::MARKER), "").unwrap();
    std::fs::write(stale.join("database.dump"), "PGDMP old plaintext").unwrap();
    std::fs::create_dir(staging.join(".restore-foreign")).unwrap();
    let server = DatabaseParams::parse(app.db.url.expose()).unwrap();
    let target = Target::new(&app);
    let small = restore(RestoreRequest {
        set_dir: &summary.path,
        key: &key(1),
        server: &server,
        target_database: &target.name,
        staging_parent: &staging,
        max_bytes: 100,
        computers: None,
    })
    .await;
    assert!(
        matches!(small, Err(BackupError::TooLarge(100))),
        "{small:?}"
    );
    assert!(!target.exists().await);
    restore_into(&app, &summary.path, &key(1), &target, &staging)
        .await
        .unwrap();
    assert!(!stale.exists(), "stale plaintext removed");
    assert!(staging.join(".restore-foreign").exists(), "not ours, kept");
}

#[tokio::test(flavor = "multi_thread")]
async fn categories_and_assignments_survive_backup_and_restore() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let main = app.main_bot().await;
    let made = app
        .owner(
            "POST",
            "/v1/categories",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "Household" })),
        )
        .await;
    let id = made.body["category"]["id"].as_str().unwrap().to_owned();
    app.owner(
        "POST",
        "/v1/categories",
        Some(json!({ "request_id": Uuid::new_v4(), "name": "Empty one" })),
    )
    .await;
    let reply = app
        .owner(
            "PATCH",
            &format!("/v1/bots/{main}/category"),
            Some(json!({ "category_id": id })),
        )
        .await;
    assert_eq!(reply.status, axum::http::StatusCode::OK);
    let dir = tempfile::tempdir().unwrap();
    let summary = worker(&app, key(1), config(dir.path()))
        .run_once()
        .await
        .unwrap();
    let target = Target::new(&app);
    restore_into(&app, &summary.path, &key(1), &target, dir.path())
        .await
        .unwrap();
    let source = app.db.url.expose().to_owned();
    for sql in [
        "SELECT id, name FROM bot_categories",
        "SELECT id, category_id FROM bots",
        "SELECT default_bot_id FROM installation",
    ] {
        assert_eq!(
            rows(&target.url(), sql).await,
            rows(&source, sql).await,
            "{sql}"
        );
    }
    assert_eq!(
        rows(&target.url(), "SELECT name FROM bot_categories")
            .await
            .len(),
        2
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_database_only_backup_restores_without_touching_the_schedule() {
    let _serial = SERIAL.lock().await;
    let db = common::TestDb::migrated().await;
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let run = |args: &[&str]| {
        let out = common::api_command(&db, &data, 0)
            .env("ELSE_MSB_BIN", data.join("unavailable-msb"))
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    };
    let (ok, out) = run(&["setup"]);
    assert!(ok, "{out}");
    let (ok, out) = run(&["help"]);
    assert!(ok && out.contains("backup --database-only"), "{out}");
    let (ok, out) = run(&["backup", "--database-only"]);
    assert!(ok, "{out}");
    let set = PathBuf::from(
        out.lines()
            .find_map(|l| l.strip_prefix("backup set: "))
            .unwrap(),
    );
    let backups = data.join("backups");
    assert_eq!(
        set.parent().unwrap(),
        backups.join(backup::BEFORE_UPDATE_DIR)
    );
    assert!(
        backup::completed_sets(&backups).unwrap().is_empty(),
        "the scheduled sets' folder stays empty, so the next scheduled set is still due"
    );
    let complete: backup::Completion =
        serde_json::from_slice(&std::fs::read(set.join("complete.json")).unwrap()).unwrap();
    assert!(!complete.whole_assistant);
    assert!(out.contains("Database only"), "{out}");
    for _ in 0..backup::BEFORE_UPDATE_KEEP {
        let (ok, out) = run(&["backup", "--database-only"]);
        assert!(ok, "{out}");
    }
    let before_update = backups.join(backup::BEFORE_UPDATE_DIR);
    let kept = backup::completed_sets(&before_update).unwrap();
    assert_eq!(
        kept.len(),
        backup::BEFORE_UPDATE_KEEP,
        "before-update sets keep their own small retention"
    );
    assert!(!set.exists(), "the oldest before-update set is removed");
    let set = before_update.join(kept.last().unwrap());

    let (ok, out) = run(&["backup", "--whole"]);
    assert!(!ok && out.contains("Usage:"), "{out}");

    let mut conn = sqlx::SqliteConnection::connect(db.url.expose())
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut conn)
        .await
        .unwrap();
    let job = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO guest_jobs (id, bot_id, command, status, deadline)
         VALUES (?1, ?2, 'sleep 60', 'running', strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '+1 hours'))",
    )
    .bind(job)
    .bind(Uuid::new_v4())
    .execute(&mut conn)
    .await
    .unwrap();
    let (ok, out) = run(&["backup", "--database-only"]);
    assert!(
        !ok && out.contains("background jobs are still running"),
        "{out}"
    );
    sqlx::query("DELETE FROM guest_jobs WHERE id = ?1")
        .bind(job)
        .execute(&mut conn)
        .await
        .unwrap();
    drop(conn);

    let app = TestApp::builder().db(std::sync::Arc::new(db)).start().await;
    let target = Target::new(&app);
    let db = app.db.clone();
    let out = common::api_command(&db, &data, 0)
        .args(["restore", set.to_str().unwrap(), &target.name])
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        rows(&target.url(), COMPARED[0]).await,
        rows(db.url.expose(), COMPARED[0]).await
    );
}
