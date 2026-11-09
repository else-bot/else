mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use common::{TestApp, TestDb};
use else_api::backup::capture::{BackupConfig, Coordinator};
use else_api::backup::chunks::{self, Index};
use else_api::backup::replicate::Replicator;
use else_api::backup::restore::{ComputerTarget, RestoreRequest, restore};
use else_api::backup::s3::{Put, S3, S3Config};
use else_api::backup::worker::BackupWorker;
use else_api::backup::{self, BackupError, DatabaseParams};
use else_api::computers::{
    Acquire, ComputerConfig, ComputerManager, ComputerRuntime, ExecOutput, RuntimeError,
    RuntimeState, Spec,
};
use else_api::credentials::SecretKey;
use sqlx::Connection;
use uuid::Uuid;

static EMULATOR: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is not set: run scripts/test-replication.py"))
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

async fn bucket() -> S3 {
    let name = format!("else-test-{}", &Uuid::new_v4().simple().to_string()[..12]);
    let s3 = S3::new(s3_config(&name)).unwrap();
    if let Err(err) = s3.create_bucket().await {
        panic!(
            "the S3 emulator is not reachable at {} ({err}); run \
             scripts/test-replication.py",
            s3.config().endpoint
        );
    }
    s3
}

fn compose(args: &[&str]) {
    let status = std::process::Command::new("docker")
        .current_dir(root())
        .args([
            "compose",
            "--project-name",
            &env("ELSE_TEST_COMPOSE_PROJECT"),
        ])
        .args(["--env-file", &env("ELSE_TEST_COMPOSE_ENV_FILE")])
        .args(["-f", &env("ELSE_TEST_COMPOSE_FILE")])
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
        DatabaseParams::parse(app.db.url.expose()).unwrap(),
        Arc::new(key()),
        config(dir),
    )
    .with_replicator(replicator)
}

async fn populated() -> TestApp {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Planner").await;
    let reply = app
        .send(&bot, Uuid::new_v4(), "Remember TEAL-8", Some("synthetic"))
        .await;
    app.wait_turn(reply.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    app
}

async fn remote_state(app: &TestApp, set: &str) -> Option<(String, Option<String>)> {
    sqlx::query_as("SELECT state, error FROM backup_remote WHERE set_id = ?1")
        .bind(set)
        .fetch_optional(&app.db.pool)
        .await
        .unwrap()
}

fn set_key(s3: &S3, set: &str, object: &str) -> String {
    s3.key(&format!("sets/{set}/{object}"))
}

/// Every chunk key in the bucket, sorted.
async fn chunk_keys(s3: &S3) -> Vec<String> {
    let mut keys: Vec<String> = s3
        .list(&s3.key("chunks/"))
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();
    keys.sort();
    keys
}

async fn chunk_ids(s3: &S3) -> Vec<String> {
    let mut ids: Vec<String> = chunk_keys(s3)
        .await
        .iter()
        .map(|k| k.rsplit('/').next().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

async fn index(s3: &S3, set: &str) -> Index {
    let complete = s3
        .get_bytes(&set_key(s3, set, "complete.json"), 1 << 16)
        .await
        .unwrap();
    let completion: backup::Completion = serde_json::from_slice(&complete).unwrap();
    assert_eq!(completion.manifest.object, chunks::INDEX);
    let sealed = s3
        .get_bytes(&set_key(s3, set, chunks::INDEX), 1 << 26)
        .await
        .unwrap();
    let identity = backup::identity_for(&key(), &completion.encryption).unwrap();
    let plain = backup::open_small(&sealed, &identity, 1 << 26).unwrap();
    Index::parse(&plain, &completion).unwrap()
}

fn referenced<'a>(indexes: impl IntoIterator<Item = &'a Index>) -> Vec<String> {
    let mut ids: Vec<String> = indexes
        .into_iter()
        .flat_map(|i| i.chunk_ids().map(str::to_owned))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Add `rows` rows of 32 KB of random text, so the next set has new chunks.
async fn grow(app: &TestApp, rows: u32) {
    sqlx::query("CREATE TABLE IF NOT EXISTS scratch (b text)")
        .execute(&app.db.pool)
        .await
        .unwrap();
    sqlx::query("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i < ?1) INSERT INTO scratch SELECT hex(randomblob(16000)) FROM n")
        .bind(rows)
        .execute(&app.db.pool)
        .await
        .unwrap();
}

async fn flip_byte(s3: &S3, key: &str) {
    let mut bytes = s3.get_bytes(key, 1 << 30).await.unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0x01;
    s3.put(key, bytes, false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn sets_are_verified_by_readback_and_published_create_only() {
    let _emulator = EMULATOR.read().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let worker = worker(&app, dir.path(), replicator(&app, &s3));
    let summary = worker.run_once().await.unwrap();
    let stats = summary.remote.clone().unwrap().unwrap();
    assert_eq!(stats.completed, std::slice::from_ref(&summary.set_id));
    assert!(stats.uploaded >= 1 && stats.reused == 0, "{stats:?}");
    assert_eq!(
        remote_state(&app, &summary.set_id).await.unwrap().0,
        "completed"
    );
    let (run, uploaded): (Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT b.remote_status, r.uploaded_bytes FROM backup_runs b
         JOIN backup_remote r ON r.set_id = b.set_id WHERE b.set_id = ?1",
    )
    .bind(&summary.set_id)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(run.as_deref(), Some("completed"));
    assert_eq!(uploaded, Some(stats.uploaded_bytes as i64));
    let index = index(&s3, &summary.set_id).await;
    assert_eq!(index.manifest, summary.manifest);
    assert_eq!(chunk_ids(&s3).await, referenced([&index]));
    let complete = set_key(&s3, &summary.set_id, "complete.json");
    let before = s3.get_bytes(&complete, 1 << 16).await.unwrap();
    assert_eq!(
        s3.put(&complete, b"{\"forged\":true}".to_vec(), true)
            .await
            .unwrap(),
        Put::Exists
    );
    assert_eq!(s3.get_bytes(&complete, 1 << 16).await.unwrap(), before);
    let again = worker.replicate_now().await.unwrap().unwrap();
    assert_eq!(again.uploaded + again.completed.len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn a_chunk_altered_before_publication_is_detected() {
    let _emulator = EMULATOR.read().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let mut tampering = replicator(&app, &s3);
    let target = s3.clone();
    tampering.before_publish = Some(Arc::new(move |_set: String| {
        let s3 = target.clone();
        Box::pin(async move {
            let chunk = chunk_keys(&s3).await.remove(0);
            flip_byte(&s3, &chunk).await;
        })
    }));
    let summary = worker(&app, dir.path(), tampering)
        .run_once()
        .await
        .unwrap();
    let remote = summary.remote.clone().unwrap();
    assert!(
        matches!(&remote, Err(e) if e.to_string().contains("does not match")),
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
    let worker = worker(&app, dir.path(), replicator(&app, &s3));
    let stats = worker.replicate_now().await.unwrap().unwrap();
    assert_eq!(stats.completed, std::slice::from_ref(&summary.set_id));
    assert_eq!(stats.uploaded, 1, "only the altered chunk again: {stats:?}");
    assert_eq!(
        chunk_ids(&s3).await,
        referenced([&index(&s3, &summary.set_id).await])
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn an_interrupted_upload_resumes_and_reuses_what_it_uploaded() {
    // Exclusive: the fault point is global and every replication reaches it.
    let _emulator = EMULATOR.write().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    grow(&app, 600).await;
    let worker = worker(&app, dir.path(), replicator(&app, &s3));
    else_api::faults::arm("replication.after_chunk");
    let summary = worker.run_once().await.unwrap();
    assert!(
        matches!(summary.remote, Some(Err(_))),
        "{:?}",
        summary.remote
    );
    assert_eq!(
        s3.head(&set_key(&s3, &summary.set_id, "complete.json"))
            .await
            .unwrap(),
        None
    );
    let left = chunk_ids(&s3).await;
    assert!(!left.is_empty(), "the crash left uploaded chunks");
    let restarted = BackupWorker::new(
        app.db.pool.clone(),
        DatabaseParams::parse(app.db.url.expose()).unwrap(),
        Arc::new(key()),
        config(dir.path()),
    )
    .with_replicator(replicator(&app, &s3));
    let stats = restarted.replicate_now().await.unwrap().unwrap();
    assert_eq!(stats.completed, std::slice::from_ref(&summary.set_id));
    assert!(stats.reused >= left.len(), "{stats:?}");
    assert!(stats.uploaded >= 1, "{stats:?}");
    let index = index(&s3, &summary.set_id).await;
    assert!(left.iter().all(|id| index.chunk_ids().any(|c| c == id)));
    assert_eq!(chunk_ids(&s3).await, referenced([&index]));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn an_unreachable_bucket_leaves_the_local_set_and_retries_later() {
    let _emulator = EMULATOR.write().await;
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
    let local = backup::completed_sets(&dir.path().join("backups")).unwrap();
    assert_eq!(local, [first.set_id.clone(), second.set_id.clone()]);
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
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn remote_retention_keeps_the_newest_complete_sets_and_sweeps_their_chunks() {
    let _emulator = EMULATOR.read().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let mut r = replicator(&app, &s3);
    r.keep = 2;
    r.grace = chrono::Duration::zero();
    let mut w = worker(&app, dir.path(), r.clone());
    w.config.keep = 2;
    let mut ids = Vec::new();
    let mut last = None;
    for _ in 0..4 {
        grow(&app, 200).await;
        let summary = w.run_once().await.unwrap();
        ids.push(summary.set_id);
        last = summary.remote;
    }
    let stats = last.unwrap().unwrap();
    assert!(stats.swept >= 1, "{stats:?}");
    assert!(stats.reused >= 1, "{stats:?}");
    assert_eq!(r.completed_remote().await.unwrap(), ids[2..].to_vec());
    let keys = s3.list(&s3.key("sets/")).await.unwrap();
    assert!(
        keys.iter()
            .all(|o| ids[2..].iter().any(|id| o.key.contains(id.as_str())))
    );
    let kept = [index(&s3, &ids[2]).await, index(&s3, &ids[3]).await];
    assert_eq!(
        chunk_ids(&s3).await,
        referenced(&kept),
        "only kept sets' chunks remain"
    );
    let stored: i64 =
        sqlx::query_scalar("SELECT stored_bytes FROM backup_bucket_usage WHERE destination = ?1")
            .bind(s3.destination())
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    let listed: u64 = s3
        .list(&s3.key(""))
        .await
        .unwrap()
        .iter()
        .map(|o| o.size)
        .sum();
    assert_eq!(stored as u64, listed);
    assert_eq!(
        backup::completed_sets(&dir.path().join("backups")).unwrap(),
        ids[2..].to_vec()
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn unreferenced_chunks_within_the_grace_period_are_kept() {
    let _emulator = EMULATOR.read().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let mut r = replicator(&app, &s3);
    r.keep = 1;
    let mut w = worker(&app, dir.path(), r.clone());
    w.config.keep = 1;
    grow(&app, 200).await;
    let first = w.run_once().await.unwrap().set_id;
    let orphans = chunk_ids(&s3).await;
    sqlx::query("DELETE FROM scratch")
        .execute(&app.db.pool)
        .await
        .unwrap();
    sqlx::query("VACUUM").execute(&app.db.pool).await.unwrap();
    let second = w.run_once().await.unwrap();
    assert_eq!(
        r.completed_remote().await.unwrap(),
        std::slice::from_ref(&second.set_id)
    );
    assert_ne!(first, second.set_id);
    let listed = chunk_ids(&s3).await;
    assert!(
        orphans.iter().all(|id| listed.contains(id)),
        "a fresh unreferenced chunk is kept"
    );
    assert_eq!(second.remote.unwrap().unwrap().swept, 0);
}

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
    async fn export_disk(&self, name: &str, archive: &Path, _: bool) -> Result<(), RuntimeError> {
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
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn a_remote_set_restores_into_a_fresh_installation_after_an_emulator_restart() {
    let _emulator = EMULATOR.write().await;
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
    let bot = app.create_bot("Builder").await;
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

            projects_dir: Some(dir.path().join("bots")),
            mcp_dir: Some(dir.path().join("mcp")),
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
    let source: Vec<String> = sqlx::query_scalar("SELECT json_array(hex(id), hex(conversation_id), seq, hex(turn_id), kind, text, provider_id, created_at) FROM items ORDER BY 1")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    std::fs::remove_dir_all(dir.path().join("backups")).unwrap();
    drop(app);
    drop(manager);
    drop(fake);
    compose(&["restart", "seaweedfs"]);
    compose(&["up", "-d", "--wait", "seaweedfs"]);
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
    let params = DatabaseParams::parse(db.url.expose()).unwrap();
    let target = format!("else_restore_{}", Uuid::new_v4().simple());
    let restored = restore(RestoreRequest {
        set_dir: &downloaded,
        key: &key(),
        server: &params,
        target_database: &target,
        staging_parent: &dir.path().join("staging"),
        max_bytes: 1 << 30,
        computers: Some(ComputerTarget {
            runtime: Some(fresh.clone()),
            data_dir: &new_dir,
        }),
    })
    .await;
    let target_options = params.with_database(&target).options;
    let rows: Option<Vec<String>> = match &restored {
        Ok(_) => {
            let mut conn = sqlx::SqliteConnection::connect_with(&target_options)
                .await
                .unwrap();
            Some(
                sqlx::query_scalar("SELECT json_array(hex(id), hex(conversation_id), seq, hex(turn_id), kind, text, provider_id, created_at) FROM items ORDER BY 1")
                    .fetch_all(&mut conn)
                    .await
                    .unwrap(),
            )
        }
        Err(_) => None,
    };
    common::remove_database(&params.options, &target);
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn a_real_bucket_round_trip_when_authorized() {
    if std::env::var("ELSE_LIVE_S3").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set ELSE_LIVE_S3=1 and ELSE_LIVE_S3_* for a real bucket");
        return;
    }
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
    for object in s3.list(&s3.key("")).await.unwrap() {
        s3.delete(&object.key).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn a_recorded_completion_is_not_trusted_without_its_index() {
    let _emulator = EMULATOR.read().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let w = worker(&app, dir.path(), replicator(&app, &s3));
    let summary = w.run_once().await.unwrap();
    assert!(matches!(summary.remote, Some(Ok(_))));
    sqlx::query("DELETE FROM backup_remote")
        .execute(&app.db.pool)
        .await
        .unwrap();
    flip_byte(&s3, &set_key(&s3, &summary.set_id, chunks::INDEX)).await;
    let stats = w.replicate_now().await.unwrap().unwrap();
    assert_eq!(stats.completed, std::slice::from_ref(&summary.set_id));
    assert_eq!(stats.uploaded, 0, "every chunk was still there: {stats:?}");
    assert_eq!(index(&s3, &summary.set_id).await.manifest, summary.manifest);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn a_damaged_chunk_is_repaired_from_the_local_set_or_reported() {
    let _emulator = EMULATOR.read().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    grow(&app, 200).await;
    let mut r = replicator(&app, &s3);
    r.sample = 1000;
    let w = worker(&app, dir.path(), r);
    let summary = w.run_once().await.unwrap();
    assert!(matches!(summary.remote, Some(Ok(_))));
    let ids = chunk_ids(&s3).await;
    let chunk = chunk_keys(&s3).await.remove(0);
    flip_byte(&s3, &chunk).await;
    let stats = w.replicate_now().await.unwrap().unwrap();
    assert_eq!((stats.damaged, stats.repaired), (1, 1), "{stats:?}");
    assert_eq!(chunk_ids(&s3).await, ids);
    let clean = w.replicate_now().await.unwrap().unwrap();
    assert_eq!((clean.damaged, clean.repaired), (0, 0), "{clean:?}");
    flip_byte(&s3, &chunk).await;
    std::fs::remove_dir_all(summary.path).unwrap();
    let lost = w.replicate_now().await.unwrap_err().to_string();
    assert!(
        lost.contains("cannot be restored") && lost.contains(&summary.set_id),
        "{lost}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn another_key_on_the_same_prefix_never_touches_these_chunks() {
    let _emulator = EMULATOR.read().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let summary = worker(&app, dir.path(), replicator(&app, &s3))
        .run_once()
        .await
        .unwrap();
    assert!(matches!(summary.remote, Some(Ok(_))));
    let before = chunk_keys(&s3).await;
    let other =
        SecretKey::parse(&base64::engine::general_purpose::STANDARD.encode([7u8; 32])).unwrap();
    let mut r = replicator(&app, &s3);
    r.sample = 1000;
    r.grace = chrono::Duration::zero();
    let stats = r
        .replicate_pending(&dir.path().join("backups"), &other)
        .await
        .unwrap();
    assert_eq!(
        (stats.checked, stats.damaged, stats.swept),
        (0, 0, 0),
        "{stats:?}"
    );
    assert_eq!(chunk_keys(&s3).await, before);
    else_api::backup::replicate::download_set(
        &s3,
        &summary.set_id,
        &dir.path().join("download"),
        &key(),
        1 << 30,
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn a_bucket_rule_that_expires_chunks_is_refused() {
    let _emulator = EMULATOR.read().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    s3.put_lifecycle(
        "<LifecycleConfiguration><Rule><ID>expire-old</ID><Status>Enabled</Status>\
         <Filter><Prefix>else-test/</Prefix></Filter><Expiration><Days>30</Days></Expiration>\
         </Rule></LifecycleConfiguration>",
    )
    .await
    .unwrap();
    let summary = worker(&app, dir.path(), replicator(&app, &s3))
        .run_once()
        .await
        .unwrap();
    let remote = summary.remote.unwrap();
    assert!(
        matches!(&remote, Err(e) if matches!(**e, BackupError::Refused(_)) && e.to_string().contains("expire-old")),
        "{remote:?}"
    );
    assert!(chunk_ids(&s3).await.is_empty(), "nothing was uploaded");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn a_set_built_from_earlier_runs_chunks_restores_into_a_fresh_database() {
    let _emulator = EMULATOR.read().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let w = worker(&app, dir.path(), replicator(&app, &s3));
    grow(&app, 2000).await;
    let first = w.run_once().await.unwrap();
    grow(&app, 20).await;
    let second = w.run_once().await.unwrap();
    app.create_bot("Later").await;
    let third = w.run_once().await.unwrap();
    for summary in [&second, &third] {
        let stats = summary.remote.clone().unwrap().unwrap();
        assert!(stats.reused >= 1, "{stats:?}");
    }
    let (first_index, third_index) = (
        index(&s3, &first.set_id).await,
        index(&s3, &third.set_id).await,
    );
    let shared = referenced([&first_index])
        .into_iter()
        .filter(|id| third_index.chunk_ids().any(|c| c == id))
        .count();
    assert!(
        shared >= 1,
        "the third set uses chunks the first run uploaded"
    );
    let source: Vec<String> =
        sqlx::query_scalar("SELECT json_array(hex(id), name) FROM bots ORDER BY 1")
            .fetch_all(&app.db.pool)
            .await
            .unwrap();
    assert!(source.iter().any(|row| row.contains("Later")), "{source:?}");
    let scratch: i64 = sqlx::query_scalar("SELECT count(*) FROM scratch")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    std::fs::remove_dir_all(dir.path().join("backups")).unwrap();
    let downloaded = else_api::backup::replicate::download_set(
        &s3,
        &third.set_id,
        &dir.path().join("download"),
        &key(),
        1 << 30,
    )
    .await
    .unwrap();
    let params = DatabaseParams::parse(app.db.url.expose()).unwrap();
    let target = format!("else_restore_{}", Uuid::new_v4().simple());
    let restored = restore(RestoreRequest {
        set_dir: &downloaded,
        key: &key(),
        server: &params,
        target_database: &target,
        staging_parent: &dir.path().join("staging"),
        max_bytes: 1 << 30,
        computers: None,
    })
    .await;
    let target_options = params.with_database(&target).options;
    let rows = match &restored {
        Ok(_) => {
            let mut conn = sqlx::SqliteConnection::connect_with(&target_options)
                .await
                .unwrap();
            let bots: Vec<String> =
                sqlx::query_scalar("SELECT json_array(hex(id), name) FROM bots ORDER BY 1")
                    .fetch_all(&mut conn)
                    .await
                    .unwrap();
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM scratch")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            Some((bots, count))
        }
        Err(_) => None,
    };
    common::remove_database(&params.options, &target);
    restored.unwrap();
    assert_eq!(rows.unwrap(), (source, scratch));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn another_destination_is_replicated_afresh() {
    let _emulator = EMULATOR.read().await;
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
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn unverified_or_lower_coverage_sets_never_displace_the_recovery_point() {
    let _emulator = EMULATOR.read().await;
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
    let bot: Uuid = app.create_bot("Builder").await.parse().unwrap();
    let Acquire::Ready { lease_id, .. } = manager.acquire(bot, "test:use").await.unwrap() else {
        panic!()
    };
    manager.release(lease_id).await.unwrap();
    let s3 = bucket().await;
    let mut r = replicator(&app, &s3);
    r.keep = 1;
    let whole = worker(&app, dir.path(), r.clone())
        .with_coordinator(Coordinator {
            scheduler: Some(app.scheduler.clone()),
            computers: Some(manager.clone()),
            drain: Duration::from_secs(5),
            exclusive: false,

            projects_dir: Some(dir.path().join("bots")),
            mcp_dir: Some(dir.path().join("mcp")),
        })
        .run_once()
        .await
        .unwrap();
    assert!(whole.manifest.coverage.whole_assistant);
    let forged = "29991231T235959999Z-ffffffff";
    s3.put(&set_key(&s3, forged, "complete.json"), b"{}".to_vec(), true)
        .await
        .unwrap();
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
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn the_schedule_keeps_capturing_while_the_bucket_is_down_and_uploads_at_start() {
    let _emulator = EMULATOR.read().await;
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
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
#[ignore = "requires the optional Compose S3 emulator; run with --ignored"]
async fn restore_remote_works_without_the_source_database() {
    let _emulator = EMULATOR.read().await;
    let app = populated().await;
    let s3 = bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let summary = worker(&app, dir.path(), replicator(&app, &s3))
        .run_once()
        .await
        .unwrap();
    assert!(matches!(summary.remote, Some(Ok(_))));
    let params = DatabaseParams::parse(app.db.url.expose()).unwrap();
    let absent = format!("sqlite://{}", dir.path().join("absent.sqlite3").display());
    let data = dir.path().join("operator");
    std::fs::create_dir(&data).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let target = dir
        .path()
        .join(format!("restored-{}.sqlite3", Uuid::new_v4()))
        .display()
        .to_string();
    let config = s3.config().clone();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_else-api"))
        .current_dir(common::without_dotenv(dir.path()))
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
    let restored = params
        .with_database(&target)
        .options
        .get_filename()
        .exists();
    common::remove_database(&params.options, &target);
    assert!(out.status.success(), "{text}");
    assert!(restored);
    assert!(!text.contains(config.secret_access_key.as_str()));
}
