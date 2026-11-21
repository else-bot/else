//! Codex on the host: its tools, the shared login, and connector events.

use super::*;

async fn installed_codex() -> Option<(PathBuf, PathBuf)> {
    let codex = codex_on_path().await?;
    let auth = codex_home::host_auth_file(
        std::env::var("CODEX_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )?;
    auth.is_file().then_some((codex, auth))
}

#[tokio::test]
async fn codex_runs_with_host_tools_and_the_shared_login() {
    let Some((codex, auth)) = installed_codex().await else {
        eprintln!("SKIPPED: codex CLI or its login is not installed on this host");
        return;
    };
    let fx = fixture();
    if !codex_supports_startup_config(&fx.host, &codex).await {
        return;
    }
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

async fn codex_on_path() -> Option<PathBuf> {
    let configured = std::env::var_os("ELSE_CODEX_BIN").map(PathBuf::from);
    else_api::providers::executable::executable(
        &Host::new(&std::env::temp_dir(), &std::env::temp_dir()),
        configured.as_deref(),
        "codex",
    )
    .await
    .unwrap_or_else(|reason| panic!("Codex CLI resolution failed: {reason}"))
    .map(|codex| codex.path().to_owned())
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
            (
                else_api::providers::cli::files::GRANT_ENV.into(),
                "tg_test".into(),
            ),
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
    let Some(codex) = codex_on_path().await else {
        eprintln!("SKIPPED: codex CLI is not installed on this host");
        return;
    };
    let fx = fixture();
    if !codex_supports_startup_config(&fx.host, &codex).await {
        return;
    }
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
                    common::SETTLE,
                )
                .await
                .unwrap();
            server.close().await.unwrap();
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
