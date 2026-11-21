use crate::common;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use common::recorder::{self, GATE};
use common::{TestApp, TestDb};
use else_api::backup::capture::{BackupConfig, Coordinator};
use else_api::backup::restore::{ComputerTarget, RestoreRequest, restore};
use else_api::backup::worker::BackupWorker;
use else_api::backup::{
    self, ArtifactKind, BackupError, Covered, DatabaseParams, ProjectState, ProviderState, VmState,
};
use else_api::computers::{
    Acquire, ComputerConfig, ComputerManager, ComputerRuntime, ExecOutput, Mount, RuntimeError,
    RuntimeState, Spec,
};
use else_api::credentials::SecretKey;
use serde_json::json;
use sqlx::Connection;
use uuid::Uuid;

mod restores;
mod sets;
mod shared_mcp;

#[derive(Default)]
struct Fake {
    computers: Mutex<HashMap<String, (RuntimeState, String)>>,
    starts: AtomicUsize,
    execs: Mutex<Vec<String>>,
    fail_export: AtomicBool,
    restored: Mutex<Vec<Spec>>,
    mounts: Mutex<HashMap<String, Vec<Mount>>>,
    images: Mutex<HashMap<String, String>>,
    /// Images a restore can use offline, as `msb` caches them on import.
    cached: Mutex<HashSet<String>>,
    checkpoint_fails: AtomicBool,
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
        self.images
            .lock()
            .unwrap()
            .insert(spec.name.clone(), spec.image.clone());
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
    async fn export_disk(
        &self,
        name: &str,
        archive: &Path,
        with_image: bool,
    ) -> Result<(), RuntimeError> {
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
        let image = match with_image {
            true => self.images.lock().unwrap()[name].clone(),
            false => String::new(),
        };
        let msb = zstd::encode_all(format!("MSB:{image}\n{disk}").as_bytes(), 3).unwrap();
        std::fs::write(archive, msb).unwrap();
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
        let bytes = String::from_utf8(zstd::decode_all(&*std::fs::read(archive).unwrap()).unwrap())
            .unwrap();
        let (image, disk) = bytes
            .strip_prefix("MSB:")
            .unwrap()
            .split_once('\n')
            .unwrap();
        let disk = disk.to_owned();
        let mut cached = self.cached.lock().unwrap();
        if !image.is_empty() {
            cached.insert(image.to_owned());
        }
        if !cached.contains(&spec.image) {
            return Err(RuntimeError::Failed {
                operation: "snapshot restore".into(),
                message: format!("image {} is not cached", spec.image),
            });
        }
        drop(cached);
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

const PINNED: &str = "ghcr.io/else-bot/else-guest@sha256:2688c7291a6fb4b0efb77a09966ce013851939affdd994ac679036b3712daaaf";

async fn setup_with(projects: bool) -> Setup {
    setup_image(projects, PINNED).await
}

async fn setup_image(projects: bool, image: &str) -> Setup {
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
            image: image.into(),
            ..ComputerConfig::default()
        },
    ));
    manager.reconcile_startup().await.unwrap();
    let registry: common::Providers = vec![Arc::new(
        else_api::providers::synthetic::Synthetic::new(Duration::from_millis(1)),
    )];
    let app = TestApp::builder()
        .db(Arc::new(db))
        .providers(registry)
        .computers(manager.clone())
        .start()
        .await;
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
        Arc::new(key()),
        BackupConfig {
            dir: s.dir.path().join("backups"),
            keep: 7,
            max_bytes: 1 << 30,
            cadence: else_api::backup::capture::Cadence::Every(Duration::from_secs(3600)),
        },
    )
    .with_coordinator(Coordinator {
        turns: Some(else_api::backup::capture::Turns {
            scheduler: s.app.scheduler.clone(),
            gate: s.app.state.gate.clone(),
        }),
        computers: Some(s.manager.clone()),
        drain,
        exclusive: false,

        projects_dir: Some(s.dir.path().join("bots")),
        mcp_dir: Some(s.dir.path().join("mcp")),
    })
}

async fn chat(app: &TestApp, bot: &str, text: &str) {
    let reply = app.send(bot, Uuid::new_v4(), text, Some("synthetic")).await;
    let turn = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["completed"]).await;
}

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
            p.with_database(database).options.get_filename().display()
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

/// The sealed chunk files of the store a local set shares.
fn stored_chunks(set: &Path) -> Vec<Vec<u8>> {
    std::fs::read_dir(set.parent().unwrap().join("chunks"))
        .unwrap()
        .flat_map(|key| std::fs::read_dir(key.unwrap().path()).unwrap())
        .map(|chunk| std::fs::read(chunk.unwrap().path()).unwrap())
        .collect()
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
