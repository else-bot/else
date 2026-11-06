mod common;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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

#[cfg(target_os = "linux")]
#[test]
fn a_probe_waits_for_an_executable_writer_to_close() {
    let fx = fixture();
    let program = fx.data.join("provider");
    std::fs::write(&program, "#!/bin/sh\necho provider-version\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&program)
        .unwrap();
    let error = std::process::Command::new(&program).output().unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ETXTBSY));
    std::thread::scope(|scope| {
        scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            drop(writer);
        });
        assert_eq!(
            fx.host.probe(&program, &["--version"], &[]).as_deref(),
            Some("provider-version")
        );
    });
}

#[test]
fn a_probe_does_not_repeat_a_command_that_exits_with_failure() {
    let fx = fixture();
    let calls = fx.data.join("probe-calls");
    assert!(
        fx.host
            .probe(
                Path::new("/bin/sh"),
                &["-c", "echo called >> \"$CALLS\"; exit 1"],
                &[("CALLS".into(), calls.to_str().unwrap().into())],
            )
            .is_none()
    );
    assert_eq!(std::fs::read_to_string(calls).unwrap(), "called\n");
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
    assert_eq!(get("HOME"), std::env::var("HOME").ok().as_deref());
    assert_eq!(get("PATH"), std::env::var("PATH").ok().as_deref());
    assert_eq!(get("PROVIDER_SETTING"), Some("allowed"));
    assert_eq!(
        get(MARKER_VAR),
        Some(format!("{}:{}", fx.host.marker_prefix(), spec.turn_id).as_str())
    );
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
    let out = output(
        &fx,
        &launch(&["--strict-config", "app-server", "--print-startup-config"]),
    )
    .await;
    assert!(out.contains("\"appServerVersion\""), "{out}");
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

fn codex_on_path() -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    let codex = path
        .split(':')
        .map(|dir| Path::new(dir).join("codex"))
        .find(|p| p.is_file())?;
    std::fs::canonicalize(codex).ok()
}

type Recorded = Arc<Mutex<Vec<serde_json::Value>>>;

async fn mock_backend() -> (String, Recorded, Recorded) {
    use axum::response::IntoResponse;
    let requests = Recorded::default();
    let recorded = requests.clone();
    let mcp_calls = Recorded::default();
    let mcp_recorded = mcp_calls.clone();
    let responses = move |axum::Json(body): axum::Json<serde_json::Value>| {
        let recorded = recorded.clone();
        async move {
            recorded.lock().unwrap().push(body);
            let message = json!({ "id": "msg", "type": "message", "role": "assistant",
                "content": [{ "type": "output_text", "text": "ok", "annotations": [] }] });
            let events = [
                json!({ "type": "response.created", "response":
                    { "id": "resp", "object": "response", "status": "in_progress", "output": [] } }),
                json!({ "type": "response.output_item.done", "output_index": 0, "item": message }),
                json!({ "type": "response.completed", "response":
                    { "id": "resp", "object": "response", "status": "completed", "output": [message] } }),
            ];
            let body: String = events
                .iter()
                .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
                .collect();
            ([("content-type", "text/event-stream")], body)
        }
    };
    let mcp = move |headers: axum::http::HeaderMap,
                    axum::Json(rpc): axum::Json<serde_json::Value>| {
        mcp_recorded.lock().unwrap().push(json!({
            "method": rpc["method"],
            "authorization": headers.get("authorization").and_then(|v| v.to_str().ok()),
        }));
        async move {
            let Some(id) = rpc.get("id").cloned() else {
                return StatusCode::ACCEPTED.into_response();
            };
            let result = match rpc["method"].as_str() {
                Some("initialize") => json!({
                    "protocolVersion": rpc["params"]["protocolVersion"],
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "elsebot", "version": "0" },
                }),
                Some("tools/list") => json!({ "tools": else_api::app::tools::TOOL_NAMES
                .iter()
                .map(|name| json!({ "name": name, "description": name,
                    "inputSchema": { "type": "object", "properties": {} } }))
                .collect::<Vec<_>>() }),
                _ => json!({}),
            };
            axum::Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
        }
    };
    let app = axum::Router::new()
        .route("/v1/responses", axum::routing::post(responses))
        .route(
            "/mcp",
            axum::routing::post(mcp).get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, requests, mcp_calls)
}

fn offered_tools(request: &serde_json::Value) -> Vec<String> {
    fn walk(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                if let (Some(serde_json::Value::Array(tools)), true) =
                    (map.get("tools"), map.contains_key("tools"))
                {
                    for tool in tools {
                        if let Some(name) = tool["name"].as_str() {
                            out.push(name.to_owned());
                        }
                    }
                }
                if let Some(text) = map.get("description").and_then(|d| d.as_str()) {
                    for declared in text.split("declare const tools: { ").skip(1) {
                        let name: String = declared
                            .chars()
                            .take_while(|c| c.is_alphanumeric() || *c == '_')
                            .collect();
                        out.push(name);
                    }
                }
                map.values().for_each(|v| walk(v, out));
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(request, &mut out);
    out.sort();
    out.dedup();
    out
}

async fn first_model_request(
    fx: &Fixture,
    codex: &Path,
    host_tools: bool,
    no_native: bool,
) -> (Vec<String>, Vec<serde_json::Value>) {
    let (url, requests, mcp_calls) = mock_backend().await;
    let dirs = fx.host.bot_dirs(Uuid::new_v4(), "codex").unwrap();
    let login = fx.data.join("no-login.json");
    codex_home::prepare(&dirs, &login, Some(&format!("{url}/mcp")), host_tools).unwrap();
    let mut launch = LaunchSpec {
        turn_id: Uuid::new_v4(),
        dirs: dirs.clone(),
        program: codex.to_owned(),
        args: Vec::new(),
        env: vec![
            (
                "CODEX_HOME".into(),
                dirs.provider_home.to_string_lossy().into_owned(),
            ),
            (codex_home::GRANT_ENV.into(), "tg_test".into()),
        ],
    };
    if no_native {
        launch.args = codex_home::no_native_tools(&fx.host, &launch, "gpt-6.1-sol")
            .await
            .unwrap();
    }
    let provider = format!(
        "model_providers.mock={{ name = \"Mock\", base_url = \"{url}/v1\", \
         wire_api = \"responses\", requires_openai_auth = false }}"
    );
    for arg in [
        "-c",
        "model_provider=\"mock\"",
        "-c",
        &provider,
        "exec",
        "--json",
        "--skip-git-repo-check",
        "--sandbox",
        codex_home::sandbox_mode(host_tools),
        "-m",
        "gpt-6.1-sol",
        "Reply ok.",
    ] {
        launch.args.push(arg.into());
    }
    let out = tokio::time::timeout(
        Duration::from_secs(90),
        fx.host
            .command(&launch)
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .expect("codex exec finished")
    .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let requests = requests.lock().unwrap();
    assert!(!requests.is_empty(), "no model request was sent: {stderr}");
    let calls = mcp_calls.lock().unwrap().clone();
    (offered_tools(&requests[0]), calls)
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_offers_a_connector_event_no_native_tools() {
    let Some(codex) = codex_on_path() else {
        eprintln!("SKIPPED: codex CLI is not installed on this host");
        return;
    };
    let fx = fixture();
    let dirs = fx.host.bot_dirs(Uuid::new_v4(), "codex").unwrap();
    let login = fx.data.join("no-login.json");
    codex_home::prepare(
        &dirs,
        &login,
        Some("http://127.0.0.1:1/tools/v1/mcp"),
        false,
    )
    .unwrap();
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
    let configuration = output(
        &fx,
        &launch(&["--strict-config", "app-server", "--print-startup-config"]),
    )
    .await;
    assert!(
        configuration.contains("appServerVersion"),
        "{configuration}"
    );
    let out = output(&fx, &launch(&["features", "list"])).await;
    let state = |feature: &str| {
        out.lines()
            .find(|line| line.split_whitespace().next() == Some(feature))
            .unwrap_or_else(|| panic!("feature {feature} missing from `codex features list`"))
            .trim_end()
            .to_owned()
    };
    for feature in codex_home::DISABLED_FEATURES.iter().chain(&["shell_tool"]) {
        assert!(state(feature).ends_with("false"), "{}", state(feature));
    }
    assert!(
        state("code_mode_host").ends_with("true"),
        "{}",
        state("code_mode_host")
    );

    let listed = |args: Vec<OsString>| {
        let mut spec = launch(&[]);
        spec.args = args;
        spec.args.push("app-server".into());
        let host = fx.host.clone();
        async move {
            let server = else_api::providers::codex_rpc::AppServer::start(&host, &spec)
                .await
                .unwrap();
            let listed = server
                .request(
                    "model/list",
                    json!({ "includeHidden": true }),
                    Duration::from_secs(30),
                )
                .await
                .unwrap();
            server.shutdown(Duration::from_secs(3)).await;
            listed["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["model"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    let catalog = codex_home::no_native_tools(&fx.host, &launch(&[]), "gpt-6.1-sol")
        .await
        .unwrap();
    assert_eq!(listed(catalog).await, ["gpt-6.1-sol"]);
    assert!(listed(Vec::new()).await.len() > 1);

    let (event, mcp) = first_model_request(&fx, &codex, false, true).await;
    let (unsanitized, _) = first_model_request(&fx, &codex, false, false).await;
    let (owner, _) = first_model_request(&fx, &codex, true, false).await;
    let has = |tools: &[String], name: &str| tools.iter().any(|t| t == name);
    for native in [
        "apply_patch",
        "exec_command",
        "write_stdin",
        "shell_command",
        "request_user_input",
        "update_plan",
    ] {
        assert!(!has(&event, native), "{native} offered: {event:?}");
    }
    assert!(has(&event, "exec"), "code mode missing: {event:?}");
    assert!(
        mcp.contains(&json!({ "method": "tools/list", "authorization": "Bearer tg_test" })),
        "else.bot's MCP tools were not listed with the grant: {mcp:?}"
    );
    for mail in ["mail_accounts", "mail_action", "mail_action_status"] {
        assert!(
            event.iter().any(|name| name.ends_with(mail)),
            "{mail} is not declared: {event:?}"
        );
    }
    assert!(has(&unsanitized, "apply_patch"), "{unsanitized:?}");
    for tool in ["apply_patch", "exec_command", "exec"] {
        assert!(has(&owner, tool), "{tool} missing: {owner:?}");
    }
}

#[tokio::test]
async fn bounded_output_stops_long_or_large_runs() {
    let fx = fixture();
    let bot = Uuid::new_v4();
    let run = |script: &str, timeout_ms: u64, limit: usize| {
        let spec = shell(&fx, bot, script);
        let host = fx.host.clone();
        async move {
            host.bounded_output(&spec, Duration::from_millis(timeout_ms), limit)
                .await
        }
    };
    assert_eq!(run("printf ok", 5000, 2).await, Ok(b"ok".to_vec()));
    assert!(run("printf too-long", 5000, 2).await.is_err());
    assert!(run("yes", 5000, 1 << 16).await.is_err());
    assert!(run("printf ok; exit 3", 5000, 16).await.is_err());
    let started = std::time::Instant::now();
    assert!(run("sleep 30", 300, 16).await.is_err());
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancelling_metadata_export_kills_descendants_after_the_leader_exits() {
    let fx = fixture();
    let pid_file = fx.data.join("export-child.pid");
    let mut spec = shell(
        &fx,
        Uuid::new_v4(),
        "sleep 30 & printf '%s %s' \"$$\" \"$!\" > \"$CHILD_PID_FILE\"; exit 0",
    );
    spec.env
        .push(("CHILD_PID_FILE".into(), pid_file.to_str().unwrap().into()));
    let host = fx.host.clone();
    let exporting = tokio::spawn(async move {
        host.bounded_output(&spec, Duration::from_secs(30), 1024)
            .await
    });
    let (leader, child) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_file)
                && let Ok(pids) = pid
                    .split_whitespace()
                    .map(str::parse::<u32>)
                    .collect::<Result<Vec<_>, _>>()
                && pids.len() == 2
            {
                break (pids[0], pids[1]);
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = std::fs::read_to_string(format!("/proc/{leader}/stat")).unwrap();
            if state
                .split_once(") ")
                .is_some_and(|(_, s)| s.starts_with('Z'))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("exporter leader did not exit while descendant held stdout");
    let alive = std::fs::read_to_string(format!("/proc/{child}/stat")).unwrap();
    assert!(!alive.split_once(") ").unwrap().1.starts_with('Z'));
    exporting.abort();
    assert!(exporting.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match std::fs::read_to_string(format!("/proc/{child}/stat")) {
                Err(_) => break,
                Ok(state)
                    if state
                        .split_once(") ")
                        .is_some_and(|(_, s)| s.starts_with('Z')) =>
                {
                    break;
                }
                _ => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .expect("cancelled exporter left a descendant running");
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
