//! POC-022: coordinated assistant backups with a fake computer runtime
//! (the live check with real VMs is in `computers_live.rs`).

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use common::{TestApp, TestDb};
use else_api::backup::capture::{BackupConfig, Coordinator};
use else_api::backup::restore::{ComputerTarget, RestoreRequest, restore};
use else_api::backup::worker::BackupWorker;
use else_api::backup::{self, BackupError, PgParams};
use else_api::computers::{
    Acquire, ComputerConfig, ComputerManager, ComputerRuntime, ExecOutput, RuntimeError,
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
        Ok(())
    }
    async fn start(&self, name: &str) -> Result<(), RuntimeError> {
        self.starts.fetch_add(1, SeqCst);
        self.set(name, RuntimeState::Running)
    }
    async fn stop(&self, name: &str) -> Result<(), RuntimeError> {
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
        PgParams::parse(s.app.db.url.expose()).unwrap(),
        Arc::new(key()),
        BackupConfig {
            dir: s.dir.path().join("backups"),
            pg_dump: PathBuf::from("pg_dump"),
            pg_restore: PathBuf::from("pg_restore"),
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
    params: PgParams,
}

impl Target {
    fn new(app: &TestApp) -> Self {
        Self {
            name: format!("else_restore_{}", Uuid::new_v4().simple()),
            params: PgParams::parse(app.db.url.expose()).unwrap(),
        }
    }
    fn url(&self, database: &str) -> String {
        let p = &self.params;
        format!(
            "postgresql://{}:{}@{}:{}/{database}",
            p.user(),
            p.password().unwrap_or_default(),
            p.host(),
            p.port()
        )
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        let url = self.url("postgres");
        let name = self.name.clone();
        let _ = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async move {
                    if let Ok(mut conn) = sqlx::PgConnection::connect(&url).await {
                        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                            "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
                        )))
                        .execute(&mut conn)
                        .await;
                    }
                })
        })
        .join();
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
    assert_eq!(execs, ["/usr/local/bin/else-checkpoint"], "{execs:?}");
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
        pg_restore: Path::new("pg_restore"),
        staging_parent: &s.dir.path().join("staging"),
        max_bytes: 1 << 30,
        computers: Some(ComputerTarget {
            runtime: fresh.clone(),
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
    let mut conn = sqlx::PgConnection::connect(&target.url(&target.name))
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
        "native sessions retired: context is rebuilt from PostgreSQL"
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
    let closed = s.app.scheduler.gate().close().unwrap();
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
        pg_restore: Path::new("pg_restore"),
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
