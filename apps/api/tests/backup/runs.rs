//! Backup runs: failures, recovery, scheduling, and the CLI commands.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn producer_failures_interruptions_and_overruns_leave_no_completed_set() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
    else_api::faults::arm("backup.snapshot");
    let result = worker(&app, key(1), config(dir.path())).run_once().await;
    assert!(
        matches!(result, Err(BackupError::Producer(_))),
        "{result:?}"
    );
    let backups = dir.path().join("backups");
    assert!(backup::completed_sets(&backups).unwrap().is_empty());
    assert_eq!(entries(&backups), 0, "staging removed");
    let (status, error): (String, Option<String>) = sqlx::query_as(
        "SELECT local_status, error FROM backup_runs ORDER BY started_at DESC LIMIT 1",
    )
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(status, "failed");
    assert!(!error.unwrap().contains(&key_text(1)));

    let mut small = config(dir.path());
    small.max_bytes = 1000;
    let result = worker(&app, key(1), small).run_once().await;
    assert!(
        matches!(result, Err(BackupError::TooLarge(1000))),
        "{result:?}"
    );
    assert_eq!(entries(&backups), 0);

    else_api::faults::arm("backup.before_complete");
    let worker = worker(&app, key(1), config(dir.path()));
    assert!(matches!(
        worker.run_once().await,
        Err(BackupError::Interrupted)
    ));
    let unfinished: Vec<PathBuf> = std::fs::read_dir(&backups)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| !p.ends_with(".lock") && !p.ends_with("chunks"))
        .collect();
    assert_eq!(unfinished.len(), 1);
    assert!(!unfinished[0].join("complete.json").exists());
    assert!(backup::completed_sets(&backups).unwrap().is_empty());
    let target = Target::new(&app);
    let result = restore_into(
        &app,
        &unfinished[0],
        &key(1),
        &target,
        &dir.path().join("s"),
    )
    .await;
    assert!(matches!(result, Err(BackupError::Invalid(_))), "{result:?}");
    assert!(!target.exists().await);
    sqlx::query("INSERT INTO backup_runs (id, set_id) VALUES (?1, 'crashed')")
        .bind(Uuid::new_v4())
        .execute(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(worker.recover().await.unwrap(), 1);
    assert_eq!(entries(&backups), 0);
    let running: i64 =
        sqlx::query_scalar("SELECT count(*) FROM backup_runs WHERE local_status = 'running'")
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(running, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_set_id_collision_keeps_the_existing_set() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let cancel = tokio_util::sync::CancellationToken::new();
    let coordinator = else_api::backup::capture::Coordinator::default();
    let set_id = backup::new_set_id();
    let key = key(1);
    let create = || {
        else_api::backup::capture::create_set(
            &app.db.pool,
            &key,
            &cfg,
            &set_id,
            &cancel,
            &coordinator,
        )
    };
    let first = create().await.unwrap();
    let again = create().await;
    assert!(matches!(again, Err(BackupError::Io(_))), "{again:?}");
    assert_eq!(
        backup::completed_sets(&cfg.dir).unwrap(),
        std::slice::from_ref(&set_id)
    );
    assert_eq!(
        entries(&cfg.dir),
        1,
        "the second attempt's staging is removed"
    );
    let target = Target::new(&app);
    restore_into(&app, &first.path, &key, &target, &dir.path().join("s"))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_completes_runs_whose_set_was_published() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
    let worker = worker(&app, key(1), config(dir.path()));
    let published = worker.run_once().await.unwrap();
    sqlx::query(
        "UPDATE backup_runs SET local_status = 'running', finished_at = NULL,
         ciphertext_bytes = NULL, coverage = NULL WHERE set_id = ?1",
    )
    .bind(&published.set_id)
    .execute(&app.db.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO backup_runs (id, set_id) VALUES (?1, 'crashed')")
        .bind(Uuid::new_v4())
        .execute(&app.db.pool)
        .await
        .unwrap();
    worker.recover().await.unwrap();
    let runs: Vec<(String, String, Option<String>, bool)> = sqlx::query_as(
        "SELECT set_id, local_status, error, finished_at IS NOT NULL FROM backup_runs ORDER BY set_id",
    )
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(
        runs,
        [
            (published.set_id.clone(), "completed".to_owned(), None, true),
            (
                "crashed".to_owned(),
                "failed".to_owned(),
                Some("interrupted before it finished".to_owned()),
                true
            ),
        ]
    );
    assert_eq!(
        backup::completed_sets(&dir.path().join("backups")).unwrap(),
        [published.set_id]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduling_serializes_runs_and_keeps_the_newest_sets() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.keep = 2;
    let worker = Arc::new(worker(&app, key(1), cfg));
    assert_eq!(
        worker.next_due(Duration::from_secs(3600)).unwrap(),
        Duration::ZERO,
        "nothing yet: due now"
    );
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(worker.run_once().await.unwrap().set_id);
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
    let sets = backup::completed_sets(&dir.path().join("backups")).unwrap();
    assert_eq!(sets, ids[1..].to_vec(), "the two newest kept");
    let due = worker.next_due(Duration::from_secs(3600)).unwrap();
    assert!(
        due > Duration::from_secs(3500) && due <= Duration::from_secs(3600),
        "{due:?}"
    );
    let (a, b) = tokio::join!(worker.run_once(), worker.run_once());
    let refused = [&a, &b]
        .iter()
        .filter(|r| matches!(r, Err(BackupError::Refused(m)) if m.contains("another backup")))
        .count();
    assert_eq!(refused, 1, "{a:?} {b:?}");
    let empty = tempfile::tempdir().unwrap();
    let mut app_config = TestApp::config(&app.db);
    app_config.backup = config(empty.path());
    let stop = tokio_util::sync::CancellationToken::new();
    let (_service, task) = else_api::backup::service::Backups::start(
        &app_config,
        app.db.pool.clone(),
        Arc::new(key(1)),
        Default::default(),
        stop.clone(),
    )
    .await
    .unwrap();
    let scheduled = empty.path().join("backups");
    common::until(
        "the scheduled backup",
        common::poll(|| {
            let sets = backup::completed_sets(&scheduled).unwrap_or_default();
            std::future::ready((!sets.is_empty()).then_some(()))
        }),
    )
    .await;
    common::never_polled(
        "another backup ran before its interval",
        Duration::from_millis(500),
        || {
            let completed = backup::completed_sets(&scheduled).unwrap();
            std::future::ready((completed.len() != 1).then_some(completed))
        },
    )
    .await;
    stop.cancel();
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_backup_and_restore_commands_work_on_an_installation() {
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
            .env("RUST_LOG", "info")
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
    let (ok, out) = run(&["backup"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("shared MCP skills, documentation and config are included"),
        "{out}"
    );
    let key = std::fs::read_to_string(data.join("secret-key")).unwrap();
    let key = key.trim();
    let set = out
        .lines()
        .find_map(|l| l.strip_prefix("backup set: "))
        .unwrap()
        .to_owned();
    let complete: backup::Completion = serde_json::from_slice(
        &std::fs::read(data.join("backups").join(&set).join("complete.json")).unwrap(),
    )
    .unwrap();
    assert!(!complete.whole_assistant);
    let app = TestApp::builder().db(std::sync::Arc::new(db)).start().await;
    let target = Target::new(&app);
    let db = app.db.clone();
    let restored_data = dir.path().join("restored-data");
    let mut command = common::api_command(&db, &data, 0);
    let out = command
        .env("RUST_LOG", "info")
        .args([
            "restore",
            &set,
            &target.name,
            restored_data.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains(&format!("into database {}", target.name)));
    assert!(restored_data.join("mcp").is_dir());
    assert!(!text.contains(key), "the key is never printed");
    assert_eq!(
        rows(&target.url(), COMPARED[0]).await,
        rows(db.url.expose(), COMPARED[0]).await
    );
    let other = Target::new(&app);
    let out = common::api_command(&db, &data, 0)
        .env("SECRET_KEY", key_text(9))
        .args(["restore", &set, &other.name])
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("made with key"));
    assert!(!other.exists().await);
    assert_eq!(
        std::fs::read_to_string(data.join("secret-key"))
            .unwrap()
            .trim(),
        key
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_waits_for_a_running_backup_and_capture_honors_cancellation() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
    let worker = Arc::new(worker(&app, key(1), config(dir.path())));
    let backups = dir.path().join("backups");
    let held = backup::try_lock(&backups).unwrap().unwrap();
    let staging = backups.join(".staging-20990101T000000000Z-aaaaaaaa");
    std::fs::create_dir(&staging).unwrap();
    std::fs::write(staging.join(backup::MARKER), "").unwrap();
    assert_eq!(worker.recover().await.unwrap(), 0);
    assert!(staging.exists());
    assert!(matches!(
        worker.run_once().await,
        Err(BackupError::Refused(_))
    ));
    drop(held);
    assert_eq!(worker.recover().await.unwrap(), 1);

    sqlx::raw_sql(
        "CREATE TABLE bulk (bytes BLOB); INSERT INTO bulk VALUES (randomblob(33554432));",
    )
    .execute(&app.db.pool)
    .await
    .unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let (w, c) = (worker.clone(), cancel.clone());
    let run = tokio::spawn(async move { w.run_once_until(&c).await });
    common::until("the backup to reach sealing", async {
        loop {
            let sealing = std::fs::read_dir(&backups)
                .unwrap()
                .flatten()
                .any(|e| e.path().join(backup::DATABASE).exists());
            if sealing {
                break;
            }
            assert!(!run.is_finished(), "capture ended before cancellation");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    cancel.cancel();
    let result = common::until("the cancelled backup to stop", run)
        .await
        .unwrap();
    assert!(
        matches!(result, Err(BackupError::Interrupted)),
        "{result:?}"
    );
    assert!(backup::completed_sets(&backups).unwrap().is_empty());
    worker.run_once().await.unwrap();
}
