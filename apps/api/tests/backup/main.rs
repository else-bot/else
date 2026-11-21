#[path = "../common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use common::TestApp;
use else_api::backup::capture::BackupConfig;
use else_api::backup::restore::{RestoreRequest, restore};
use else_api::backup::worker::BackupWorker;
use else_api::backup::{self, BackupError, Covered, DatabaseParams, VmState};
use else_api::credentials::SecretKey;
use else_api::db::turns::TurnStatus;
use serde_json::json;
use sqlx::Connection;
use uuid::Uuid;

mod restores;
mod runs;
mod storage;

/// Entries of a backup directory besides its lock and shared chunk store.
fn entries(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter(|e| {
            !matches!(
                e.as_ref().unwrap().file_name().to_str(),
                Some(".lock" | "chunks")
            )
        })
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
        cadence: else_api::backup::capture::Cadence::Every(Duration::from_secs(3600)),
    }
}

fn worker(app: &TestApp, key: SecretKey, config: BackupConfig) -> BackupWorker {
    BackupWorker::new(app.db.pool.clone(), Arc::new(key), config)
}

struct Target {
    name: String,
}
impl Target {
    fn new(app: &TestApp) -> Self {
        Self::beside(&app.db)
    }
    fn beside(db: &common::TestDb) -> Self {
        let p = DatabaseParams::parse(db.url.expose()).unwrap();
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
    restore_from(&app.db, set, key, target, staging).await
}

async fn restore_from(
    db: &common::TestDb,
    set: &Path,
    key: &SecretKey,
    target: &Target,
    staging: &Path,
) -> Result<else_api::backup::restore::RestoreReport, BackupError> {
    let server = DatabaseParams::parse(db.url.expose()).unwrap();
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
