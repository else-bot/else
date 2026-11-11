mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use common::TestApp;
use else_api::backup::capture::BackupConfig;
use else_api::backup::restore::{RestoreRequest, restore};
use else_api::backup::worker::BackupWorker;
use else_api::backup::{self, BackupError, DatabaseParams};
use else_api::credentials::SecretKey;
use serde_json::json;
use sqlx::Connection;
use uuid::Uuid;

fn entries(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_name() != ".lock")
        .count()
}

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn key_text(byte: u8) -> String {
    let mut bytes = [byte; 32];
    bytes[0] = 0xA5;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn key(byte: u8) -> SecretKey {
    SecretKey::parse(&key_text(byte)).unwrap()
}

fn config(dir: &Path) -> BackupConfig {
    BackupConfig {
        dir: dir.join("backups"),
        keep: 7,
        max_bytes: 1 << 30,
        interval: Duration::from_secs(3600),
    }
}

fn worker(app: &TestApp, key: SecretKey, config: BackupConfig) -> BackupWorker {
    BackupWorker::new(
        app.db.pool.clone(),
        DatabaseParams::parse(app.db.url.expose()).unwrap(),
        Arc::new(key),
        config,
    )
}

struct Target {
    name: String,
}
impl Target {
    fn new(app: &TestApp) -> Self {
        let p = DatabaseParams::parse(app.db.url.expose()).unwrap();
        Self {
            name: p
                .options
                .get_filename()
                .parent()
                .unwrap()
                .join(format!("restore-{}.sqlite3", Uuid::new_v4()))
                .display()
                .to_string(),
        }
    }
    fn url(&self) -> String {
        format!("sqlite://{}", self.name)
    }
    async fn exists(&self) -> bool {
        Path::new(&self.name).exists()
    }
}
impl Drop for Target {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.name));
        }
    }
}

async fn restore_into(
    app: &TestApp,
    set: &Path,
    key: &SecretKey,
    target: &Target,
    staging: &Path,
) -> Result<else_api::backup::restore::RestoreReport, BackupError> {
    let server = DatabaseParams::parse(app.db.url.expose()).unwrap();
    restore(RestoreRequest {
        set_dir: set,
        key,
        server: &server,
        target_database: &target.name,
        staging_parent: staging,
        max_bytes: 1 << 30,
        computers: None,
    })
    .await
}

async fn rows(url: &str, sql: &str) -> Vec<String> {
    let mut conn = sqlx::SqliteConnection::connect(url).await.unwrap();
    common::rows(&mut conn, sql).await
}

async fn populated() -> TestApp {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Planner").await;
    for text in ["first message ünïcode", "second message"] {
        let reply = app
            .send(&bot, Uuid::new_v4(), text, Some("synthetic"))
            .await;
        let turn = reply.body["turn"]["id"].as_str().unwrap().to_owned();
        app.wait_turn(&turn, &["completed"]).await;
    }
    let patched = app
        .owner(
            "PATCH",
            "/v1/settings/providers/codex",
            Some(json!({ "reasoning_effort": "high", "service_tier": "default" })),
        )
        .await;
    assert!(patched.status.is_success(), "{:?}", patched.body);
    app
}

const COMPARED: &[&str] = &[
    "SELECT id, name, status FROM bots",
    "SELECT id, bot_id, kind, last_turn_no FROM conversations",
    "SELECT id, conversation_id, seq, kind, text FROM items",
    "SELECT provider_id, model, reasoning_effort, service_tier, revision FROM provider_profiles",
    "SELECT version, description, checksum, success FROM _sqlx_migrations",
    "SELECT bot_id, seq, kind, body FROM memory_entry",
];

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
    assert_eq!(manifest.coverage.vm, "omitted");
    assert!(
        manifest
            .bots
            .iter()
            .all(|b| b.vm == "none" && b.computer.is_none())
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
        [
            ".else-backup",
            "complete.json",
            "database.dump.age",
            "manifest.json.age"
        ]
    );
    use std::os::unix::fs::PermissionsExt;
    for name in &names {
        let mode = std::fs::metadata(summary.path.join(name))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{name} is private");
    }
    let text = key_text(1);
    let raw = base64::engine::general_purpose::STANDARD
        .decode(&text)
        .unwrap();
    let (_, _, identity) = else_api::backup::restore::open_set(&summary.path, &key(1)).unwrap();
    let dump = std::fs::read(summary.path.join(backup::DATABASE)).unwrap();
    let plain = backup::open_small(&dump, &identity, 1 << 30).unwrap();
    assert!(
        plain.starts_with(b"SQLite format 3\0"),
        "a SQLite database image"
    );
    for bytes in names
        .iter()
        .map(|n| std::fs::read(summary.path.join(n)).unwrap())
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

    let damaged = |name: &str, change: &dyn Fn(&Path)| {
        let root = dir.path().join(name);
        std::fs::create_dir(&root).unwrap();
        let copy = root.join(&summary.set_id);
        std::fs::create_dir(&copy).unwrap();
        for file in ["complete.json", backup::DATABASE, backup::MANIFEST] {
            std::fs::copy(summary.path.join(file), copy.join(file)).unwrap();
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
        .filter(|p| !p.ends_with(".lock"))
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
async fn scheduling_serializes_runs_and_keeps_the_newest_sets() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.keep = 2;
    let worker = Arc::new(worker(&app, key(1), cfg));
    assert_eq!(
        worker.next_due().unwrap(),
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
    let due = worker.next_due().unwrap();
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
    let mut cfg = config(empty.path());
    cfg.interval = Duration::from_secs(3600);
    let scheduled = Arc::new(BackupWorker::new(
        app.db.pool.clone(),
        DatabaseParams::parse(app.db.url.expose()).unwrap(),
        Arc::new(key(1)),
        cfg,
    ));
    let stop = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(scheduled.clone().run(stop.clone()));
    for _ in 0..200 {
        if !backup::completed_sets(&empty.path().join("backups"))
            .unwrap()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        backup::completed_sets(&empty.path().join("backups"))
            .unwrap()
            .len(),
        1
    );
    stop.cancel();
    task.await.unwrap();
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
async fn the_backup_and_restore_commands_work_on_an_installation() {
    let _serial = SERIAL.lock().await;
    let db = common::TestDb::migrated().await;
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let run = |args: &[&str]| {
        let out = common::api_command(&db, &data, common::free_port())
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
    let app = TestApp::with_db(db);
    let target = Target::new(&app);
    let db = app.db.clone();
    let restored_data = dir.path().join("restored-data");
    let mut command = common::api_command(&db, &data, common::free_port());
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
    let out = common::api_command(&db, &data, common::free_port())
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
async fn cleanup_and_retention_touch_only_owned_real_set_directories() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
    let backups = dir.path().join("backups");
    std::fs::create_dir(&backups).unwrap();
    std::fs::create_dir(backups.join("archive")).unwrap();
    std::fs::write(backups.join("archive/photo.jpg"), "keep").unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("complete.json"), "{}").unwrap();
    std::fs::write(outside.join(backup::MARKER), "").unwrap();
    std::os::unix::fs::symlink(&outside, backups.join("20000101T000000000Z-00000000")).unwrap();
    let mut cfg = config(dir.path());
    cfg.keep = 1;
    let worker = worker(&app, key(1), cfg);
    worker.run_once().await.unwrap();
    let newest = worker.run_once().await.unwrap();
    assert!(
        newest.path.join("complete.json").exists(),
        "the set just made is kept"
    );
    assert_eq!(
        backup::completed_sets(&backups).unwrap(),
        std::slice::from_ref(&newest.set_id)
    );
    assert_eq!(
        std::fs::read_to_string(backups.join("archive/photo.jpg")).unwrap(),
        "keep"
    );
    assert!(
        outside.join("complete.json").exists(),
        "nothing deleted through a symlink"
    );
    assert_eq!(worker.recover().await.unwrap(), 0);
    assert!(backups.join("archive").exists());
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
    tokio::time::timeout(Duration::from_secs(10), async {
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
    .await
    .unwrap();
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(result, Err(BackupError::Interrupted)),
        "{result:?}"
    );
    assert!(backup::completed_sets(&backups).unwrap().is_empty());
    worker.run_once().await.unwrap();
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
        let out = common::api_command(&db, &data, common::free_port())
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

    let (ok, out) = run(&["backup", "--whole"]);
    assert!(!ok && out.contains("usage:"), "{out}");

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

    let app = TestApp::with_db(db);
    let target = Target::new(&app);
    let db = app.db.clone();
    let out = common::api_command(&db, &data, common::free_port())
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
