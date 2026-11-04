//! POC-022: coordinated assistant backups with a fake computer runtime
//! (the live check with real VMs is in `computers_live.rs`).

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use common::{TestApp, TestDb};
use else_api::backup::capture::{BackupConfig, Coordinator};
use else_api::backup::restore::{ComputerTarget, RestoreRequest, restore};
use else_api::backup::worker::BackupWorker;
use else_api::backup::{self, BackupError, DatabaseParams};
use else_api::computers::{
    Acquire, ComputerConfig, ComputerManager, ComputerRuntime, ExecOutput, Mount, RuntimeError,
    RuntimeState, Spec,
};
use else_api::credentials::SecretKey;
use serde_json::json;
use sqlx::Connection;
use uuid::Uuid;

#[derive(Default)]
struct Fake {
    /// name -> (state, disk contents)
    computers: Mutex<HashMap<String, (RuntimeState, String)>>,
    starts: AtomicUsize,
    execs: Mutex<Vec<String>>,
    fail_export: AtomicBool,
    restored: Mutex<Vec<Spec>>,
    /// name -> the mounts it was created or restored with
    mounts: Mutex<HashMap<String, Vec<Mount>>>,
    checkpoint_fails: AtomicBool,
    /// Restore boots the computer, then fails.
    restore_fails_running: AtomicBool,
    stop_fails: AtomicBool,
}

impl Fake {
    fn set(&self, name: &str, state: RuntimeState) -> Result<(), RuntimeError> {
        match self.computers.lock().unwrap().get_mut(name) {
            Some(entry) => {
                entry.0 = state;
                Ok(())
            }
            None => Err(RuntimeError::Failed {
                operation: "control".into(),
                message: "missing".into(),
            }),
        }
    }
    fn write(&self, name: &str, disk: &str) {
        self.computers.lock().unwrap().get_mut(name).unwrap().1 = disk.into();
    }
    fn disk(&self, name: &str) -> Option<String> {
        self.computers
            .lock()
            .unwrap()
            .get(name)
            .map(|c| c.1.clone())
    }
}

fn ok() -> ExecOutput {
    ExecOutput {
        status: Some(0),
        stdout: String::new(),
        stderr: String::new(),
        truncated: false,
    }
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
        self.computers
            .lock()
            .unwrap()
            .insert(spec.name.clone(), (RuntimeState::Running, String::new()));
        self.mounts
            .lock()
            .unwrap()
            .insert(spec.name.clone(), spec.mounts.clone());
        Ok(())
    }
    async fn mounts(&self, name: &str) -> Result<Vec<Mount>, RuntimeError> {
        Ok(self
            .mounts
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_default())
    }
    async fn start(&self, name: &str) -> Result<(), RuntimeError> {
        self.starts.fetch_add(1, SeqCst);
        self.set(name, RuntimeState::Running)
    }
    async fn stop(&self, name: &str) -> Result<(), RuntimeError> {
        if self.stop_fails.load(SeqCst) {
            return Err(RuntimeError::Unavailable("stop timed out".into()));
        }
        self.set(name, RuntimeState::Stopped)
    }
    async fn pause(&self, name: &str) -> Result<(), RuntimeError> {
        self.set(name, RuntimeState::Paused)
    }
    async fn resume(&self, name: &str) -> Result<(), RuntimeError> {
        self.set(name, RuntimeState::Running)
    }
    async fn exec(
        &self,
        _: &str,
        program: &str,
        _: &[&str],
        _: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        self.execs.lock().unwrap().push(program.to_owned());
        if program.ends_with("else-checkpoint") && self.checkpoint_fails.load(SeqCst) {
            return Ok(ExecOutput {
                status: Some(1),
                stderr: "chromium did not exit".into(),
                ..ok()
            });
        }
        Ok(ok())
    }
    async fn copy_out(&self, _: &str, _: &str, _: &Path) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn export_disk(&self, name: &str, archive: &Path) -> Result<(), RuntimeError> {
        if self.fail_export.load(SeqCst) {
            return Err(RuntimeError::Failed {
                operation: "snapshot export".into(),
                message: "disk full".into(),
            });
        }
        let (state, disk) = self.computers.lock().unwrap().get(name).cloned().unwrap();
        assert_eq!(
            state,
            RuntimeState::Stopped,
            "only stopped disks are exported"
        );
        std::fs::write(archive, format!("MSB:{disk}")).unwrap();
        Ok(())
    }
    async fn restore_disk(&self, archive: &Path, spec: &Spec) -> Result<(), RuntimeError> {
        if self.restore_fails_running.load(SeqCst) {
            self.computers
                .lock()
                .unwrap()
                .insert(spec.name.clone(), (RuntimeState::Running, String::new()));
            return Err(RuntimeError::Failed {
                operation: "snapshot restore".into(),
                message: "agent did not answer".into(),
            });
        }
        let bytes = std::fs::read_to_string(archive).unwrap();
        let disk = bytes.strip_prefix("MSB:").unwrap().to_owned();
        self.computers
            .lock()
            .unwrap()
            .insert(spec.name.clone(), (RuntimeState::Stopped, disk));
        self.restored.lock().unwrap().push(spec.clone());
        Ok(())
    }
}

fn key() -> SecretKey {
    SecretKey::parse(&base64::engine::general_purpose::STANDARD.encode([7u8; 32])).unwrap()
}

struct Setup {
    app: TestApp,
    fake: Arc<Fake>,
    manager: Arc<ComputerManager>,
    dir: tempfile::TempDir,
}

async fn setup() -> Setup {
    setup_with(false).await
}

/// With `projects`, computers are created with their bot's project mount.
async fn setup_with(projects: bool) -> Setup {
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        fake.clone(),
        ComputerConfig {
            desktop: true,
            state_dir: dir.path().join("state"),
            idle_after: Duration::from_secs(3600),
            projects_dir: projects.then(|| dir.path().join("bots")),
            ..ComputerConfig::default()
        },
    ));
    manager.reconcile_startup().await.unwrap();
    let registry = else_api::providers::Registry::new(vec![Arc::new(
        else_api::providers::synthetic::Synthetic::new(Duration::from_millis(1)),
    )]);
    let app = TestApp::with_parts(Arc::new(db), 4, registry, Some(manager.clone()));
    Setup {
        app,
        fake,
        manager,
        dir,
    }
}

fn worker(s: &Setup, drain: Duration) -> BackupWorker {
    BackupWorker::new(
        s.app.db.pool.clone(),
        DatabaseParams::parse(s.app.db.url.expose()).unwrap(),
        Arc::new(key()),
        BackupConfig {
            dir: s.dir.path().join("backups"),
            keep: 7,
            max_bytes: 1 << 30,
            interval: Duration::from_secs(3600),
        },
    )
    .with_coordinator(Coordinator {
        scheduler: Some(s.app.scheduler.clone()),
        computers: Some(s.manager.clone()),
        drain,
        exclusive: false,

        projects_dir: Some(s.dir.path().join("bots")),
    })
}

async fn chat(app: &TestApp, bot: &str, text: &str) {
    let reply = app.send(bot, Uuid::new_v4(), text, Some("synthetic")).await;
    let turn = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["completed"]).await;
}

/// A database dropped when the test ends.
struct Target {
    name: String,
    params: DatabaseParams,
}

impl Target {
    fn new(app: &TestApp) -> Self {
        Self {
            name: format!("else_restore_{}", Uuid::new_v4().simple()),
            params: DatabaseParams::parse(app.db.url.expose()).unwrap(),
        }
    }
    fn url(&self, database: &str) -> String {
        let p = &self.params;
        format!(
            "sqlite://{}",
            p.with_database(&format!("{database}"))
                .options
                .get_filename()
                .display()
        )
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        let path = self
            .params
            .with_database(&self.name)
            .options
            .get_filename()
            .to_owned();
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn one_set_holds_the_database_and_every_computer_at_one_checkpoint() {
    let s = setup().await;
    let a = s.app.create_bot("Alpha", "keeps notes").await;
    let b = s.app.create_bot("Beta", "browses").await;
    let (a_id, b_id): (Uuid, Uuid) = (a.parse().unwrap(), b.parse().unwrap());
    chat(&s.app, &a, "Alpha's chat").await;
    chat(&s.app, &b, "Beta's chat").await;
    // A: used and left running, idle. B: used, then stopped by the owner.
    for (bot, disk) in [
        (a_id, "alpha files + browser profile"),
        (b_id, "beta files"),
    ] {
        let Acquire::Ready {
            lease_id,
            runtime_name,
            ..
        } = s.manager.acquire(bot, "test:use").await.unwrap()
        else {
            panic!()
        };
        s.fake.write(&runtime_name, disk);
        s.manager.release(lease_id).await.unwrap();
    }
    s.manager.owner_stop(b_id).await.unwrap();
    let starts = s.fake.starts.load(SeqCst);
    let checkpoints_before = s.fake.execs.lock().unwrap().len();

    let summary = worker(&s, Duration::from_secs(5)).run_once().await.unwrap();
    let manifest = &summary.manifest;
    assert!(manifest.coverage.whole_assistant, "{:?}", manifest.coverage);
    assert_eq!(manifest.coverage.vm, "included");
    let entry = |id: Uuid| manifest.bots.iter().find(|b| b.id == id).unwrap().clone();
    let (ea, eb) = (entry(a_id), entry(b_id));
    assert_eq!((ea.vm.as_str(), eb.vm.as_str()), ("included", "included"));
    let (ca, cb) = (ea.computer.unwrap(), eb.computer.unwrap());
    assert!(ca.was_running && !cb.was_running);
    assert_eq!(cb.desired_state, "stopped", "the owner's stop is recorded");
    assert!(ca.desktop_port.is_some() && ca.desktop_password.is_some());
    assert!(
        !ea.provider_sessions.is_empty(),
        "sessions recorded, not included"
    );
    assert_eq!(ea.provider_state, "omitted-reauth-required");
    let main = manifest
        .bots
        .iter()
        .find(|b| b.name != "Alpha" && b.name != "Beta")
        .unwrap();
    assert_eq!(main.vm, "none");
    assert_eq!(
        manifest.artifacts.len(),
        3,
        "database and two disk archives"
    );
    // The stopped computer was never woken; the running one was stopped
    // through the normal stop, which checkpoints the desktop browser.
    assert_eq!(
        s.fake.starts.load(SeqCst),
        starts,
        "no start for the backup"
    );
    let execs = s.fake.execs.lock().unwrap()[checkpoints_before..].to_vec();
    // A verified checkpoint, then the one the normal stop runs.
    assert!(
        !execs.is_empty() && execs.iter().all(|e| e == "/usr/local/bin/else-checkpoint"),
        "{execs:?}"
    );
    let observed: Vec<String> =
        sqlx::query_scalar("SELECT observed_state FROM bot_computers ORDER BY bot_id")
            .fetch_all(&s.app.db.pool)
            .await
            .unwrap();
    assert!(observed.iter().all(|o| o == "stopped"));
    // Nothing in the set is plaintext: the archives are sealed.
    for artifact in &manifest.artifacts {
        let bytes = std::fs::read(summary.path.join(&artifact.object)).unwrap();
        assert!(bytes.starts_with(b"age-encryption.org/v1"));
    }
    assert!(
        !std::fs::read_dir(&summary.path).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".msb"))
    );

    // Restore with the original runtime gone: a new runtime and directory.
    let fresh = Arc::new(Fake::default());
    let new_dir = s.dir.path().join("restored-installation");
    std::fs::create_dir(&new_dir).unwrap();
    let target = Target::new(&s.app);
    let report = restore(RestoreRequest {
        set_dir: &summary.path,
        key: &key(),
        server: &target.params,
        target_database: &target.name,
        staging_parent: &s.dir.path().join("staging"),
        max_bytes: 1 << 30,
        computers: Some(ComputerTarget {
            runtime: Some(fresh.clone()),
            data_dir: &new_dir,
        }),
    })
    .await
    .unwrap();
    assert!(report.manifest.coverage.whole_assistant);
    let name = |id: Uuid| ComputerManager::runtime_name(id);
    assert_eq!(
        fresh.disk(&name(a_id)).unwrap(),
        "alpha files + browser profile"
    );
    assert_eq!(fresh.disk(&name(b_id)).unwrap(), "beta files");
    let specs = fresh.restored.lock().unwrap().clone();
    let spec_a = specs.iter().find(|s| s.name == name(a_id)).unwrap();
    assert_eq!(
        spec_a.network, "public",
        "the network policy is restored explicitly"
    );
    assert_eq!(spec_a.ports, vec![(ca.desktop_port.unwrap(), 6080)]);
    let password =
        std::fs::read_to_string(new_dir.join(format!("computers/{a_id}/vnc.pass"))).unwrap();
    assert_eq!(password.trim(), ca.desktop_password.unwrap());
    let mut conn = sqlx::SqliteConnection::connect(&target.url(&target.name))
        .await
        .unwrap();
    let computers: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT bot_id, observed_state, desired_state FROM bot_computers ORDER BY bot_id",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    for (bot, observed, desired) in &computers {
        assert_eq!(observed, "stopped");
        assert_eq!(desired, if *bot == b_id { "stopped" } else { "auto" });
    }
    let active: i64 =
        sqlx::query_scalar("SELECT count(*) FROM provider_sessions WHERE status = 'active'")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!(
        active, 0,
        "native sessions retired: context is rebuilt from SQLite"
    );
    let chats: Vec<String> =
        sqlx::query_scalar("SELECT text FROM items WHERE kind = 'user' ORDER BY text")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    assert_eq!(chats, ["Alpha's chat", "Beta's chat"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn live_work_defers_the_set_and_a_failed_component_publishes_nothing() {
    let s = setup().await;
    let a = s.app.create_bot("Alpha", "keeps notes").await;
    let a_id: Uuid = a.parse().unwrap();
    let backups = s.dir.path().join("backups");
    // A turn that does not finish within the drain time defers the set.
    let reply = s
        .app
        .send(
            &a,
            Uuid::new_v4(),
            "busy [synthetic:hang]",
            Some("synthetic"),
        )
        .await;
    let hung = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    s.app.wait_turn(&hung, &["running"]).await;
    let result = worker(&s, Duration::from_millis(300)).run_once().await;
    assert!(
        matches!(result, Err(BackupError::Deferred(_))),
        "{result:?}"
    );
    assert!(backup::completed_sets(&backups).unwrap().is_empty());
    assert!(!s.app.scheduler.gate().is_closed(), "the gate reopens");
    s.app
        .owner(
            "POST",
            &format!("/v1/bots/{a}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    s.app.wait_turn(&hung, &["interrupted"]).await;

    // A held computer (an open desktop or a background job) defers too.
    let Acquire::Ready { lease_id, .. } = s.manager.acquire(a_id, "desktop:x").await.unwrap()
    else {
        panic!()
    };
    let result = worker(&s, Duration::from_secs(2)).run_once().await;
    assert!(
        matches!(result, Err(BackupError::Deferred(ref m)) if m.contains("in use")),
        "{result:?}"
    );
    s.manager.release(lease_id).await.unwrap();

    // A failed disk export: no completed set, staging removed.
    s.fake.fail_export.store(true, SeqCst);
    let result = worker(&s, Duration::from_secs(2)).run_once().await;
    assert!(
        matches!(result, Err(BackupError::Producer(ref m)) if m.contains("disk full")),
        "{result:?}"
    );
    assert!(backup::completed_sets(&backups).unwrap().is_empty());
    let leftovers = std::fs::read_dir(&backups)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_name() != ".lock")
        .count();
    assert_eq!(leftovers, 0);
    s.fake.fail_export.store(false, SeqCst);
    let set = worker(&s, Duration::from_secs(2)).run_once().await.unwrap();
    assert!(set.manifest.coverage.whole_assistant);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_gate_refuses_changes_and_desktop_wakes_while_closed() {
    let s = setup().await;
    let bot = s.app.main_bot().await;
    let closed = s
        .app
        .scheduler
        .gate()
        .close(Duration::from_secs(1))
        .await
        .unwrap()
        .unwrap();
    let send = s
        .app
        .send(&bot, Uuid::new_v4(), "during backup", Some("synthetic"))
        .await;
    assert_eq!(
        send.status,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "{:?}",
        send.body
    );
    assert_eq!(send.body["error"]["code"], "maintenance");
    let desktop = s
        .app
        .owner("GET", &format!("/v1/bots/{bot}/desktop"), None)
        .await;
    assert_eq!(desktop.body["error"]["code"], "maintenance");
    let read = s.app.owner("GET", "/v1/bots", None).await;
    assert!(read.status.is_success());
    let stop = s
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "Stop work stays available");
    drop(closed);
    let send = s
        .app
        .send(&bot, Uuid::new_v4(), "after backup", Some("synthetic"))
        .await;
    assert!(send.status.is_success());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_set_with_computers_needs_a_new_installation_to_restore() {
    let s = setup().await;
    let a = s.app.create_bot("Alpha", "keeps notes").await;
    let Acquire::Ready { lease_id, .. } = s
        .manager
        .acquire(a.parse().unwrap(), "test:use")
        .await
        .unwrap()
    else {
        panic!()
    };
    s.manager.release(lease_id).await.unwrap();
    let summary = worker(&s, Duration::from_secs(2)).run_once().await.unwrap();
    let target = Target::new(&s.app);
    let result = restore(RestoreRequest {
        set_dir: &summary.path,
        key: &key(),
        server: &target.params,
        target_database: &target.name,
        staging_parent: &s.dir.path().join("staging"),
        max_bytes: 1 << 30,
        computers: None,
    })
    .await;
    assert!(matches!(result, Err(BackupError::Refused(_))), "{result:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_keeps_every_artifact_a_kept_set_needs() {
    let s = setup().await;
    let a = s.app.create_bot("Alpha", "keeps notes").await;
    let Acquire::Ready { lease_id, .. } = s
        .manager
        .acquire(a.parse().unwrap(), "test:use")
        .await
        .unwrap()
    else {
        panic!()
    };
    s.manager.release(lease_id).await.unwrap();
    let mut w = worker(&s, Duration::from_secs(2));
    w.config.keep = 1;
    let first = w.run_once().await.unwrap();
    let second = w.run_once().await.unwrap();
    assert!(!first.path.exists(), "the older set is removed whole");
    // Sets are standalone: the kept one has every artifact it names and
    // opens with its own manifest.
    let (_, manifest, _) = else_api::backup::restore::open_set(&second.path, &key()).unwrap();
    for artifact in &manifest.artifacts {
        let meta = std::fs::metadata(second.path.join(&artifact.object)).unwrap();
        assert_eq!(meta.len(), artifact.ciphertext_bytes, "{}", artifact.object);
    }
    assert_eq!(manifest.artifacts.len(), 2);
}

async fn used(s: &Setup, bot: Uuid) -> String {
    let Acquire::Ready {
        lease_id,
        runtime_name,
        ..
    } = s.manager.acquire(bot, "test:use").await.unwrap()
    else {
        panic!()
    };
    s.manager.release(lease_id).await.unwrap();
    runtime_name
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_checkpoints_and_desktop_secrets_are_required() {
    let s = setup().await;
    let a: Uuid = s
        .app
        .create_bot("Alpha", "keeps notes")
        .await
        .parse()
        .unwrap();
    let name = used(&s, a).await;
    // A failed checkpoint defers the set: the computer keeps running.
    s.fake.checkpoint_fails.store(true, SeqCst);
    let result = worker(&s, Duration::from_secs(2)).run_once().await;
    assert!(
        matches!(result, Err(BackupError::Deferred(ref m)) if m.contains("checkpoint")),
        "{result:?}"
    );
    assert_eq!(
        s.manager.runtime_state(&name).await.unwrap(),
        Some(RuntimeState::Running)
    );
    s.fake.checkpoint_fails.store(false, SeqCst);
    // A paused desktop is resumed for its checkpoint, then captured.
    s.fake.set(&name, RuntimeState::Paused).unwrap();
    sqlx::query("UPDATE bot_computers SET observed_state = 'paused' WHERE bot_id = ?1")
        .bind(a)
        .execute(&s.app.db.pool)
        .await
        .unwrap();
    let before = s.fake.execs.lock().unwrap().len();
    let set = worker(&s, Duration::from_secs(2)).run_once().await.unwrap();
    assert!(
        s.fake.execs.lock().unwrap()[before..]
            .iter()
            .any(|e| e.ends_with("else-checkpoint"))
    );
    let entry = set.manifest.bots.iter().find(|b| b.id == a).unwrap();
    assert_eq!(
        entry
            .computer
            .as_ref()
            .unwrap()
            .browser_checkpoint
            .as_deref(),
        Some("ok")
    );
    assert!(set.manifest.coverage.whole_assistant);
    // A missing desktop password fails the capture.
    std::fs::remove_file(s.manager.desktop_password_file(a)).unwrap();
    let result = worker(&s, Duration::from_secs(2)).run_once().await;
    assert!(
        matches!(result, Err(BackupError::Producer(ref m)) if m.contains("password")),
        "{result:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn host_artifacts_are_captured_and_a_partial_set_never_replaces_a_whole_one() {
    let s = setup().await;
    let a: Uuid = s
        .app
        .create_bot("Alpha", "keeps notes")
        .await
        .parse()
        .unwrap();
    used(&s, a).await;
    // A file the bot fetched to the host, registered with its hash.
    let id = Uuid::new_v4();
    let store = s.dir.path().join("state/artifacts").join(a.to_string());
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join(id.to_string()), b"REPORT-BYTES").unwrap();
    use sha2::Digest;
    sqlx::query(
        "INSERT INTO artifacts (id, bot_id, guest_path, size_bytes, sha256) VALUES (?1, ?2, '/r.txt', 12, ?3)",
    )
    .bind(id)
    .bind(a)
    .bind(sha2::Sha256::digest(b"REPORT-BYTES").to_vec())
    .execute(&s.app.db.pool)
    .await
    .unwrap();
    let mut w = worker(&s, Duration::from_secs(2));
    w.config.keep = 1;
    let whole = w.run_once().await.unwrap();
    assert!(whole.manifest.coverage.whole_assistant);
    assert!(
        whole
            .manifest
            .artifacts
            .iter()
            .any(|x| x.object == format!("file-{id}.age"))
    );
    // Restored into the new installation's artifact store.
    let fresh = Arc::new(Fake::default());
    let new_dir = s.dir.path().join("new");
    std::fs::create_dir(&new_dir).unwrap();
    let target = Target::new(&s.app);
    restore(RestoreRequest {
        set_dir: &whole.path,
        key: &key(),
        server: &target.params,
        target_database: &target.name,
        staging_parent: &s.dir.path().join("staging"),
        max_bytes: 1 << 30,
        computers: Some(ComputerTarget {
            runtime: Some(fresh),
            data_dir: &new_dir,
        }),
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(new_dir.join(format!("artifacts/{a}/{id}"))).unwrap(),
        b"REPORT-BYTES"
    );
    // The file disappears from the host: the next set is partial, and with
    // keep 1 it does not delete the whole-assistant set.
    std::fs::remove_file(store.join(id.to_string())).unwrap();
    let partial = w.run_once().await.unwrap();
    assert!(!partial.manifest.coverage.whole_assistant);
    assert!(
        whole.path.join("complete.json").exists(),
        "the last whole set is kept"
    );
    assert!(partial.path.join("complete.json").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_restore_stops_what_it_started_or_keeps_the_installation() {
    let s = setup().await;
    let a: Uuid = s
        .app
        .create_bot("Alpha", "keeps notes")
        .await
        .parse()
        .unwrap();
    let name = used(&s, a).await;
    let set = worker(&s, Duration::from_secs(2)).run_once().await.unwrap();
    for stop_fails in [false, true] {
        let fresh = Arc::new(Fake::default());
        fresh.restore_fails_running.store(true, SeqCst);
        fresh.stop_fails.store(stop_fails, SeqCst);
        let new_dir = s.dir.path().join(format!("new-{stop_fails}"));
        std::fs::create_dir(&new_dir).unwrap();
        let target = Target::new(&s.app);
        let result = restore(RestoreRequest {
            set_dir: &set.path,
            key: &key(),
            server: &target.params,
            target_database: &target.name,
            staging_parent: &s.dir.path().join("staging"),
            max_bytes: 1 << 30,
            computers: Some(ComputerTarget {
                runtime: Some(fresh.clone()),
                data_dir: &new_dir,
            }),
        })
        .await;
        let state = fresh.computers.lock().unwrap().get(&name).map(|c| c.0);
        if stop_fails {
            assert!(
                matches!(result, Err(BackupError::Leftover(_))),
                "{result:?}"
            );
        } else {
            assert!(
                matches!(result, Err(BackupError::Producer(_))),
                "{result:?}"
            );
            assert_eq!(
                state,
                Some(RuntimeState::Stopped),
                "what it started was stopped"
            );
        }
        let exists = target
            .params
            .with_database(&target.name)
            .options
            .get_filename()
            .exists();
        assert!(!exists, "the new database was dropped");
    }
}

/// Each bot's project directory is sealed into the set at the checkpoint
/// and restored into the new installation's `projects/`, and a computer
/// created with the project mount is restored bound to the restored copy.
#[tokio::test(flavor = "multi_thread")]
async fn project_directories_are_backed_up_and_restored_with_their_mounts() {
    let s = setup_with(true).await;
    let bots = s.dir.path().join("bots");
    let a = s.app.create_bot("Alpha", "keeps notes").await;
    let b = s.app.create_bot("Beta", "keeps sheets").await;
    let (a_id, b_id): (Uuid, Uuid) = (a.parse().unwrap(), b.parse().unwrap());
    chat(&s.app, &a, "Alpha's chat").await;
    // Alpha's computer is created with the mount (and its project).
    let Acquire::Ready {
        lease_id,
        runtime_name,
        ..
    } = s.manager.acquire(a_id, "test:use").await.unwrap()
    else {
        panic!()
    };
    s.fake.write(&runtime_name, "alpha disk");
    s.manager.release(lease_id).await.unwrap();
    let pa = bots.join(a_id.to_string());
    std::fs::create_dir_all(pa.join("sub")).unwrap();
    std::fs::write(pa.join("notes.md"), "alpha notes").unwrap();
    std::fs::write(pa.join("sub/data.csv"), "a,b\n").unwrap();
    std::os::unix::fs::symlink("sub/data.csv", pa.join("latest")).unwrap();
    // Beta never used a computer but keeps files in its project.
    let pb = bots.join(b_id.to_string());
    std::fs::create_dir_all(&pb).unwrap();
    std::fs::write(pb.join("sheet.csv"), "item,qty\n").unwrap();

    let summary = worker(&s, Duration::from_secs(5)).run_once().await.unwrap();
    let manifest = &summary.manifest;
    assert!(manifest.coverage.whole_assistant, "{:?}", manifest.coverage);
    assert_eq!(manifest.coverage.projects, "included");
    let entry = |id: Uuid| manifest.bots.iter().find(|e| e.id == id).unwrap().clone();
    let (ea, eb) = (entry(a_id), entry(b_id));
    assert_eq!(
        (ea.project.as_str(), eb.project.as_str()),
        ("included", "included")
    );
    assert_eq!((ea.vm.as_str(), eb.vm.as_str()), ("included", "none"));
    let main = manifest
        .bots
        .iter()
        .find(|e| e.name != "Alpha" && e.name != "Beta")
        .unwrap();
    assert_eq!(main.project, "none", "no directory, nothing to include");
    let mount = ea.computer.as_ref().unwrap().project_mount.clone().unwrap();
    assert_eq!(
        (mount.guest.as_str(), mount.uid),
        ("/home/elsebot/work", 1000),
        "a desktop computer's elsebot user owns the project"
    );
    for bot in [a_id, b_id] {
        let object = backup::projects::object(bot);
        assert!(
            manifest
                .artifacts
                .iter()
                .any(|x| x.object == object && x.kind == "project")
        );
        let bytes = std::fs::read(summary.path.join(&object)).unwrap();
        assert!(bytes.starts_with(b"age-encryption.org/v1"));
        assert!(!bytes.windows(11).any(|w| w == b"alpha notes"));
    }

    let fresh = Arc::new(Fake::default());
    let new_dir = s.dir.path().join("restored-installation");
    std::fs::create_dir(&new_dir).unwrap();
    let target = Target::new(&s.app);
    restore(RestoreRequest {
        set_dir: &summary.path,
        key: &key(),
        server: &target.params,
        target_database: &target.name,
        staging_parent: &s.dir.path().join("staging"),
        max_bytes: 1 << 30,
        computers: Some(ComputerTarget {
            runtime: Some(fresh.clone()),
            data_dir: &new_dir,
        }),
    })
    .await
    .unwrap();
    let projects = else_api::backup::restore::projects_dir(&new_dir);
    let ra = projects.join(a_id.to_string());
    assert_eq!(
        std::fs::read_to_string(ra.join("notes.md")).unwrap(),
        "alpha notes"
    );
    assert_eq!(
        std::fs::read_to_string(ra.join("sub/data.csv")).unwrap(),
        "a,b\n"
    );
    assert_eq!(
        std::fs::read_link(ra.join("latest")).unwrap(),
        Path::new("sub/data.csv")
    );
    assert_eq!(
        std::fs::read_to_string(projects.join(b_id.to_string()).join("sheet.csv")).unwrap(),
        "item,qty\n"
    );
    let spec = fresh.restored.lock().unwrap()[0].clone();
    assert_eq!(
        spec.mounts,
        vec![Mount {
            host: ra.canonicalize().unwrap(),
            guest: "/home/elsebot/work".into(),
            uid: 1000,
        }],
        "the restored computer binds the restored project"
    );

    // An entry that cannot be read is left out, and the set says so.
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        use std::os::unix::fs::PermissionsExt;
        let locked = pb.join("locked.csv");
        std::fs::write(&locked, "secret").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let partial = worker(&s, Duration::from_secs(5)).run_once().await.unwrap();
        let manifest = &partial.manifest;
        assert_eq!(manifest.coverage.projects, "partial");
        assert!(!manifest.coverage.whole_assistant);
        assert_eq!(
            manifest.bots.iter().find(|e| e.id == b_id).unwrap().project,
            "partial"
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}
