//! POC-010: provider CLIs on the host, and the task-grant tool boundary.
//!
//! The launch tests run a shell the way providers run. The Codex check uses
//! the installed CLI and the owner's existing login without running
//! inference. Process discovery runs on Linux and macOS.

mod common;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::Duration;

use axum::http::StatusCode;
use common::TestApp;
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::codex_home;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use else_api::providers::host::Supervised;
use else_api::providers::host::{Host, LaunchSpec, MARKER_VAR};
use serde_json::json;
use uuid::Uuid;

struct Fixture {
    _dir: tempfile::TempDir,
    data: PathBuf,
    bots: PathBuf,
    host: Host,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let data = root.join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let bots = root.join("bots");
    let host = Host::new(&data, &bots);
    Fixture {
        _dir: dir,
        data,
        bots,
        host,
    }
}

fn shell(fx: &Fixture, bot: Uuid, script: &str) -> LaunchSpec {
    LaunchSpec {
        turn_id: Uuid::new_v4(),
        dirs: fx.host.bot_dirs(bot, "test").unwrap(),
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
        env: vec![("PROVIDER_SETTING".into(), "allowed".into())],
    }
}

async fn output(fx: &Fixture, spec: &LaunchSpec) -> String {
    let out = fx.host.command(spec).output().await.unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test]
async fn runs_start_in_the_project_with_the_users_session_environment() {
    let fx = fixture();
    let bot = Uuid::new_v4();
    let spec = shell(&fx, bot, "pwd; echo ---; env");
    let dirs = &spec.dirs;
    assert_eq!(dirs.project, fx.bots.join(bot.to_string()));
    assert_eq!(fx.host.project_path(bot), dirs.project);
    assert!(dirs.provider_home.starts_with(fx.data.join("bots")));
    for dir in [&dirs.project, &dirs.provider_home] {
        let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", dir.display());
    }
    let out = output(&fx, &spec).await;
    let (cwd, env) = out.split_once("---").unwrap();
    assert_eq!(
        Path::new(cwd.trim()).canonicalize().unwrap(),
        dirs.project.canonicalize().unwrap(),
        "the run starts in the bot's project"
    );
    let vars: Vec<(&str, &str)> = env.lines().filter_map(|l| l.split_once('=')).collect();
    let get = |name: &str| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| *v);
    // The owner's session: host work sees their home and tools.
    assert_eq!(get("HOME"), std::env::var("HOME").ok().as_deref());
    assert_eq!(get("PATH"), std::env::var("PATH").ok().as_deref());
    assert_eq!(get("PROVIDER_SETTING"), Some("allowed"));
    assert_eq!(
        get(MARKER_VAR),
        Some(format!("{}:{}", fx.host.marker_prefix(), spec.turn_id).as_str())
    );
    // Nothing of the API's own environment (cargo's, here) passes.
    let session = [
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "PATH",
        "LANG",
        "LANGUAGE",
        "TZ",
        "TMPDIR",
        "TERM",
        "XDG_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "DBUS_SESSION_BUS_ADDRESS",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "SSH_AUTH_SOCK",
    ];
    let shell_own = ["PWD", "OLDPWD", "SHLVL", "_"];
    for (key, _) in &vars {
        assert!(
            session.contains(key)
                || shell_own.contains(key)
                || key.starts_with("LC_")
                || *key == "PROVIDER_SETTING"
                || *key == MARKER_VAR,
            "{key} leaked into the provider's environment"
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn termination_reaps_the_whole_process_group() {
    let fx = fixture();
    let spec = shell(
        &fx,
        Uuid::new_v4(),
        "sleep 300 & sleep 300 & sh -c 'sleep 300' & wait",
    );
    let mut process = Supervised::spawn(fx.host.command(&spec)).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        fx.host.live_processes(spec.turn_id).len() >= 4,
        "descendants are discoverable"
    );
    process.terminate(Duration::from_secs(2)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fx.host.live_processes(spec.turn_id), Vec::<i32>::new());
}

/// A run whose supervisor vanished (as after an API crash) is found and
/// terminated by the startup sweep.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn orphans_are_swept() {
    let fx = fixture();
    let spec = shell(&fx, Uuid::new_v4(), "sleep 300 & wait");
    let process = Supervised::spawn(fx.host.command(&spec)).unwrap();
    std::mem::forget(process);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!fx.host.live_processes(spec.turn_id).is_empty());
    assert!(fx.host.sweep_orphans().unwrap() >= 1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(fx.host.live_processes(spec.turn_id).is_empty());
}

fn installed_codex() -> Option<(PathBuf, PathBuf)> {
    let path = std::env::var("PATH").ok()?;
    let codex = path
        .split(':')
        .map(|dir| Path::new(dir).join("codex"))
        .find(|p| p.is_file())?;
    let codex = std::fs::canonicalize(codex).ok()?;
    let auth = codex_home::host_auth_file(
        std::env::var("CODEX_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )?;
    auth.is_file().then_some((codex, auth))
}

#[tokio::test]
async fn codex_runs_with_host_tools_and_the_shared_login() {
    let Some((codex, auth)) = installed_codex() else {
        eprintln!("SKIPPED: codex CLI or its login is not installed on this host");
        return;
    };
    let fx = fixture();
    let dirs = fx.host.bot_dirs(Uuid::new_v4(), "codex").unwrap();
    codex_home::prepare(&dirs, &auth, None, true).unwrap();
    let launch = |args: &[&str]| LaunchSpec {
        turn_id: Uuid::new_v4(),
        dirs: dirs.clone(),
        program: codex.clone(),
        args: args.iter().map(OsString::from).collect(),
        env: vec![(
            "CODEX_HOME".into(),
            dirs.provider_home.to_string_lossy().into_owned(),
        )],
    };
    // The configuration is valid for the installed version.
    let out = output(
        &fx,
        &launch(&["--strict-config", "app-server", "--print-startup-config"]),
    )
    .await;
    assert!(out.contains("\"appServerVersion\""), "{out}");
    // Native sub-agents and other tool families are off; the shell is on.
    let out = output(&fx, &launch(&["features", "list"])).await;
    let state = |feature: &str| {
        out.lines()
            .find(|line| line.split_whitespace().next() == Some(feature))
            .unwrap_or_else(|| panic!("feature {feature} missing from `codex features list`"))
            .trim_end()
            .to_owned()
    };
    for feature in codex_home::DISABLED_FEATURES {
        assert!(state(feature).ends_with("false"), "{}", state(feature));
    }
    assert!(
        state("shell_tool").ends_with("true"),
        "{}",
        state("shell_tool")
    );
    // The supported login works without copying credentials.
    let status = fx
        .host
        .command(&launch(&["login", "status"]))
        .output()
        .await
        .unwrap();
    let out = String::from_utf8_lossy(&status.stdout).into_owned()
        + &String::from_utf8_lossy(&status.stderr);
    assert!(status.status.success(), "{out}");
    assert!(out.to_lowercase().contains("logged in"), "{out}");
    assert_eq!(
        std::fs::read_link(dirs.provider_home.join("auth.json")).unwrap(),
        auth,
        "the bot's home links the login, never a copy"
    );
}

async fn hanging_turn(app: &TestApp, bot: &str) -> (Uuid, Uuid) {
    let reply = app
        .send(bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn: Uuid = reply.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&turn.to_string(), &["running"]).await;
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    (turn, conversation)
}

#[tokio::test(flavor = "multi_thread")]
async fn task_grants_are_scoped_to_their_turn() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let other = app.create_bot("Other").await;
    let foreign = app
        .send(&other, Uuid::new_v4(), "foreign secret", Some("synthetic"))
        .await;
    app.wait_turn(foreign.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let foreign_item = foreign.body["item"]["id"].as_str().unwrap().to_owned();

    let (turn, conversation) = hanging_turn(&app, &bot).await;
    let grants = app.scheduler.grants();
    let token = grants
        .issue(
            bot.parse().unwrap(),
            conversation,
            turn,
            vec![Tool::ReadHistory],
            DEFAULT_TTL,
        )
        .await;

    let history = tool(&app, "/tools/v1/history", Some(&token)).await;
    assert_eq!(history.status, StatusCode::OK);
    let texts: Vec<_> = history.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["text"].clone())
        .collect();
    assert_eq!(
        texts,
        vec![json!("[synthetic:hang]")],
        "only its own conversation"
    );
    // Identity cannot be chosen by the request.
    let forged = tool(
        &app,
        &format!("/tools/v1/history?bot_id={other}"),
        Some(&token),
    )
    .await;
    assert_eq!(forged.status, StatusCode::BAD_REQUEST);
    let item = tool(
        &app,
        &format!("/tools/v1/items/{foreign_item}"),
        Some(&token),
    )
    .await;
    assert_eq!(
        item.status,
        StatusCode::NOT_FOUND,
        "foreign objects look missing"
    );
    // Grants and owner tokens never cross route groups.
    let owner = app.call("GET", "/v1/bots", None, Some(&token)).await;
    assert_eq!(
        (owner.status, owner.body["error"]["code"].clone()),
        (StatusCode::FORBIDDEN, json!("task_grant_forbidden"))
    );
    let create = app
        .call(
            "POST",
            "/v1/bots",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "x" })),
            Some(&token),
        )
        .await;
    assert_eq!(create.status, StatusCode::FORBIDDEN);
    let settings = app
        .call(
            "PATCH",
            "/v1/settings/providers/codex",
            Some(json!({ "reasoning_effort": "high" })),
            Some(&token),
        )
        .await;
    assert_eq!(
        settings.status,
        StatusCode::FORBIDDEN,
        "task grants cannot update settings"
    );
    let with_owner = tool(&app, "/tools/v1/history", Some(common::TOKEN)).await;
    assert_eq!(with_owner.status, StatusCode::FORBIDDEN);
    assert_eq!(
        tool(&app, "/tools/v1/history", Some("tg_forged"))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        tool(&app, "/tools/v1/history", None).await.status,
        StatusCode::UNAUTHORIZED
    );

    // A grant without the tool is refused.
    let narrow = grants
        .issue(
            bot.parse().unwrap(),
            conversation,
            turn,
            vec![],
            DEFAULT_TTL,
        )
        .await;
    assert_eq!(
        tool(&app, "/tools/v1/history", Some(&narrow)).await.status,
        StatusCode::FORBIDDEN
    );

    // Stop work revokes the grant.
    let stop_request = Uuid::new_v4();
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/cancel"),
        Some(json!({ "request_id": stop_request })),
    )
    .await;
    assert_eq!(
        tool(&app, "/tools/v1/history", Some(&token)).await.status,
        StatusCode::UNAUTHORIZED
    );

    // A grant for a turn that is no longer running is refused even if it
    // was never revoked in memory (for example, after reconciliation).
    app.wait_turn(&turn.to_string(), &["interrupted"]).await;
    let stale = grants
        .issue(
            bot.parse().unwrap(),
            conversation,
            turn,
            vec![Tool::ReadHistory],
            DEFAULT_TTL,
        )
        .await;
    assert_eq!(
        tool(&app, "/tools/v1/history", Some(&stale)).await.status,
        StatusCode::UNAUTHORIZED
    );

    // Retrying the completed stop leaves a newer turn's grant alone.
    let (newer, conversation) = hanging_turn(&app, &bot).await;
    let newer_grant = grants
        .issue(
            bot.parse().unwrap(),
            conversation,
            newer,
            vec![Tool::ReadHistory],
            DEFAULT_TTL,
        )
        .await;
    let retry = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": stop_request })),
        )
        .await;
    assert_eq!(retry.body["state"], "done");
    assert_eq!(
        tool(&app, "/tools/v1/history", Some(&newer_grant))
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_bot_revokes_its_grants() {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Temp").await;
    let (turn, conversation) = hanging_turn(&app, &bot).await;
    let token = app
        .scheduler
        .grants()
        .issue(
            bot.parse().unwrap(),
            conversation,
            turn,
            vec![Tool::ReadHistory],
            DEFAULT_TTL,
        )
        .await;
    assert_eq!(
        app.call("GET", "/tools/v1/history", None, Some(&token))
            .await
            .status,
        StatusCode::OK
    );
    app.owner("DELETE", &format!("/v1/bots/{bot}"), None).await;
    assert_eq!(
        app.call("GET", "/tools/v1/history", None, Some(&token))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

async fn tool(app: &TestApp, path: &str, token: Option<&str>) -> common::Reply {
    app.call("GET", path, None, token).await
}

/// Environment values given to a provider (such as its task grant) reach it
/// but never appear on any host command line, which other users can read.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn launch_secrets_stay_off_host_command_lines() {
    let fx = fixture();
    let mut spec = shell(&fx, Uuid::new_v4(), "echo \"$GRANT_CANARY\"; read release");
    spec.env
        .push(("GRANT_CANARY".into(), "tg_canary_secret".into()));
    let mut command = fx.host.command(&spec);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    let mut process = Supervised::spawn(command).unwrap();
    let mut stdout = tokio::io::BufReader::new(process.child.stdout.take().unwrap());
    let mut seen = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::io::AsyncBufReadExt::read_line(&mut stdout, &mut seen),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!fx.host.live_processes(spec.turn_id).is_empty());
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        if let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) {
            let text = String::from_utf8_lossy(&cmdline);
            assert!(!text.contains("tg_canary_secret"), "{text}");
        }
    }
    assert_eq!(
        seen.trim(),
        "tg_canary_secret",
        "the value reached the provider"
    );
}
