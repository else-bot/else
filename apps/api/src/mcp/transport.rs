use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use rmcp::model::{ClientConfig, ClientRequest, ProtocolVersion};
use rmcp::service::RunningService;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, InsufficientScopeError, StreamableHttpClientTransport,
    StreamableHttpClientTransportConfig, StreamableHttpError,
};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

use zeroize::Zeroizing;

use super::{Error, Transport, USER_AGENT};
use crate::db::task_secrets::{self, Redact};
use crate::providers::host::{Host, MARKER_VAR, Supervised};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const METHODS: &[&str] = &[
    "tools/list",
    "tools/call",
    "resources/list",
    "resources/templates/list",
    "resources/read",
    "prompts/list",
    "prompts/get",
];

pub(super) struct Connection {
    service: RunningService<RoleClient, ClientConfig>,
    process: Option<Supervised>,
    /// Credentials this connection sends, kept out of logged server errors.
    redact: Vec<Redact>,
}

/// The header and environment values `config` sends, which may be
/// credentials a server echoes back in an error.
fn credentials(config: &Transport) -> Vec<Redact> {
    let values = match config {
        Transport::Http { headers, .. } => headers,
        Transport::Stdio { env, .. } => env,
    };
    let mut set: Vec<Redact> = values
        .iter()
        .filter(|(_, value)| value.len() >= task_secrets::MIN_REDACTED)
        .map(|(name, value)| Redact {
            name: name.clone(),
            value: Zeroizing::new(value.clone()),
        })
        .collect();
    set.sort_by_key(|r| std::cmp::Reverse(r.value.len()));
    set
}

pub(super) async fn connect(
    config: &Transport,
    cwd: &Path,
    data_dir: &Path,
) -> Result<Connection, Error> {
    // rmcp offers its newest revision by default, which dropped the
    // initialize handshake. Servers that accept it, such as GitHub's, then
    // hand back a session id they never honor.
    let redact = credentials(config);
    let info =
        ClientConfig::default().with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE);
    let (service, process) = match config {
        Transport::Http { url, headers, .. } => {
            let client = reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .map_err(|_| Error::Connection)?;
            let mut options = StreamableHttpClientTransportConfig::with_uri(url.clone());
            options.channel_buffer_capacity = 8;
            options.max_concurrent_requests = 1;
            options.max_sse_event_size = MAX_RESPONSE_BYTES;
            options.reinit_on_expired_session = false;
            for (name, value) in headers {
                let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| Error::Invalid("Invalid MCP HTTP header name."))?;
                let mut value = reqwest::header::HeaderValue::from_str(value)
                    .map_err(|_| Error::Invalid("Invalid MCP HTTP header value."))?;
                value.set_sensitive(true);
                options.custom_headers.insert(name, value);
            }
            let transport = StreamableHttpClientTransport::with_client(client, options);
            let service = tokio::time::timeout(CONNECT_TIMEOUT, info.serve(transport))
                .await
                .map_err(|_| Error::Timeout)?
                .map_err(|error| {
                    let logged = task_secrets::redact(&error.to_string(), &redact);
                    tracing::warn!(error = %logged, "MCP initialization failed");
                    initialize_error(error)
                })?;
            (service, None)
        }
        Transport::Stdio { command, args, env } => {
            let mut process = tokio::process::Command::new(command);
            process
                .env_clear()
                .args(args)
                .current_dir(cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .process_group(0)
                .kill_on_drop(true);
            for name in ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR", "SYSTEMROOT"] {
                if let Some(value) = std::env::var_os(name) {
                    process.env(name, value);
                }
            }
            process.envs(env);
            let host = Host::new(data_dir, data_dir);
            process.env(
                MARKER_VAR,
                format!("{}:{}", host.marker_prefix(), uuid::Uuid::new_v4()),
            );
            let mut process = Supervised::spawn(process).map_err(|_| Error::Connection)?;
            let stdout = process.child.stdout.take().ok_or(Error::Connection)?;
            let stdin = process.child.stdin.take().ok_or(Error::Connection)?;
            let transport =
                AsyncRwTransport::new(stdout, stdin).with_max_line_length(MAX_RESPONSE_BYTES);
            let service = tokio::time::timeout(CONNECT_TIMEOUT, info.serve(transport))
                .await
                .map_err(|_| Error::Timeout)?
                .map_err(|error| {
                    let error = task_secrets::redact(&error.to_string(), &redact);
                    tracing::warn!(%error, "MCP initialization failed");
                    Error::Connection
                })?;
            (service, Some(process))
        }
    };
    Ok(Connection {
        service,
        process,
        redact,
    })
}

impl Connection {
    pub(super) async fn request(&mut self, method: String, params: Value) -> Result<Value, Error> {
        if !METHODS.contains(&method.as_str()) {
            return Err(Error::Invalid("Unsupported MCP request method."));
        }
        if !params.is_object() {
            return Err(Error::Invalid("MCP request params must be an object."));
        }
        let request: ClientRequest = serde_json::from_value(json!({
            "method": &method,
            "params": params,
        }))
        .map_err(|_| Error::Invalid("Invalid MCP request params."))?;
        if matches!(request, ClientRequest::CustomRequest(_)) {
            return Err(Error::Invalid("Invalid MCP request params."));
        }
        let response =
            match tokio::time::timeout(REQUEST_TIMEOUT, self.service.peer().send_request(request))
                .await
            {
                Ok(Ok(response)) => response,
                Ok(Err(rmcp::ServiceError::TransportSend(error))) => {
                    let logged = task_secrets::redact(&error.to_string(), &self.redact);
                    tracing::warn!(error = %logged, %method, "MCP request failed");
                    return Err(transport_error(error.error.as_ref()));
                }
                Ok(Err(error)) => {
                    let error = task_secrets::redact(&error.to_string(), &self.redact);
                    tracing::warn!(%error, %method, "MCP request failed");
                    return Err(Error::Connection);
                }
                Err(_) => {
                    self.service.cancellation_token().cancel();
                    return Err(Error::Timeout);
                }
            };
        let bytes = serde_json::to_vec(&response).map_err(|_| Error::Connection)?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(Error::Invalid("MCP response exceeds the 2 MiB limit."));
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::Connection)
    }

    pub(super) async fn shutdown(&mut self) {
        if let Some(mut process) = self.process.take() {
            process.signal(rustix::process::Signal::KILL);
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, process.child.wait()).await;
        }
        let _ = self.service.close_with_timeout(CLOSE_TIMEOUT).await;
    }
}

fn transport_error(error: &(dyn std::error::Error + 'static)) -> Error {
    let mut source = Some(error);
    while let Some(error) = source {
        if let Some(error) = error.downcast_ref::<StreamableHttpError<reqwest::Error>>() {
            match error {
                StreamableHttpError::Client(http) => {
                    if let Some(status) = http.status() {
                        return status_error(status.as_u16());
                    }
                }
                StreamableHttpError::HttpStatus(http) => return status_error(http.status),
                // rmcp keeps the status only in this message, followed by
                // the body, which can echo credentials and is left out.
                StreamableHttpError::UnexpectedServerResponse(message) => {
                    if let Some(status) = message
                        .strip_prefix("HTTP ")
                        .and_then(|rest| rest.get(..3))
                        .and_then(|code| code.parse().ok())
                    {
                        return status_error(status);
                    }
                }
                _ => {}
            }
        }
        if let Some(auth) = error.downcast_ref::<AuthRequiredError>() {
            return Error::AuthChallenge(auth.www_authenticate_header.clone());
        }
        if let Some(auth) = error.downcast_ref::<InsufficientScopeError>() {
            return Error::AuthChallenge(auth.www_authenticate_header.clone());
        }
        if let Some(status) = error
            .downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status)
        {
            return status_error(status.as_u16());
        }
        source = error.source();
    }
    Error::Connection
}

fn status_error(status: u16) -> Error {
    if status == reqwest::StatusCode::UNAUTHORIZED.as_u16() {
        Error::AuthChallenge(String::new())
    } else {
        Error::Http(status)
    }
}

fn initialize_error(error: rmcp::service::ClientInitializeError) -> Error {
    if let Some(challenge) = error.auth_challenge() {
        return Error::AuthChallenge(challenge.to_owned());
    }
    match error {
        rmcp::service::ClientInitializeError::TransportError { error, .. } => {
            transport_error(error.error.as_ref())
        }
        rmcp::service::ClientInitializeError::LegacyFallbackFailed { fallback, .. } => {
            initialize_error(*fallback)
        }
        _ => Error::Connection,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Json;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Redirect, Response};
    use axum::routing::post;

    use super::*;

    const STDIO_SERVER: &str = r#"
import json, os, subprocess, sys
calls = 0
descendant = None
for line in sys.stdin:
    message = json.loads(line)
    if 'id' not in message:
        continue
    method = message['method']
    if method == 'initialize':
        result = {'protocolVersion': message['params']['protocolVersion'], 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'test', 'version': '1'}}
    elif method == 'tools/list':
        result = {'tools': [{'name': 'environment', 'description': 'Inspect test environment', 'inputSchema': {'type': 'object'}}]}
    elif method == 'tools/call':
        calls += 1
        if message['params']['name'] == 'descendant' and descendant is None:
            descendant = subprocess.Popen([sys.executable, '-c', 'import signal,time;signal.signal(signal.SIGTERM,signal.SIG_IGN);time.sleep(300)'], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        text = 'x' * (3 * 1024 * 1024) if message['params']['name'] == 'oversized' else json.dumps({'calls': calls, 'env': dict(os.environ), 'cwd': os.getcwd(), 'pid': os.getpid(), 'descendant': descendant.pid if descendant else None})
        result = {'content': [{'type': 'text', 'text': text}]}
    else:
        result = {}
    print(json.dumps({'jsonrpc': '2.0', 'id': message['id'], 'result': result}), flush=True)
    if method == 'tools/call' and message['params']['name'] == 'oversized':
        break
"#;

    fn interpreter_environment() -> Vec<String> {
        let output = std::process::Command::new("python3")
            .env_clear()
            .envs(std::env::var_os("PATH").map(|path| ("PATH", path)))
            .args(["-c", "import json, os; print(json.dumps(list(os.environ)))"])
            .output()
            .unwrap();
        serde_json::from_slice(&output.stdout).unwrap()
    }

    #[tokio::test]
    async fn stdio_reuses_the_server_and_exposes_only_configured_environment() {
        let dir = tempfile::tempdir().unwrap();
        let mut allowed: Vec<String> = [
            "PATH",
            "HOME",
            "LANG",
            "LC_ALL",
            "TMPDIR",
            "SYSTEMROOT",
            "EXPLICIT_CONFIG",
            MARKER_VAR,
        ]
        .map(String::from)
        .into();
        allowed.extend(interpreter_environment());
        let host_only: Vec<String> = std::env::vars_os()
            .filter_map(|(key, _)| key.into_string().ok())
            .filter(|key| !allowed.contains(key))
            .collect();
        assert!(!host_only.is_empty(), "the test host has variables to leak");
        let config = Transport::Stdio {
            command: "python3".into(),
            args: vec!["-u".into(), "-c".into(), STDIO_SERVER.into()],
            env: BTreeMap::from([("EXPLICIT_CONFIG".into(), "present".into())]),
        };
        let mut connection = connect(&config, dir.path(), dir.path()).await.unwrap();
        let listed = connection
            .request("tools/list".into(), json!({}))
            .await
            .unwrap();
        assert_eq!(listed["tools"][0]["name"], "environment");
        let mut pid = None;
        for expected in [1, 2] {
            let called = connection
                .request(
                    "tools/call".into(),
                    json!({"name":"environment", "arguments":{}}),
                )
                .await
                .unwrap();
            let environment: Value =
                serde_json::from_str(called["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(environment["calls"], expected);
            assert_eq!(environment["env"]["EXPLICIT_CONFIG"], "present");
            assert_eq!(
                Path::new(environment["cwd"].as_str().unwrap())
                    .canonicalize()
                    .unwrap(),
                dir.path().canonicalize().unwrap()
            );
            pid = rustix::process::Pid::from_raw(environment["pid"].as_i64().unwrap() as i32);
            for key in environment["env"].as_object().unwrap().keys() {
                assert!(allowed.contains(key), "unexpected inherited variable {key}");
            }
        }
        connection.shutdown().await;
        assert!(
            rustix::process::test_kill_process(pid.expect("the child reported its pid")).is_err(),
            "MCP child exited"
        );
        assert!(
            connection
                .request("tools/list".into(), json!({}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn stdio_rejects_an_oversized_response() {
        let dir = tempfile::tempdir().unwrap();
        let config = Transport::Stdio {
            command: "python3".into(),
            args: vec!["-u".into(), "-c".into(), STDIO_SERVER.into()],
            env: BTreeMap::new(),
        };
        let mut connection = connect(&config, dir.path(), dir.path()).await.unwrap();
        assert!(matches!(
            connection
                .request(
                    "tools/call".into(),
                    json!({"name":"oversized", "arguments":{}}),
                )
                .await,
            Err(Error::Invalid(_)) | Err(Error::Connection)
        ));
        connection.shutdown().await;
    }

    #[tokio::test]
    async fn stdio_shutdown_and_drop_terminate_launcher_descendants() {
        for explicit_shutdown in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let config = Transport::Stdio {
                command: "python3".into(),
                args: vec!["-u".into(), "-c".into(), STDIO_SERVER.into()],
                env: BTreeMap::new(),
            };
            let mut connection = connect(&config, dir.path(), dir.path()).await.unwrap();
            let result = connection
                .request(
                    "tools/call".into(),
                    json!({"name":"descendant", "arguments":{}}),
                )
                .await
                .unwrap();
            let report: Value =
                serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
            let marker = report["env"][MARKER_VAR].as_str().unwrap();
            let turn = marker.rsplit_once(':').unwrap().1.parse().unwrap();
            let host = Host::new(dir.path(), dir.path());
            let descendant = report["descendant"].as_i64().unwrap() as i32;
            // Popen returns once the child begins its exec, before the kernel
            // records its environment, so the marker can show up a moment later.
            tokio::time::timeout(Duration::from_secs(30), async {
                while !host.live_processes(turn).contains(&descendant) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the MCP server's descendant carries its marker");
            if explicit_shutdown {
                connection.shutdown().await;
            }
            drop(connection);
            tokio::time::timeout(Duration::from_secs(2), async {
                while !host.live_processes(turn).is_empty() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("MCP launcher descendants stopped");
        }
    }

    #[tokio::test]
    async fn startup_sweep_finds_mcp_servers_only_for_its_installation() {
        let first_dir = tempfile::tempdir().unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let config = Transport::Stdio {
            command: "python3".into(),
            args: vec!["-u".into(), "-c".into(), STDIO_SERVER.into()],
            env: BTreeMap::new(),
        };
        let mut first = connect(&config, first_dir.path(), first_dir.path())
            .await
            .unwrap();
        let mut other = connect(&config, other_dir.path(), other_dir.path())
            .await
            .unwrap();
        let host = Host::new(first_dir.path(), first_dir.path());
        let swept = tokio::task::spawn_blocking(move || host.sweep_orphans())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(swept, 1);
        assert!(first.request("tools/list".into(), json!({})).await.is_err());
        assert!(other.request("tools/list".into(), json!({})).await.is_ok());
        first.shutdown().await;
        other.shutdown().await;
    }

    #[tokio::test]
    async fn failed_initialization_terminates_launcher_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
import json, os, subprocess, sys
child = subprocess.Popen([sys.executable, '-c', 'import time;time.sleep(300)'], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
with open('started.json', 'w') as f:
    json.dump({'marker': os.environ['ELSE_BOT_PROCESS'], 'child': child.pid}, f)
message = json.loads(sys.stdin.readline())
print(json.dumps({'jsonrpc':'2.0', 'id':message['id'], 'result':{}}), flush=True)
"#;
        let config = Transport::Stdio {
            command: "python3".into(),
            args: vec!["-u".into(), "-c".into(), script.into()],
            env: BTreeMap::new(),
        };
        assert!(connect(&config, dir.path(), dir.path()).await.is_err());
        let report: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("started.json")).unwrap())
                .unwrap();
        let turn = report["marker"]
            .as_str()
            .unwrap()
            .rsplit_once(':')
            .unwrap()
            .1
            .parse()
            .unwrap();
        let host = Host::new(dir.path(), dir.path());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !host.live_processes(turn).is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("failed MCP launcher descendants stopped");
    }

    #[derive(Clone)]
    struct MockState {
        calls: Arc<AtomicUsize>,
    }

    async fn mock_server(
        State(state): State<MockState>,
        headers: HeaderMap,
        Json(message): Json<Value>,
    ) -> Response {
        assert_eq!(headers["authorization"], "Bearer mcp-test-token");
        if message.get("id").is_none() {
            return StatusCode::ACCEPTED.into_response();
        }
        let result = match message["method"].as_str().unwrap() {
            "initialize" => json!({
                "protocolVersion":message["params"]["protocolVersion"],
                "capabilities":{"tools":{},"resources":{},"prompts":{}},
                "serverInfo":{"name":"mock","version":"1"},
            }),
            "tools/list" => json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}),
            "tools/call" => {
                state.calls.fetch_add(1, Ordering::SeqCst);
                json!({"content":[{"type":"text","text":"called"}]})
            }
            "resources/list" => json!({"resources":[]}),
            "resources/templates/list" => json!({"resourceTemplates":[]}),
            "resources/read" => json!({"contents":[{"uri":"test://hello","text":"hello"}]}),
            "prompts/list" => json!({"prompts":[]}),
            "prompts/get" => {
                json!({"messages":[{"role":"user","content":{"type":"text","text":"hello"}}]})
            }
            _ => panic!("unexpected method"),
        };
        Json(json!({"jsonrpc":"2.0","id":message["id"],"result":result})).into_response()
    }

    async fn http_fixture() -> (Transport, MockState, tokio::task::JoinHandle<()>) {
        let state = MockState {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let router = axum::Router::new()
            .route("/mcp", post(mock_server))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let config = Transport::Http {
            secrets: Vec::new(),
            url: format!("http://{address}/mcp"),
            headers: BTreeMap::from([("Authorization".into(), "Bearer mcp-test-token".into())]),
        };
        (config, state, server)
    }

    #[tokio::test]
    async fn http_supports_tools_resources_and_prompts_without_replaying_calls() {
        let (config, state, server) = http_fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let mut connection = connect(&config, dir.path(), dir.path()).await.unwrap();
        for (method, params) in [
            ("tools/list", json!({})),
            ("tools/call", json!({"name":"echo", "arguments":{}})),
            ("resources/list", json!({})),
            ("resources/templates/list", json!({})),
            ("resources/read", json!({"uri":"test://hello"})),
            ("prompts/list", json!({})),
            ("prompts/get", json!({"name":"hello"})),
        ] {
            assert!(
                connection.request(method.into(), params).await.is_ok(),
                "{method}"
            );
        }
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            connection.request("initialize".into(), json!({})).await,
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            connection.request("tools/call".into(), json!({})).await,
            Err(Error::Invalid(_))
        ));
        connection.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn http_refuses_redirects_instead_of_forwarding_credentials() {
        let (target, state, target_server) = http_fixture().await;
        let Transport::Http { url, .. } = target else {
            unreachable!()
        };
        let router = axum::Router::new().route(
            "/mcp",
            post(move || async move { Redirect::temporary(&url) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let redirect_server =
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let config = Transport::Http {
            secrets: Vec::new(),
            url: format!("http://{address}/mcp"),
            headers: BTreeMap::from([("Authorization".into(), "Bearer mcp-test-token".into())]),
        };
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            connect(&config, dir.path(), dir.path()).await,
            Err(Error::Http(307))
        ));
        assert_eq!(state.calls.load(Ordering::SeqCst), 0);
        redirect_server.abort();
        target_server.abort();
    }

    #[tokio::test]
    async fn unauthorized_http_preserves_challenges_and_discovers_without_a_header() {
        for challenge in [None, Some("Bearer scope=read:issues")] {
            let router = axum::Router::new().route(
                "/mcp",
                post(move || async move {
                    let mut response = StatusCode::UNAUTHORIZED.into_response();
                    if let Some(challenge) = challenge {
                        response
                            .headers_mut()
                            .insert("www-authenticate", challenge.parse().unwrap());
                    }
                    response
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let config = Transport::Http {
                secrets: Vec::new(),
                url: format!("http://{address}/mcp"),
                headers: BTreeMap::new(),
            };
            let dir = tempfile::tempdir().unwrap();
            let result = connect(&config, dir.path(), dir.path()).await;
            assert!(
                matches!(result, Err(Error::AuthChallenge(ref found)) if found == challenge.unwrap_or(""))
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn an_expired_http_session_does_not_repeat_a_tool_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let requests = calls.clone();
        let router = axum::Router::new().route(
            "/mcp",
            post(move |Json(message): Json<Value>| {
                let requests = requests.clone();
                async move {
                    match message["method"].as_str() {
                        Some("initialize") => (
                            [("mcp-session-id", "one-session")],
                            Json(json!({
                                "jsonrpc":"2.0","id":message["id"],
                                "result":{
                                    "protocolVersion":message["params"]["protocolVersion"],
                                    "capabilities":{"tools":{}},
                                    "serverInfo":{"name":"expiring","version":"1"},
                                }
                            })),
                        )
                            .into_response(),
                        Some("tools/call") => {
                            requests.fetch_add(1, Ordering::SeqCst);
                            StatusCode::NOT_FOUND.into_response()
                        }
                        _ => StatusCode::ACCEPTED.into_response(),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let config = Transport::Http {
            secrets: Vec::new(),
            url: format!("http://{address}/mcp"),
            headers: BTreeMap::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let mut connection = connect(&config, dir.path(), dir.path()).await.unwrap();
        assert!(matches!(
            connection
                .request(
                    "tools/call".into(),
                    json!({"name":"mutation", "arguments":{}}),
                )
                .await,
            Err(Error::Connection)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        connection.shutdown().await;
        server.abort();
    }
    #[tokio::test]
    async fn http_offers_a_protocol_version_that_keeps_the_initialize_session() {
        let router = axum::Router::new().route(
            "/mcp",
            post(
                |headers: HeaderMap, Json(message): Json<Value>| async move {
                    let offered = message["params"]["protocolVersion"].as_str();
                    match message["method"].as_str() {
                        Some("initialize") => (
                            [(
                                "mcp-session-id",
                                if offered < Some("2026-07-28") {
                                    "kept"
                                } else {
                                    "dropped"
                                },
                            )],
                            Json(json!({
                                "jsonrpc":"2.0","id":message["id"],
                                "result":{
                                    "protocolVersion":"2025-11-25",
                                    "capabilities":{"tools":{}},
                                    "serverInfo":{"name":"github-like","version":"1"},
                                }
                            })),
                        )
                            .into_response(),
                        _ if headers["mcp-session-id"] != "kept" => {
                            (StatusCode::BAD_REQUEST, "invalid session").into_response()
                        }
                        Some("tools/list") => Json(json!({
                            "jsonrpc":"2.0","id":message["id"],"result":{"tools":[]}
                        }))
                        .into_response(),
                        _ => StatusCode::ACCEPTED.into_response(),
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let config = Transport::Http {
            secrets: Vec::new(),
            url: format!("http://{address}/mcp"),
            headers: BTreeMap::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let mut connection = connect(&config, dir.path(), dir.path()).await.unwrap();
        assert!(
            connection
                .request("tools/list".into(), json!({}))
                .await
                .is_ok()
        );
        connection.shutdown().await;
        server.abort();
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn logged_server_errors_leave_out_the_credentials_sent() {
        async fn echo(headers: HeaderMap, Json(message): Json<Value>) -> Response {
            let Some(id) = message.get("id") else {
                return StatusCode::ACCEPTED.into_response();
            };
            let token = headers["authorization"].to_str().unwrap();
            let reply = match message["method"].as_str().unwrap() {
                "initialize" => json!({"jsonrpc":"2.0","id":id,"result":{
                    "protocolVersion":message["params"]["protocolVersion"],
                    "capabilities":{"tools":{}},"serverInfo":{"name":"echo","version":"1"}}}),
                _ => {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":format!("bad credential {token}")}})
                }
            };
            Json(reply).into_response()
        }
        use tracing_subscriber::layer::SubscriberExt;
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new("trace"))
            .with(tracing_subscriber::filter::filter_fn(|meta| {
                !crate::connectors::sensitive_log_target(meta.target())
            }))
            .with(
                tracing_subscriber::fmt::layer()
                    .without_time()
                    .with_ansi(false)
                    .with_writer(move || writer.clone()),
            );
        let _logging = tracing::subscriber::set_default(subscriber);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, axum::Router::new().route("/mcp", post(echo)))
                .await
                .unwrap()
        });
        let config = Transport::Http {
            url: format!("http://{address}/mcp"),
            headers: BTreeMap::from([("Authorization".into(), "Bearer private-token".into())]),
            secrets: Vec::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let mut connection = connect(&config, dir.path(), dir.path()).await.unwrap();
        assert!(
            connection
                .request("tools/list".into(), json!({}))
                .await
                .is_err()
        );
        connection.shutdown().await;
        server.abort();
        let logged = String::from_utf8_lossy(&capture.0.lock().unwrap()).into_owned();
        assert!(
            logged.contains("bad credential [secret Authorization]"),
            "{logged}"
        );
        assert!(!logged.contains("private-token"), "{logged}");
    }

    /// Bento's endpoint sits behind a Cloudflare rule that blocks requests
    /// without a User-Agent with an HTML 403 page.
    async fn bento(headers: HeaderMap, Json(message): Json<Value>) -> Response {
        if headers.get("user-agent").is_none() {
            return (
                StatusCode::FORBIDDEN,
                [("content-type", "text/html")],
                "<title>Attention Required! | Cloudflare</title>",
            )
                .into_response();
        }
        assert_eq!(headers["x-bento-secret-key"], "bento-secret-value");
        let Some(id) = message.get("id") else {
            return StatusCode::ACCEPTED.into_response();
        };
        let result = match message["method"].as_str().unwrap() {
            "initialize" => json!({
                "protocolVersion":"2024-11-05",
                "capabilities":{"tools":{}},
                "serverInfo":{"name":"bento-mcp-worker","version":"0.1.0"},
            }),
            "tools/list" => {
                json!({"tools":[{"name":"bento_list_tags","inputSchema":{"type":"object"}}]})
            }
            other => panic!("unexpected {other}"),
        };
        let event = json!({"jsonrpc":"2.0","id":id,"result":result});
        (
            [("content-type", "text/event-stream")],
            format!("event: message\ndata: {event}\n\n"),
        )
            .into_response()
    }

    async fn serve(router: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (format!("http://{address}/mcp"), server)
    }

    #[tokio::test]
    async fn http_identifies_itself_to_a_server_that_blocks_anonymous_clients() {
        let (url, server) = serve(axum::Router::new().route("/mcp", post(bento))).await;
        let config = Transport::Http {
            url,
            headers: BTreeMap::from([("X-Bento-Secret-Key".into(), "bento-secret-value".into())]),
            secrets: Vec::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let mut connection = connect(&config, dir.path(), dir.path()).await.unwrap();
        let listed = connection
            .request("tools/list".into(), json!({}))
            .await
            .unwrap();
        assert_eq!(listed["tools"][0]["name"], "bento_list_tags");
        connection.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn a_refused_http_connection_reports_its_status() {
        let router = axum::Router::new().route(
            "/mcp",
            post(|| async { (StatusCode::FORBIDDEN, "blocked").into_response() }),
        );
        let (url, server) = serve(router).await;
        let config = Transport::Http {
            url,
            headers: BTreeMap::new(),
            secrets: Vec::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let error = connect(&config, dir.path(), dir.path())
            .await
            .err()
            .expect("the server refuses every request");
        assert!(matches!(error, Error::Http(403)), "{error}");
        assert!(error.to_string().contains("HTTP 403"), "{error}");
        server.abort();
    }
}
