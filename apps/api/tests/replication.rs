//! POC-023: bucket replication, end to end against the SeaweedFS emulator in
//! Compose (`docker compose up -d --wait seaweedfs`). Like the PostgreSQL
//! tests, these require the emulator: a missing one fails with a message.
//! Each test uses its own bucket; they run one at a time because some stop
//! or restart the container.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use common::{TestApp, TestDb};
use else_api::backup::capture::{BackupConfig, Coordinator};
use else_api::backup::replicate::Replicator;
use else_api::backup::restore::{ComputerTarget, RestoreRequest, restore};
use else_api::backup::s3::{Put, S3, S3Config};
use else_api::backup::worker::BackupWorker;
use else_api::backup::{self, BackupError, PgParams};
use else_api::computers::{
    Acquire, ComputerConfig, ComputerManager, ComputerRuntime, ExecOutput, RuntimeError,
    RuntimeState, Spec,
};
use else_api::credentials::SecretKey;
use sqlx::Connection;
use uuid::Uuid;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn env(name: &str) -> String {
    let _ = dotenvy::from_path(root().join(".env"));
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} is not set: copy the ELSE_TEST_S3_* entries from .env.example into .env")
    })
}

fn s3_config(bucket: &str) -> S3Config {
    S3Config {
        endpoint: env("ELSE_TEST_S3_ENDPOINT"),
        region: env("ELSE_TEST_S3_REGION"),
        bucket: bucket.to_owned(),
        prefix: "else-test/".into(),
        access_key_id: env("ELSE_TEST_S3_ACCESS_KEY_ID"),
        secret_access_key: zeroize::Zeroizing::new(env("ELSE_TEST_S3_SECRET_ACCESS_KEY")),
        path_style: true,
    }
}

/// A fresh bucket on the emulator; fails clearly if it is not running.
async fn bucket() -> S3 {
    let name = format!("else-test-{}", &Uuid::new_v4().simple().to_string()[..12]);
    let s3 = S3::new(s3_config(&name)).unwrap();
    if let Err(err) = s3.create_bucket().await {
        panic!(
            "the S3 emulator is not reachable at {} ({err}); start it with \
             `docker compose up -d --wait seaweedfs`",
            s3.config().endpoint
        );
    }
    s3
}

fn compose(args: &[&str]) {
    let status = std::process::Command::new("docker")
        .current_dir(root())
        .args(["compose", "--env-file", ".env", "-f", "infra/compose.yaml"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "docker compose {args:?}");
}

fn key() -> SecretKey {
    SecretKey::parse(&base64::engine::general_purpose::STANDARD.encode([9u8; 32])).unwrap()
}

fn config(dir: &Path) -> BackupConfig {
    BackupConfig {
        dir: dir.join("backups"),
        pg_dump: PathBuf::from("pg_dump"),
        pg_restore: PathBuf::from("pg_restore"),
        keep: 7,
        max_bytes: 1 << 30,
        interval: Duration::from_secs(3600),
    }
}

fn replicator(app: &TestApp, s3: &S3) -> Replicator {
    Replicator::new(s3.clone(), app.db.pool.clone(), 7)
}

fn worker(app: &TestApp, dir: &Path, replicator: Replicator) -> BackupWorker {
    BackupWorker::new(
        app.db.pool.clone(),
        PgParams::parse(app.db.url.expose()).unwrap(),
        Arc::new(key()),
        config(dir),
    )
    .with_replicator(replicator)
}

async fn populated() -> TestApp {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Planner", "plans trips").await;
    let reply = app
        .send(&bot, Uuid::new_v4(), "Remember TEAL-8", Some("synthetic"))
        .await;
    app.wait_turn(reply.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    app
}

async fn remote_state(app: &TestApp, set: &str) -> Option<(String, Option<String>)> {
    sqlx::query_as("SELECT state, error FROM backup_remote WHERE set_id = $1")
        .bind(set)
        .fetch_optional(&app.db.pool)
        .await
        .unwrap()
}

fn set_key(s3: &S3, set: &str, object: &str) -> String {
    s3.key(&format!("sets/{set}/{object}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn sets_are_verified_by_readback_and_published_create_only() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let worker = worker(&app, dir.path(), replicator(&app, &s3));
    let summary = worker.run_once().await.unwrap();
    let stats = summary.remote.clone().unwrap().unwrap();
    assert_eq!(stats.completed, std::slice::from_ref(&summary.set_id));
    assert_eq!(stats.uploaded, 2, "manifest and dump");
    assert_eq!(
        remote_state(&app, &summary.set_id).await.unwrap().0,
        "completed"
    );
    let run: Option<String> =
        sqlx::query_scalar("SELECT remote_status FROM backup_runs WHERE set_id = $1")
            .bind(&summary.set_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(run.as_deref(), Some("completed"));
    // Every remote object hashes to the manifest's ciphertext SHA-256.
    for artifact in &summary.manifest.artifacts {
        let (len, sha) = s3
            .get_with(
                &set_key(&s3, &summary.set_id, &artifact.object),
                u64::MAX,
                |_| Ok(()),
            )
            .await
            .unwrap();
        assert_eq!(
            (len, sha.as_str()),
            (
                artifact.ciphertext_bytes,
                artifact.ciphertext_sha256.as_str()
            )
        );
    }
    // A second publication of the completion record is refused and the
    // first one stays.
    let complete = set_key(&s3, &summary.set_id, "complete.json");
    let before = s3.get_bytes(&complete, 1 << 16).await.unwrap();
    assert_eq!(
        s3.put(&complete, b"{\"forged\":true}".to_vec(), true)
            .await
            .unwrap(),
        Put::Exists
    );
    assert_eq!(s3.get_bytes(&complete, 1 << 16).await.unwrap(), before);
    assert_eq!(
        before,
        std::fs::read(summary.path.join("complete.json")).unwrap()
    );
    // Nothing to do on the next pass.
    let again = worker.replicate_now().await.unwrap().unwrap();
    assert_eq!(again.uploaded + again.completed.len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_object_altered_before_publication_is_detected() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let mut tampering = replicator(&app, &s3);
    let target = s3.clone();
    tampering.before_publish = Some(Arc::new(move |set: String| {
        let s3 = target.clone();
        Box::pin(async move {
            let key = s3.key(&format!("sets/{set}/{}", backup::DATABASE));
            // Same length, one byte different: only the SHA-256 tells.
            let mut bytes = s3.get_bytes(&key, 1 << 30).await.unwrap();
            let at = bytes.len() / 2;
            bytes[at] ^= 0x01;
            s3.put(&key, bytes, false).await.unwrap();
        })
    }));
    let summary = worker(&app, dir.path(), tampering)
        .run_once()
        .await
        .unwrap();
    let remote = summary.remote.clone().unwrap();
    assert!(
        matches!(&remote, Err(e) if e.contains("does not match")),
        "{remote:?}"
    );
    assert!(
        summary.path.join("complete.json").exists(),
        "the local set stays complete"
    );
    assert_eq!(
        s3.head(&set_key(&s3, &summary.set_id, "complete.json"))
            .await
            .unwrap(),
        None,
        "not published remotely"
    );
    assert_eq!(
        remote_state(&app, &summary.set_id).await.unwrap().0,
        "failed"
    );
    // A clean pass repairs and publishes it.
    let worker = worker(&app, dir.path(), replicator(&app, &s3));
    let stats = worker.replicate_now().await.unwrap().unwrap();
    assert_eq!(stats.completed, std::slice::from_ref(&summary.set_id));
    assert_eq!(stats.uploaded, 1, "only the altered object again");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_upload_resumes_aborts_and_skips_what_matches() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    // A dump of several parts: S3 (and the emulator) need parts of at
    // least 5 MiB, so the database gets incompressible bulk first.
    sqlx::query("CREATE TABLE scratch (b text)")
        .execute(&app.db.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO scratch SELECT (SELECT string_agg(md5(random()::text || g::text || i::text), '')
                                     FROM generate_series(1, 2000) i)
         FROM generate_series(1, 300) g",
    )
    .execute(&app.db.pool)
    .await
    .unwrap();
    let mut small_parts = replicator(&app, &s3);
    small_parts.part_bytes = 5 * 1024 * 1024;
    let worker = worker(&app, dir.path(), small_parts.clone());
    else_api::faults::arm("replication.after_part");
    let summary = worker.run_once().await.unwrap();
    assert!(
        matches!(summary.remote, Some(Err(_))),
        "{:?}",
        summary.remote
    );
    let open = s3
        .list_multipart(&s3.key(&format!("sets/{}/", summary.set_id)))
        .await
        .unwrap();
    assert_eq!(open.len(), 1, "the crash left a multipart upload");
    assert_eq!(
        s3.head(&set_key(&s3, &summary.set_id, "complete.json"))
            .await
            .unwrap(),
        None
    );
    // The worker restarts: the same set completes.
    let restarted = BackupWorker::new(
        app.db.pool.clone(),
        PgParams::parse(app.db.url.expose()).unwrap(),
        Arc::new(key()),
        config(dir.path()),
    )
    .with_replicator(small_parts);
    let stats = restarted.replicate_now().await.unwrap().unwrap();
    assert_eq!(stats.aborted, 1);
    assert_eq!(stats.skipped, 1, "the manifest already matched");
    assert_eq!(stats.uploaded, 1);
    assert_eq!(stats.completed, std::slice::from_ref(&summary.set_id));
    assert!(
        s3.list_multipart(&s3.key(&format!("sets/{}/", summary.set_id)))
            .await
            .unwrap()
            .is_empty()
    );
    let dump = summary
        .manifest
        .artifacts
        .iter()
        .find(|a| a.kind == "postgres")
        .unwrap();
    let (_, sha) = s3
        .get_with(
            &set_key(&s3, &summary.set_id, backup::DATABASE),
            u64::MAX,
            |_| Ok(()),
        )
        .await
        .unwrap();
    assert_eq!(sha, dump.ciphertext_sha256);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_bucket_leaves_the_local_set_and_retries_later() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let mut w = worker(&app, dir.path(), replicator(&app, &s3));
    w.config.keep = 1;
    compose(&["stop", "seaweedfs"]);
    let first = w.run_once().await;
    let second = w.run_once().await;
    compose(&["up", "-d", "--wait", "seaweedfs"]);
    let (first, second) = (first.unwrap(), second.unwrap());
    for summary in [&first, &second] {
        assert!(summary.path.join("complete.json").exists());
        assert!(matches!(summary.remote, Some(Err(_))));
        assert_eq!(
            remote_state(&app, &summary.set_id).await.unwrap().0,
            "failed"
        );
    }
    // With keep 1, both awaiting sets are kept locally.
    let local = backup::completed_sets(&dir.path().join("backups")).unwrap();
    assert_eq!(local, [first.set_id.clone(), second.set_id.clone()]);
    // The emulator may need a moment after it reports healthy; the worker
    // retries the same way, with the same sets.
    let mut done = false;
    for _ in 0..30 {
        match w.replicate_now().await {
            Ok(_) => {
                done = true;
                break;
            }
            Err(err) => {
                eprintln!("retrying: {err}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    assert!(done, "replicated once the emulator returned");
    for summary in [&first, &second] {
        assert_eq!(
            remote_state(&app, &summary.set_id).await.unwrap().0,
            "completed"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_retention_keeps_the_newest_complete_sets() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let mut r = replicator(&app, &s3);
    r.keep = 2;
    let mut w = worker(&app, dir.path(), r.clone());
    w.config.keep = 2;
    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(w.run_once().await.unwrap().set_id);
    }
    assert_eq!(r.completed_remote().await.unwrap(), ids[2..].to_vec());
    // Older sets left no objects behind.
    let keys = s3.list(&s3.key("sets/")).await.unwrap();
    assert!(
        keys.iter()
            .all(|(k, _)| ids[2..].iter().any(|id| k.contains(id.as_str())))
    );
    assert_eq!(
        backup::completed_sets(&dir.path().join("backups")).unwrap(),
        ids[2..].to_vec()
    );
}

/// A computer runtime that keeps disks in memory (the real one is covered
/// by the live VM tests).
#[derive(Default)]
struct Fake {
    computers: Mutex<HashMap<String, (RuntimeState, String)>>,
}

#[async_trait]
impl ComputerRuntime for Fake {
    async fn list(&self) -> Result<Vec<(String, RuntimeState)>, RuntimeError> {
        Ok(self
            .computers
            .lock()
            .unwrap()
            .iter()
            .map(|(n, (s, _))| (n.clone(), *s))
            .collect())
    }
    async fn create(&self, spec: &Spec) -> Result<(), RuntimeError> {
        self.computers.lock().unwrap().insert(
            spec.name.clone(),
            (RuntimeState::Running, "GUEST-DISK".into()),
        );
        Ok(())
    }
    async fn start(&self, name: &str) -> Result<(), RuntimeError> {
        self.computers.lock().unwrap().get_mut(name).unwrap().0 = RuntimeState::Running;
        Ok(())
    }
    async fn stop(&self, name: &str) -> Result<(), RuntimeError> {
        if let Some(c) = self.computers.lock().unwrap().get_mut(name) {
            c.0 = RuntimeState::Stopped;
        }
        Ok(())
    }
    async fn pause(&self, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn resume(&self, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn exec(
        &self,
        _: &str,
        _: &str,
        _: &[&str],
        _: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        Ok(ExecOutput {
            status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
        })
    }
    async fn copy_out(&self, _: &str, _: &str, _: &Path) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn export_disk(&self, name: &str, archive: &Path) -> Result<(), RuntimeError> {
        let disk = self.computers.lock().unwrap().get(name).unwrap().1.clone();
        std::fs::write(archive, disk).unwrap();
        Ok(())
    }
    async fn restore_disk(&self, archive: &Path, spec: &Spec) -> Result<(), RuntimeError> {
        let disk = std::fs::read_to_string(archive).unwrap();
        self.computers
            .lock()
            .unwrap()
            .insert(spec.name.clone(), (RuntimeState::Stopped, disk));
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_set_restores_into_a_fresh_installation_after_an_emulator_restart() {
    let _serial = SERIAL.lock().await;
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        fake.clone(),
        ComputerConfig {
            state_dir: dir.path().join("state"),
            idle_after: Duration::from_secs(3600),
            ..ComputerConfig::default()
        },
    ));
    manager.reconcile_startup().await.unwrap();
    let registry = else_api::providers::Registry::new(vec![Arc::new(
        else_api::providers::synthetic::Synthetic::new(Duration::from_millis(1)),
    )]);
    let app = TestApp::with_parts(db.clone(), 4, registry, Some(manager.clone()));
    let bot = app.create_bot("Builder", "builds").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let reply = app
        .send(&bot, Uuid::new_v4(), "Remember TEAL-8", Some("synthetic"))
        .await;
    app.wait_turn(reply.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let Acquire::Ready { lease_id, .. } = manager.acquire(bot_id, "test:use").await.unwrap() else {
        panic!()
    };
    manager.release(lease_id).await.unwrap();
    let s3 = bucket().await;
    let r = replicator(&app, &s3);
    let summary = worker(&app, dir.path(), r.clone())
        .with_coordinator(Coordinator {
            scheduler: Some(app.scheduler.clone()),
            computers: Some(manager.clone()),
            drain: Duration::from_secs(5),
            exclusive: false,
        })
        .run_once()
        .await
        .unwrap();
    assert!(summary.manifest.coverage.whole_assistant);
    assert!(
        matches!(summary.remote, Some(Ok(_))),
        "{:?}",
        summary.remote
    );
    let source: Vec<String> = sqlx::query_scalar("SELECT t::text FROM items t ORDER BY 1")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    // Local artifacts and the original runtime are gone; the emulator
    // restarts in between.
    std::fs::remove_dir_all(dir.path().join("backups")).unwrap();
    drop(app);
    drop(manager);
    drop(fake);
    compose(&["restart", "seaweedfs"]);
    compose(&["up", "-d", "--wait", "seaweedfs"]);
    // It may answer 500 for a moment after its health check passes.
    let mut listed = None;
    for _ in 0..30 {
        match r.completed_remote().await {
            Ok(sets) => {
                listed = Some(sets);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
    assert_eq!(listed.unwrap(), std::slice::from_ref(&summary.set_id));
    // The emulator can still answer 500 right after its restart.
    let mut downloaded = None;
    for attempt in 0..10 {
        match else_api::backup::replicate::download_set(
            &r.s3,
            &summary.set_id,
            &dir.path().join(format!("download-{attempt}")),
            &key(),
            1 << 30,
        )
        .await
        {
            Ok(set) => {
                downloaded = Some(set);
                break;
            }
            Err(err) => {
                eprintln!("download attempt {attempt}: {err}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    let downloaded = downloaded.expect("the set downloads once the emulator is back");
    let fresh = Arc::new(Fake::default());
    let new_dir = dir.path().join("installation");
    std::fs::create_dir(&new_dir).unwrap();
    let params = PgParams::parse(db.url.expose()).unwrap();
    let target = format!("else_restore_{}", Uuid::new_v4().simple());
    let restored = restore(RestoreRequest {
        set_dir: &downloaded,
        key: &key(),
        server: &params,
        target_database: &target,
        pg_restore: Path::new("pg_restore"),
        staging_parent: &dir.path().join("staging"),
        max_bytes: 1 << 30,
        computers: Some(ComputerTarget {
            runtime: fresh.clone(),
            data_dir: &new_dir,
        }),
    })
    .await;
    let target_options = params.with_database(&target).options;
    let rows: Option<Vec<String>> = match &restored {
        Ok(_) => {
            let mut conn = sqlx::PgConnection::connect_with(&target_options)
                .await
                .unwrap();
            Some(
                sqlx::query_scalar("SELECT t::text FROM items t ORDER BY 1")
                    .fetch_all(&mut conn)
                    .await
                    .unwrap(),
            )
        }
        Err(_) => None,
    };
    let mut admin = sqlx::PgConnection::connect_with(&params.with_database("postgres").options)
        .await
        .unwrap();
    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS \"{target}\" WITH (FORCE)"
    )))
    .execute(&mut admin)
    .await;
    restored.unwrap();
    assert_eq!(rows.unwrap(), source, "chats restored from the bucket");
    assert_eq!(
        fresh
            .computers
            .lock()
            .unwrap()
            .get(&ComputerManager::runtime_name(bot_id))
            .map(|c| c.1.clone()),
        Some("GUEST-DISK".into()),
        "the computer's disk came from the bucket"
    );
    // A forged or incomplete remote set is not downloaded.
    let missing = else_api::backup::replicate::download_set(
        &r.s3,
        "20000101T000000000Z-00000000",
        &dir.path().join("download"),
        &key(),
        1 << 30,
    )
    .await;
    assert!(
        matches!(missing, Err(BackupError::Invalid(_))),
        "{missing:?}"
    );
}

/// Optional: the same upload, verification, conditional publication, and
/// remote download against a real R2 or S3 bucket the owner authorized, with
/// `ELSE_LIVE_S3=1` and `ELSE_LIVE_S3_ENDPOINT`, `_REGION`, `_BUCKET`,
/// `_PREFIX` (an isolated prefix), `_ACCESS_KEY_ID`, `_SECRET_ACCESS_KEY`.
/// Routine runs never contact a real bucket.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_bucket_round_trip_when_authorized() {
    if std::env::var("ELSE_LIVE_S3").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set ELSE_LIVE_S3=1 and ELSE_LIVE_S3_* for a real bucket");
        return;
    }
    let _serial = SERIAL.lock().await;
    let var = |n: &str| std::env::var(format!("ELSE_LIVE_S3_{n}")).unwrap();
    let s3 = S3::new(S3Config {
        endpoint: var("ENDPOINT"),
        region: var("REGION"),
        bucket: var("BUCKET"),
        prefix: format!(
            "{}/{}",
            var("PREFIX").trim_end_matches('/'),
            Uuid::new_v4().simple()
        ),
        access_key_id: var("ACCESS_KEY_ID"),
        secret_access_key: zeroize::Zeroizing::new(var("SECRET_ACCESS_KEY")),
        path_style: std::env::var("ELSE_LIVE_S3_PATH_STYLE").as_deref() != Ok("0"),
    })
    .unwrap();
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
    let r = replicator(&app, &s3);
    let summary = worker(&app, dir.path(), r.clone())
        .run_once()
        .await
        .unwrap();
    assert!(
        matches!(summary.remote, Some(Ok(_))),
        "{:?}",
        summary.remote
    );
    let complete = set_key(&s3, &summary.set_id, "complete.json");
    assert_eq!(
        s3.put(&complete, b"{}".to_vec(), true).await.unwrap(),
        Put::Exists
    );
    let downloaded = else_api::backup::replicate::download_set(
        &r.s3,
        &summary.set_id,
        &dir.path().join("download"),
        &key(),
        1 << 30,
    )
    .await
    .unwrap();
    else_api::backup::restore::open_set(&downloaded, &key()).unwrap();
    // Leave the isolated prefix empty.
    for (key, _) in s3.list(&s3.key("")).await.unwrap() {
        s3.delete(&key).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_completion_is_not_trusted_without_its_payload() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let w = worker(&app, dir.path(), replicator(&app, &s3));
    let summary = w.run_once().await.unwrap();
    assert!(matches!(summary.remote, Some(Ok(_))));
    // A crash after publication lost the record, and the remote dump was
    // damaged (same length) meanwhile.
    sqlx::query("DELETE FROM backup_remote")
        .execute(&app.db.pool)
        .await
        .unwrap();
    let key = set_key(&s3, &summary.set_id, backup::DATABASE);
    let mut bytes = s3.get_bytes(&key, 1 << 30).await.unwrap();
    bytes[10] ^= 0x01;
    s3.put(&key, bytes, false).await.unwrap();
    let stats = w.replicate_now().await.unwrap().unwrap();
    assert_eq!(stats.uploaded, 1, "the damaged object was repaired");
    assert_eq!(stats.completed, std::slice::from_ref(&summary.set_id));
    let dump = summary
        .manifest
        .artifacts
        .iter()
        .find(|a| a.kind == "postgres")
        .unwrap();
    let (_, sha) = s3.get_with(&key, u64::MAX, |_| Ok(())).await.unwrap();
    assert_eq!(sha, dump.ciphertext_sha256);
}

#[tokio::test(flavor = "multi_thread")]
async fn another_destination_is_replicated_afresh() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let first = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let summary = worker(&app, dir.path(), replicator(&app, &first))
        .run_once()
        .await
        .unwrap();
    assert!(matches!(summary.remote, Some(Ok(_))));
    let second = bucket().await;
    let r = replicator(&app, &second);
    assert_eq!(
        r.awaiting(&dir.path().join("backups")).await.unwrap(),
        std::slice::from_ref(&summary.set_id),
        "completion in another bucket does not count here"
    );
    let stats = worker(&app, dir.path(), r)
        .replicate_now()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.completed, std::slice::from_ref(&summary.set_id));
    assert!(
        second
            .head(&set_key(&second, &summary.set_id, "complete.json"))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unverified_or_lower_coverage_sets_never_displace_the_recovery_point() {
    let _serial = SERIAL.lock().await;
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        fake.clone(),
        ComputerConfig {
            state_dir: dir.path().join("state"),
            idle_after: Duration::from_secs(3600),
            ..ComputerConfig::default()
        },
    ));
    manager.reconcile_startup().await.unwrap();
    let registry = else_api::providers::Registry::new(vec![Arc::new(
        else_api::providers::synthetic::Synthetic::new(Duration::from_millis(1)),
    )]);
    let app = TestApp::with_parts(db.clone(), 4, registry, Some(manager.clone()));
    let bot: Uuid = app.create_bot("Builder", "builds").await.parse().unwrap();
    let Acquire::Ready { lease_id, .. } = manager.acquire(bot, "test:use").await.unwrap() else {
        panic!()
    };
    manager.release(lease_id).await.unwrap();
    let s3 = bucket().await;
    let mut r = replicator(&app, &s3);
    r.keep = 1;
    // A whole-assistant set first.
    let whole = worker(&app, dir.path(), r.clone())
        .with_coordinator(Coordinator {
            scheduler: Some(app.scheduler.clone()),
            computers: Some(manager.clone()),
            drain: Duration::from_secs(5),
            exclusive: false,
        })
        .run_once()
        .await
        .unwrap();
    assert!(whole.manifest.coverage.whole_assistant);
    // A forged newer remote set with a completion record of its own.
    let forged = "29991231T235959999Z-ffffffff";
    s3.put(&set_key(&s3, forged, "complete.json"), b"{}".to_vec(), true)
        .await
        .unwrap();
    // Then a database-only (partial) set, with keep 1.
    let mut partial_worker = worker(&app, dir.path(), r.clone());
    partial_worker.config.keep = 1;
    let partial = partial_worker.run_once().await.unwrap();
    assert!(!partial.manifest.coverage.whole_assistant);
    assert!(
        matches!(partial.remote, Some(Ok(_))),
        "{:?}",
        partial.remote
    );
    let remote = r.completed_remote().await.unwrap();
    assert!(
        remote.contains(&whole.set_id),
        "the newest whole set is kept: {remote:?}"
    );
    assert!(remote.contains(&partial.set_id));
    assert!(
        remote.contains(&forged.to_owned()),
        "an unverified set is left alone"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_schedule_keeps_capturing_while_the_bucket_is_down_and_uploads_at_start() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
    // An unreachable bucket: captures still happen on schedule.
    let unreachable = S3::new(S3Config {
        endpoint: "http://127.0.0.1:9".into(),
        ..s3_config("nowhere")
    })
    .unwrap();
    let mut w = worker(&app, dir.path(), replicator(&app, &unreachable));
    w.config.interval = Duration::from_secs(2);
    let w = Arc::new(w);
    let stop = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(w.clone().run(stop.clone()));
    tokio::time::sleep(Duration::from_secs(7)).await;
    stop.cancel();
    task.await.unwrap();
    let sets = backup::completed_sets(&dir.path().join("backups")).unwrap();
    assert!(sets.len() >= 2, "captures continued: {sets:?}");
    // A restarted worker with a reachable bucket uploads them at once,
    // before its next capture is due.
    let s3 = bucket().await;
    let mut w = worker(&app, dir.path(), replicator(&app, &s3));
    w.config.interval = Duration::from_secs(3600);
    let w = Arc::new(w);
    let stop = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(w.clone().run(stop.clone()));
    let r = replicator(&app, &s3);
    let mut done = false;
    for _ in 0..100 {
        if r.awaiting(&dir.path().join("backups"))
            .await
            .unwrap()
            .is_empty()
        {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stop.cancel();
    task.await.unwrap();
    assert!(done, "pending uploads ran at startup");
    assert_eq!(
        backup::completed_sets(&dir.path().join("backups"))
            .unwrap()
            .len(),
        sets.len(),
        "no new capture was due"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_remote_works_without_the_source_database() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let summary = worker(&app, dir.path(), replicator(&app, &s3))
        .run_once()
        .await
        .unwrap();
    assert!(matches!(summary.remote, Some(Ok(_))));
    // A replacement server: the configured database does not exist.
    let params = PgParams::parse(app.db.url.expose()).unwrap();
    let absent = format!(
        "postgresql://{}:{}@{}:{}/else_absent_{}",
        params.user(),
        params.password().unwrap_or_default(),
        params.host(),
        params.port(),
        Uuid::new_v4().simple()
    );
    let data = dir.path().join("operator");
    std::fs::create_dir(&data).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let target = format!("else_restore_{}", Uuid::new_v4().simple());
    let config = s3.config().clone();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_else-api"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &data)
        .env("DATABASE_URL", &absent)
        .env("ELSE_DATA_DIR", &data)
        .env(
            "SECRET_KEY",
            base64::engine::general_purpose::STANDARD.encode([9u8; 32]),
        )
        .env("ELSE_S3_ENDPOINT", &config.endpoint)
        .env("ELSE_S3_REGION", &config.region)
        .env("ELSE_S3_BUCKET", &config.bucket)
        .env("ELSE_S3_PREFIX", &config.prefix)
        .env("ELSE_S3_ACCESS_KEY_ID", &config.access_key_id)
        .env(
            "ELSE_S3_SECRET_ACCESS_KEY",
            config.secret_access_key.as_str(),
        )
        .args(["restore-remote", &summary.set_id, &target])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut admin = sqlx::PgConnection::connect_with(&params.with_database("postgres").options)
        .await
        .unwrap();
    let restored: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&target)
            .fetch_one(&mut admin)
            .await
            .unwrap();
    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS \"{target}\" WITH (FORCE)"
    )))
    .execute(&mut admin)
    .await;
    assert!(out.status.success(), "{text}");
    assert!(restored);
    assert!(!text.contains(config.secret_access_key.as_str()));
}
