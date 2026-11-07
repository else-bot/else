use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use else_api::config::DatabaseUrl;

struct Installation {
    _root: tempfile::TempDir,
    home: PathBuf,
}

impl Installation {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("installation with spaces");
        for name in ["config", "current/bin", "bin", "bots", "logs", "run"] {
            fs::create_dir_all(home.join(name)).unwrap();
        }
        fs::write(home.join("config/elsebot.env"), "ELSEBOT_MANAGER=process\n").unwrap();
        fs::write(
            home.join("config/.env"),
            format!(
                "ELSE_DATA_DIR='{}'\nELSE_BOTS_DIR='{}'\nELSE_BACKUP_INTERVAL_SECS=0\n",
                home.join("data").display(),
                home.join("bots").display(),
            ),
        )
        .unwrap();
        symlink(
            env!("CARGO_BIN_EXE_else-api"),
            home.join("current/bin/else-api"),
        )
        .unwrap();
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/elsebot"),
            home.join("bin/elsebot"),
        )
        .unwrap();
        let installation = Self { _root: root, home };
        installation.success(&["setup"]);
        installation
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_else-api"));
        self.environment(&mut command);
        command
    }

    fn environment(&self, command: &mut Command) {
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", self._root.path())
            .env("ELSE_HOME", &self.home)
            .env("RUST_LOG", "error")
            .current_dir(self.home.join("config"));
    }

    fn success(&self, args: &[&str]) -> Output {
        let result = self.command().args(args).output().unwrap();
        assert!(
            result.status.success(),
            "status: {}, stdout: {}, stderr: {}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        result
    }

    fn cli(&self, args: &[&str]) -> Output {
        let mut command = Command::new("bash");
        self.environment(&mut command);
        command.arg(self.home.join("bin/elsebot"));
        command.args(args).output().unwrap()
    }

    fn seed(&self) {
        for name in [
            "data/memory/note",
            "data/backups/old/complete.json",
            "data/sessions/history",
            "bots/old/project.txt",
            "logs/else-api.log",
            "run/stale.pid",
            "config/desktop.json",
            "config/desktop.layout.json",
            "config/.desktop.json.old.tmp",
        ] {
            let path = self.home.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "old test data").unwrap();
        }
    }

    fn token(&self) -> Vec<u8> {
        fs::read(self.home.join("data/owner-token")).unwrap()
    }
}

#[tokio::test]
async fn reset_erases_history_rotates_identity_and_reseeds_onboarding() {
    let installation = Installation::new();
    installation.seed();
    let token = installation.token();
    let key = fs::read(installation.home.join("data/secret-key")).unwrap();
    let settings = fs::read(installation.home.join("config/.env")).unwrap();
    let url = format!(
        "sqlite://{}",
        installation.home.join("data/else.sqlite3").display()
    );
    let pool = else_api::db::connect(&DatabaseUrl::new(url.clone()))
        .await
        .unwrap();
    let old_bot: uuid::Uuid = sqlx::query_scalar("SELECT id FROM bots")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE bots SET name = 'Unwanted test bot'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE installation SET onboarded_at = '2026-01-01T00:00:00Z'")
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    else_api::db::memory::append(
        &mut tx,
        old_bot,
        &else_api::db::memory::NewEntry {
            request_key: "old-test-memory",
            kind: else_api::db::memory::Kind::OwnerMessage,
            body: "Unwanted test memory",
            source: serde_json::json!({}),
            supersedes_seq: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    pool.close().await;

    for _ in 0..2 {
        let before = installation.token();
        let result = installation.cli(&["reset", "--yes"]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_ne!(before, installation.token());
    }
    assert_ne!(token, installation.token());
    assert_ne!(
        key,
        fs::read(installation.home.join("data/secret-key")).unwrap()
    );
    assert_eq!(
        settings,
        fs::read(installation.home.join("config/.env")).unwrap()
    );
    assert!(installation.home.join("current/bin/else-api").exists());
    assert!(installation.home.join("config/elsebot.env").exists());
    for name in [
        "data/memory",
        "data/backups",
        "data/sessions",
        "bots/old",
        "logs/else-api.log",
        "config/desktop.json",
        "config/desktop.layout.json",
        "config/.desktop.json.old.tmp",
        "data/api.json",
    ] {
        assert!(
            !installation.home.join(name).exists(),
            "{name} survived reset"
        );
    }
    let pool = else_api::db::connect(&DatabaseUrl::new(url)).await.unwrap();
    let bots: Vec<(uuid::Uuid, String)> = sqlx::query_as("SELECT id, name FROM bots")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(bots.len(), 1);
    assert_ne!(bots[0].0, old_bot);
    assert_ne!(bots[0].1, "Unwanted test bot");
    let onboarding: Option<String> = sqlx::query_scalar("SELECT onboarded_at FROM installation")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(onboarding.is_none());
    let memory: i64 = sqlx::query_scalar("SELECT count(*) FROM memory_entry")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(memory, 0);
    else_api::db::installation::check_installed(&pool)
        .await
        .unwrap();
    pool.close().await;
}

#[test]
fn confirmation_and_invalid_arguments_preserve_state() {
    let installation = Installation::new();
    let token = installation.token();
    for args in [
        vec!["reset"],
        vec!["reset", "--typo"],
        vec!["reset", "--yes", "extra"],
    ] {
        assert!(!installation.cli(&args).status.success());
        assert_eq!(token, installation.token());
    }
    assert!(installation.cli(&["reset", "--help"]).status.success());
    assert!(
        !installation
            .command()
            .arg("reset")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(token, installation.token());
}

#[test]
fn a_live_api_lock_prevents_erasure_and_keeps_the_same_lock_file() {
    let installation = Installation::new();
    let data = installation.home.join("data");
    let url = DatabaseUrl::new(format!("sqlite://{}", data.join("else.sqlite3").display()));
    let held = else_api::runtime_lock::acquire(&data, &url).unwrap();
    let token = installation.token();
    let result = installation
        .command()
        .args(["reset", "--yes"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(token, installation.token());
    drop(held);
    let inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(data.join("api.lock")).unwrap());
    installation.success(&["reset", "--yes"]);
    assert_eq!(
        inode,
        std::os::unix::fs::MetadataExt::ino(&fs::metadata(data.join("api.lock")).unwrap())
    );
}

#[test]
fn external_storage_and_symlinks_are_refused_before_erasure() {
    let installation = Installation::new();
    let token = installation.token();
    let outside = installation._root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), "unrelated files").unwrap();
    for variable in [
        "ELSE_DATA_DIR",
        "ELSE_BOTS_DIR",
        "ELSE_BACKUP_DIR",
        "OWNER_TOKEN_FILE",
        "SECRET_KEY_FILE",
        "DATABASE_URL",
    ] {
        let value = if variable == "DATABASE_URL" {
            format!("sqlite://{}", outside.join("other.sqlite3").display())
        } else {
            outside.display().to_string()
        };
        let result = installation
            .command()
            .env(variable, value)
            .args(["reset", "--yes"])
            .output()
            .unwrap();
        assert!(!result.status.success(), "accepted {variable}");
        assert_eq!(token, installation.token());
    }
    for name in ["bots", "logs", "run"] {
        fs::remove_dir(installation.home.join(name)).unwrap();
        symlink(&outside, installation.home.join(name)).unwrap();
        assert!(
            !installation
                .command()
                .args(["reset", "--yes"])
                .output()
                .unwrap()
                .status
                .success()
        );
        fs::remove_file(installation.home.join(name)).unwrap();
        fs::create_dir(installation.home.join(name)).unwrap();
    }
    for alias in [
        outside.join("alias.sqlite3"),
        installation.home.join("data/alias.sqlite3"),
    ] {
        symlink(installation.home.join("data/else.sqlite3"), &alias).unwrap();
        let result = installation
            .command()
            .env("DATABASE_URL", format!("sqlite://{}", alias.display()))
            .args(["reset", "--yes"])
            .output()
            .unwrap();
        assert!(
            !result.status.success(),
            "accepted database alias {}",
            alias.display()
        );
        assert!(fs::symlink_metadata(alias).unwrap().is_symlink());
        assert_eq!(token, installation.token());
    }
    assert_eq!(token, installation.token());
    assert_eq!(
        fs::read_to_string(outside.join("keep")).unwrap(),
        "unrelated files"
    );
}

#[test]
fn reset_stops_running_computers_before_deleting_their_disks() {
    let installation = Installation::new();
    let msb = installation.home.join("microsandbox/bin/msb");
    fs::create_dir_all(msb.parent().unwrap()).unwrap();
    fs::create_dir_all(installation.home.join("microsandbox/lib")).unwrap();
    fs::write(
        installation.home.join("microsandbox/lib/libkrunfw.so.1"),
        "fixture",
    )
    .unwrap();
    fs::create_dir_all(installation.home.join("data/microsandbox")).unwrap();
    let log = installation.home.join("vm-stops");
    fs::write(&msb, format!(r##"#!/bin/sh
case "$1" in
--version) echo 'msb 0.7.13' ;;
list) echo '[{{"name":"active","status":"Running"}},{{"name":"paused","status":"Paused"}},{{"name":"idle","status":"Stopped"}}]' ;;
stop) test -d "$MSB_HOME" || exit 1; echo "$2" >> '{}' ;;
esac
"##, log.display())).unwrap();
    fs::set_permissions(&msb, fs::Permissions::from_mode(0o755)).unwrap();
    let result = installation
        .command()
        .env("ELSE_MSB_BIN", &msb)
        .args(["reset", "--yes"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read_to_string(log).unwrap(), "active\npaused\n");
    assert!(!installation.home.join("data/microsandbox").exists());

    fs::create_dir_all(installation.home.join("data/microsandbox")).unwrap();
    fs::write(&msb, "#!/bin/sh\nexit 1\n").unwrap();
    let token = installation.token();
    assert!(
        !installation
            .command()
            .env("ELSE_MSB_BIN", &msb)
            .args(["reset", "--yes"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(token, installation.token());
    assert!(installation.home.join("data/microsandbox").exists());
}

#[test]
fn reset_removes_only_claude_sessions_for_its_own_bot_projects() {
    let installation = Installation::new();
    let bot = uuid::Uuid::new_v4();
    let project = installation.home.join("bots").join(bot.to_string());
    fs::create_dir(&project).unwrap();
    let claude = installation._root.path().join("claude-config");
    let sessions = else_api::providers::claude_home::project_sessions(&claude, &project);
    let unrelated = claude.join("projects/unrelated-project/session.jsonl");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
    fs::write(sessions.join("session.jsonl"), "old bot transcript").unwrap();
    fs::write(&unrelated, "keep my other work").unwrap();
    fs::write(claude.join(".credentials.json"), "keep my login").unwrap();
    let result = installation
        .command()
        .env("CLAUDE_CONFIG_DIR", &claude)
        .args(["reset", "--yes"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!sessions.exists());
    assert!(unrelated.exists());
    assert!(claude.join(".credentials.json").exists());
}

#[test]
fn reset_terminates_orphaned_provider_processes() {
    let installation = Installation::new();
    let host = else_api::providers::host::Host::new(
        &installation.home.join("data"),
        &installation.home.join("bots"),
    );
    let mut child = Command::new("sleep")
        .arg("30")
        .env(
            else_api::providers::host::MARKER_VAR,
            format!("{}:{}", host.marker_prefix(), uuid::Uuid::new_v4()),
        )
        .spawn()
        .unwrap();
    let result = installation
        .command()
        .args(["reset", "--yes"])
        .output()
        .unwrap();
    let status = child.try_wait().unwrap();
    if status.is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        status.is_some_and(|status| !status.success()),
        "the provider process survived reset"
    );
}

#[test]
fn remove_erases_the_installation_and_its_command_link_without_recreating_state() {
    let installation = Installation::new();
    installation.seed();
    let link = installation._root.path().join(".local/bin/elsebot");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(installation.home.join("bin/elsebot"), &link).unwrap();
    let unconfirmed = installation.cli(&["remove"]);
    assert!(!unconfirmed.status.success());
    assert!(installation.home.join("data/owner-token").exists());
    let result = installation.cli(&["remove", "--yes"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("else.bot removed."));
    assert!(!installation.home.exists());
    assert!(!link.is_symlink());
    assert!(Path::new(env!("CARGO_BIN_EXE_else-api")).exists());
}

#[test]
fn remove_state_preserves_the_installation_lock_and_does_not_provision_new_credentials() {
    let installation = Installation::new();
    installation.seed();
    let lock = installation.home.join("run/install.lock");
    fs::create_dir(&lock).unwrap();
    fs::write(lock.join("pid"), "123").unwrap();
    installation.success(&["remove-state", "--yes"]);
    assert_eq!(fs::read_to_string(lock.join("pid")).unwrap(), "123");
    assert!(!installation.home.join("data/owner-token").exists());
    assert!(!installation.home.join("data/secret-key").exists());
    assert!(!installation.home.join("data/else.sqlite3").exists());
    assert!(installation.home.join("current/bin/else-api").exists());
}

#[test]
fn remove_refuses_external_storage_and_keeps_the_software_for_retry() {
    let installation = Installation::new();
    let external = installation._root.path().join("external-data");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("keep"), "external data").unwrap();
    fs::write(
        installation.home.join("config/.env"),
        format!("ELSE_DATA_DIR='{}'\n", external.display()),
    )
    .unwrap();
    let result = installation.cli(&["remove", "--yes"]);
    assert!(!result.status.success());
    assert!(installation.home.join("bin/elsebot").exists());
    assert!(installation.home.join("data/owner-token").exists());
    assert!(external.join("keep").exists());
    assert!(!installation.home.join("run/install.lock").exists());
}

#[test]
fn remove_stops_its_running_server_before_deleting_the_installation() {
    let installation = Installation::new();
    let binary = installation.home.join("current/bin/else-api");
    fs::remove_file(&binary).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_else-api"), &binary).unwrap();
    let mut command = Command::new(&binary);
    installation.environment(&mut command);
    command.env("ELSE_API_BIND", "127.0.0.1:0").arg("serve");
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().unwrap();
    let pid = child.id();
    let reaper = std::thread::spawn(move || child.wait().unwrap());
    fs::write(installation.home.join("run/else-api.pid"), pid.to_string()).unwrap();
    for _ in 0..100 {
        if installation.home.join("data/api.json").exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let result = installation.cli(&["remove", "--yes"]);
    if !result.status.success() {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let status = reaper.join().unwrap();
    assert!(status.success(), "server exited with {status}");
    assert!(!installation.home.exists());
}
