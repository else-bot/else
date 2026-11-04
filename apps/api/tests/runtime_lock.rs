//! Ownership of a data directory and database by real `else-api` processes,
//! and the listener manifest that lets several instances run side by side.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use else_api::config::DatabaseUrl;
use else_api::runtime_lock::{MANIFEST, Manifest};
use serde_json::json;
use uuid::Uuid;

/// An `else-api` command with no provider, sandbox, or account of the host.
fn api(root: &Path, data: &Path, database: Option<&Path>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_else-api"));
    command
        .current_dir(common::without_dotenv(root))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", root.join("home"))
        .env("ELSE_DATA_DIR", data)
        .env("ELSE_API_BIND", "127.0.0.1:0")
        .env("ELSE_SYNTHETIC_PROVIDER", "1")
        .env("ELSE_SHUTDOWN_GRACE_SECS", "2")
        .env("ELSE_BACKUP_INTERVAL_SECS", "0")
        .env("ELSE_CODEX_BIN", root.join("absent/codex"))
        .env("ELSE_CLAUDE_BIN", root.join("absent/claude"))
        .env("ELSE_MSB_BIN", root.join("absent/msb"))
        .env("CODEX_HOME", root.join("absent/codex-home"))
        .env("CLAUDE_CONFIG_DIR", root.join("absent/claude-home"))
        .env("RUST_LOG", "warn");
    if let Some(database) = database {
        command.env("DATABASE_URL", format!("sqlite://{}", database.display()));
    }
    command
}

fn refused(output: Output, why: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains(why), "{stderr}");
}

#[test]
fn commands_refuse_held_ownership_before_writing_anything() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let data = root.join("a");
    std::fs::create_dir(root.join("db")).unwrap();
    std::os::unix::fs::symlink(root.join("db"), root.join("alias")).unwrap();
    let database = root.join("db/else.sqlite3");
    let held = else_api::runtime_lock::acquire(
        &data,
        &DatabaseUrl::new(format!("sqlite://{}", database.display())),
    )
    .unwrap();

    for command in ["setup", "migrate", "serve", "backup"] {
        refused(
            api(root, &data, Some(&database))
                .arg(command)
                .output()
                .unwrap(),
            "already running with data directory",
        );
    }
    let other = root.join("b");
    for command in ["setup", "migrate", "serve"] {
        refused(
            api(root, &other, Some(&root.join("alias/else.sqlite3")))
                .arg(command)
                .output()
                .unwrap(),
            "already using this database",
        );
    }
    for dir in [&data, &other] {
        for credential in ["owner-token", "secret-key"] {
            assert!(!dir.join(credential).exists(), "{}", dir.display());
        }
    }
    assert!(!database.exists(), "no SQLite file was opened or migrated");

    drop(held);
    let output = api(root, &other, Some(&root.join("alias/else.sqlite3")))
        .arg("setup")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(database.exists() && other.join("owner-token").exists());
}

struct Served {
    child: std::process::Child,
    data: PathBuf,
}

impl Served {
    fn start(root: &Path, data: &Path, codex: Option<&Path>) -> Self {
        let mut setup = api(root, data, None);
        assert!(setup.arg("setup").output().unwrap().status.success());
        let mut serve = api(root, data, None);
        if let Some(codex) = codex {
            serve
                .env("ELSE_CODEX_BIN", codex.join("codex"))
                .env("CODEX_HOME", codex);
        }
        let child = serve.arg("serve").spawn().unwrap();
        Self {
            child,
            data: data.to_owned(),
        }
    }

    async fn manifest(&self) -> Manifest {
        for _ in 0..400 {
            if let Ok(bytes) = std::fs::read(self.data.join(MANIFEST)) {
                return serde_json::from_slice(&bytes).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no listener manifest in {}", self.data.display());
    }

    fn token(&self) -> String {
        std::fs::read_to_string(self.data.join("owner-token"))
            .unwrap()
            .trim()
            .to_owned()
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Stands in for Codex: passes the version check and fails every run, after
/// the API has written the bot's Codex configuration.
fn fake_codex(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("auth.json"), "{}").unwrap();
    let program = dir.join("codex");
    std::fs::write(
        &program,
        "#!/bin/sh\n[ \"$1\" = --version ] && echo 'codex-cli 0.166.0' && exit 0\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn find_files(dir: &Path, name: &str, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_files(&path, name, found);
        } else if path.file_name().is_some_and(|n| n == name) {
            found.push(path);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn port_zero_instances_publish_their_bound_listener_and_callback() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let codex = root.join("codex");
    fake_codex(&codex);
    let mut a = Served::start(root, &root.join("a"), Some(&codex));
    let b = Served::start(root, &root.join("b"), None);
    let (ma, mb) = tokio::join!(a.manifest(), b.manifest());
    assert_eq!(ma.pid, a.child.id());
    assert_eq!(mb.pid, b.child.id());
    assert_ne!(ma.url, mb.url);
    let client = reqwest::Client::new();
    for (served, manifest) in [(&a, &ma), (&b, &mb)] {
        assert!(
            manifest.url.starts_with("http://127.0.0.1:"),
            "{manifest:?}"
        );
        assert!(!manifest.url.ends_with(":0"), "{manifest:?}");
        let bots: serde_json::Value = client
            .get(format!("{}/v1/bots", manifest.url))
            .bearer_auth(served.token())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(bots["default_bot_id"].is_string(), "{bots}");
    }

    let bots: serde_json::Value = client
        .get(format!("{}/v1/bots", ma.url))
        .bearer_auth(a.token())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let main = bots["default_bot_id"].as_str().unwrap();
    let sent = client
        .post(format!("{}/v1/bots/{main}/turns", ma.url))
        .bearer_auth(a.token())
        .json(&json!({ "request_id": Uuid::new_v4(), "text": "hi", "provider_id": "codex" }))
        .send()
        .await
        .unwrap();
    assert_eq!(sent.status(), 202);
    let callback = format!("url = \"{}/tools/v1/mcp\"", ma.url);
    let mut configs = Vec::new();
    for _ in 0..400 {
        configs.clear();
        find_files(&a.data, "config.toml", &mut configs);
        if !configs.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(configs.len(), 1, "{configs:?}");
    let text = std::fs::read_to_string(&configs[0]).unwrap();
    assert!(text.contains(&callback), "{callback} not in {text}");

    // SAFETY: signalling a child this test spawned and still owns.
    assert_eq!(unsafe { libc::kill(a.child.id() as i32, libc::SIGTERM) }, 0);
    assert!(a.child.wait().unwrap().success());
    assert!(
        !a.data.join(MANIFEST).exists(),
        "a stopped API withdraws it"
    );
    assert_eq!(b.manifest().await, mb, "the other instance is untouched");
}
