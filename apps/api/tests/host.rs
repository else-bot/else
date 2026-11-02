//! POC-010: restricted host execution and the task-grant tool boundary.
//!
//! The sandbox tests use the real bubblewrap. The Codex checks use the
//! installed CLI and the owner's existing login without running inference.

mod common;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::http::StatusCode;
use common::TestApp;
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::codex_home;
use else_api::providers::host::{HostSandbox, LaunchSpec, Supervised};
use serde_json::json;
use uuid::Uuid;

struct Fixture {
    _dir: tempfile::TempDir,
    data: PathBuf,
    sandbox: HostSandbox,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(data.join("owner-token"), "OWNER-TOKEN-CANARY").unwrap();
    std::fs::write(data.join("secret-key"), "SECRET-KEY-CANARY").unwrap();
    let sandbox = HostSandbox::detect(&data).expect("bubblewrap must work on this host");
    Fixture {
        _dir: dir,
        data,
        sandbox,
    }
}

fn shell(fx: &Fixture, bot: Uuid, script: &str) -> LaunchSpec {
    LaunchSpec {
        turn_id: Uuid::new_v4(),
        dirs: fx.sandbox.bot_dirs(bot, "test").unwrap(),
        program: PathBuf::from("/usr/bin/sh"),
        args: vec!["-c".into(), script.into()],
        env: vec![("PROVIDER_SETTING".into(), "allowed".into())],
        extra_binds: vec![],
    }
}

async fn output(fx: &Fixture, spec: &LaunchSpec) -> String {
    let out = fx.sandbox.command(spec).output().await.unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test]
async fn sandbox_environment_and_files_are_restricted() {
    let fx = fixture();
    let bot = Uuid::new_v4();
    let sibling = Uuid::new_v4();
    let sibling_dirs = fx.sandbox.bot_dirs(sibling, "test").unwrap();
    std::fs::write(sibling_dirs.work.join("sibling.txt"), "SIBLING-CANARY").unwrap();
    let home = std::env::var("HOME").unwrap();
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let own = shell(&fx, bot, "true");
    std::fs::write(own.dirs.work.join("own.txt"), "OWN-OK").unwrap();
    let data = fx.data.display().to_string();
    let script = format!(
        "env; echo ---; cat own.txt; echo; \
         for f in '{data}/owner-token' '{data}/secret-key' '{sib}/sibling.txt' \
                  '{home}/.codex/auth.json' '{home}/.ssh' '/run/user/{uid}/bus' \
                  '/var/run/docker.sock' '/run/docker.sock' '/run' '/etc/os-release' \
                  '/etc/codex' '/etc/machine-id'; do \
           if [ -e \"$f\" ]; then echo \"VISIBLE $f\"; else echo \"hidden $f\"; fi; done; \
         if [ -e /proc/{host_pid} ]; then echo \"VISIBLE host pid\"; fi; \
         echo procs=$(ls /proc | grep -c '^[0-9]')",
        sib = sibling_dirs.work.display(),
        host_pid = std::process::id(),
    );
    let mut spec = shell(&fx, bot, &script);
    spec.dirs = own.dirs.clone();
    let out = output(&fx, &spec).await;
    let (env, rest) = out.split_once("---").unwrap();
    let mut keys: Vec<&str> = env
        .lines()
        .filter_map(|line| line.split_once('=').map(|(k, _)| k))
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "ELSE_BOT_SANDBOX",
            "HOME",
            "LANG",
            "PATH",
            "PROVIDER_SETTING",
            "PWD",
            "SHLVL",
            "TMPDIR",
            "_", // set by the shell itself
        ],
        "only the allowlist reaches the provider"
    );
    for canary in [
        "OWNER-TOKEN-CANARY",
        "SECRET-KEY-CANARY",
        "SIBLING-CANARY",
        "DATABASE_URL",
        "SECRET_KEY=",
    ] {
        assert!(!out.contains(canary), "{canary} leaked");
    }
    assert!(
        rest.contains("OWN-OK"),
        "the bot's own work directory is usable"
    );
    assert!(!rest.contains("VISIBLE"), "{rest}");
    let procs: usize = rest
        .trim()
        .rsplit_once("procs=")
        .unwrap()
        .1
        .parse()
        .unwrap();
    assert!(
        procs <= 8,
        "only the sandbox's own processes are visible ({procs})"
    );
}

#[tokio::test]
async fn termination_reaps_the_whole_sandbox() {
    let fx = fixture();
    let spec = shell(
        &fx,
        Uuid::new_v4(),
        "sleep 300 & sleep 300 & sh -c 'sleep 300' & wait",
    );
    let mut process = Supervised::spawn(fx.sandbox.command(&spec)).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        fx.sandbox.live_processes(spec.turn_id).len() >= 4,
        "descendants are discoverable"
    );
    process.terminate(Duration::from_secs(2)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fx.sandbox.live_processes(spec.turn_id), Vec::<i32>::new());
}

#[tokio::test]
async fn sandbox_dies_with_its_parent_and_orphans_are_swept() {
    let fx = fixture();
    // The parent (a shell) is killed: bubblewrap takes the sandbox down.
    let spec = shell(&fx, Uuid::new_v4(), "sleep 300 & wait");
    let mut args: Vec<OsString> = vec![
        "-c".into(),
        "\"$0\" \"$@\" & sleep 0.5; kill -9 $$".into(),
        "/usr/bin/bwrap".into(),
    ];
    args.extend(fx.sandbox.args(&spec));
    let status = tokio::process::Command::new("/usr/bin/sh")
        .args(&args)
        .env_clear()
        .env(
            "ELSE_BOT_SANDBOX",
            format!("{}:{}", fx.sandbox.marker_prefix(), spec.turn_id),
        )
        .status()
        .await
        .unwrap();
    assert!(!status.success());
    for _ in 0..40 {
        if fx.sandbox.live_processes(spec.turn_id).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        fx.sandbox.live_processes(spec.turn_id).is_empty(),
        "no orphan after parent death"
    );

    // A sandbox whose supervisor vanished (as after an API crash that
    // bubblewrap did not see) is found and terminated by the startup sweep.
    let spec = shell(&fx, Uuid::new_v4(), "sleep 300 & wait");
    let process = Supervised::spawn(fx.sandbox.command(&spec)).unwrap();
    std::mem::forget(process);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!fx.sandbox.live_processes(spec.turn_id).is_empty());
    assert!(fx.sandbox.sweep_orphans().unwrap() >= 1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(fx.sandbox.live_processes(spec.turn_id).is_empty());
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
async fn codex_runs_locked_down_with_the_shared_login() {
    let Some((codex, auth)) = installed_codex() else {
        eprintln!("SKIPPED: codex CLI or its login is not installed on this host");
        return;
    };
    let fx = fixture();
    let dirs = fx.sandbox.bot_dirs(Uuid::new_v4(), "codex").unwrap();
    let login = codex_home::prepare(&dirs, &auth).unwrap();
    let launch = |args: &[&str]| LaunchSpec {
        turn_id: Uuid::new_v4(),
        dirs: dirs.clone(),
        program: codex.clone(),
        args: args.iter().map(OsString::from).collect(),
        env: vec![("CODEX_HOME".into(), "/var/lib/provider-home".into())],
        extra_binds: vec![login.clone()],
    };
    // The locked configuration is valid for the installed version.
    let out = output(
        &fx,
        &launch(&["--strict-config", "app-server", "--print-startup-config"]),
    )
    .await;
    assert!(out.contains("\"appServerVersion\""), "{out}");
    // Native sub-agents and tool families are off.
    let out = output(&fx, &launch(&["features", "list"])).await;
    for feature in codex_home::DISABLED_FEATURES {
        let line = out
            .lines()
            .find(|line| line.split_whitespace().next() == Some(feature))
            .unwrap_or_else(|| panic!("feature {feature} missing from `codex features list`"));
        assert!(line.trim_end().ends_with("false"), "{line}");
    }
    // The supported login works without copying credentials.
    let status = fx
        .sandbox
        .command(&launch(&["login", "status"]))
        .output()
        .await
        .unwrap();
    let out = String::from_utf8_lossy(&status.stdout).into_owned()
        + &String::from_utf8_lossy(&status.stderr);
    assert!(status.status.success(), "{out}");
    assert!(out.to_lowercase().contains("logged in"), "{out}");
    let placeholder = std::fs::metadata(dirs.provider_home.join("auth.json")).unwrap();
    assert_eq!(
        placeholder.len(),
        0,
        "the bot's home holds a mount point, not a copy"
    );
}

async fn hanging_turn(app: &TestApp, bot: &str) -> (Uuid, Uuid) {
    let reply = app
        .send(bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn: Uuid = reply.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&turn.to_string(), &["running"]).await;
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = $1")
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
    let other = app.create_bot("Other", "other").await;
    let foreign = app
        .send(&other, Uuid::new_v4(), "foreign secret", Some("synthetic"))
        .await;
    app.wait_turn(foreign.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let foreign_item = foreign.body["item"]["id"].as_str().unwrap().to_owned();

    let (turn, conversation) = hanging_turn(&app, &bot).await;
    let grants = app.scheduler.grants();
    let token = grants.issue(
        bot.parse().unwrap(),
        conversation,
        turn,
        vec![Tool::ReadHistory],
        DEFAULT_TTL,
    );

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
            Some(json!({ "request_id": Uuid::new_v4(), "name": "x", "role": "y" })),
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
    let narrow = grants.issue(
        bot.parse().unwrap(),
        conversation,
        turn,
        vec![],
        DEFAULT_TTL,
    );
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
    let stale = grants.issue(
        bot.parse().unwrap(),
        conversation,
        turn,
        vec![Tool::ReadHistory],
        DEFAULT_TTL,
    );
    assert_eq!(
        tool(&app, "/tools/v1/history", Some(&stale)).await.status,
        StatusCode::UNAUTHORIZED
    );

    // Retrying the completed stop leaves a newer turn's grant alone.
    let (newer, conversation) = hanging_turn(&app, &bot).await;
    let newer_grant = grants.issue(
        bot.parse().unwrap(),
        conversation,
        newer,
        vec![Tool::ReadHistory],
        DEFAULT_TTL,
    );
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
    let bot = app.create_bot("Temp", "temp").await;
    let (turn, conversation) = hanging_turn(&app, &bot).await;
    let token = app.scheduler.grants().issue(
        bot.parse().unwrap(),
        conversation,
        turn,
        vec![Tool::ReadHistory],
        DEFAULT_TTL,
    );
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
