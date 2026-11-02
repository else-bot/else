//! The host Claude adapter, over Claude Code's structured print mode in the
//! bot's sandbox (POC-020).
//!
//! One supervised CLI process runs per turn: `claude -p` with stream-json
//! input and output, user-message replay, and partial messages. A new
//! native session gets an ID allocated here (`--session-id`); the next turn
//! resumes it explicitly (`--resume <id>`), never by directory. Sessions
//! live in the bot's own `CLAUDE_CONFIG_DIR`, so another bot cannot resume
//! them; one whose transcript is missing, whose fingerprint changed, or that
//! may hold an uncertain delivery is replaced and the conversation rebuilt
//! from PostgreSQL.
//!
//! Restrictions, enforced by the CLI's flags and verified from its events:
//! no built-in tools (`--tools ""`, and denied again in locked settings), so
//! no shell, files, web, skills, or native sub-agents (the `Agent` tool); MCP
//! only from else.bot's configuration (`--strict-mcp-config`), its tools
//! pre-approved by name and authorized per call by the turn's task grant;
//! no user, project, or local settings, hooks, or slash commands; an else.bot
//! system prompt replacing Claude Code's. The `system/init` event must report
//! exactly else.bot's tools and server and the saved model; any other tool
//! use, a permission request, a sub-agent's event (anything carrying a
//! parent tool use), a task lifecycle event, or another model ends the turn
//! as a policy failure. An empty custom-agent list is not taken as proof:
//! the built-in agents are listed but unusable without the `Agent` tool.
//!
//! The owner's message carries a UUID chosen here; acceptance is its replayed
//! echo, never initialization. Completion comes only from the terminal
//! `result`. Interruption is the CLI's `interrupt` control request; if no
//! result follows within a grace period the process tree is terminated and
//! the turn is uncertain (its session is then retired).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::claude_home;
use super::host::{HostSandbox, LaunchSpec, SANDBOX_PROVIDER_HOME, Supervised};
use super::prompt;
use crate::app::providers::{
    Availability, CheckpointRequest, Profile, Provider, SessionState, TurnInput, TurnOutcome,
    TurnSink,
};

pub const ID: &str = "claude";
/// Bump when the system prompt or packet format change.
const PROMPT_VERSION: &str = "else-claude-v1";
/// Explicit model IDs only: an alias would follow the CLI's floating default.
pub const MODELS: &[&str] = &[
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-fable-5-1",
];
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
/// Fast mode needs an SDK opt-in this adapter does not use.
const TIERS: &[&str] = &["default"];
/// Claude Code versions whose flags and events were verified (POC-020).
const VERIFIED_VERSIONS: &[&str] = &["2.1.316"];
const TURN_DEADLINE: Duration = Duration::from_secs(30 * 60);
/// How long an interrupted turn may take to report its result.
const INTERRUPT_GRACE: Duration = Duration::from_secs(15);
const EXIT_GRACE: Duration = Duration::from_secs(5);
const PARK_GRACE: Duration = Duration::from_secs(20);
const PARK_STOP: Duration = Duration::from_secs(15);
/// One event line; larger is a protocol failure.
const MAX_LINE: u64 = 8 * 1024 * 1024;
const STDERR_TAIL: usize = 4096;
/// Writing the owner's message or a control line to the CLI.
const PIPE_TIMEOUT: Duration = Duration::from_secs(30);
const SANDBOX_SETTINGS: &str = "/var/lib/provider-home/else-settings.json";
const SANDBOX_MCP: &str = "/var/lib/provider-home/elsebot-mcp.json";

pub struct Claude {
    sandbox: Option<HostSandbox>,
    program: Option<PathBuf>,
    credentials: Option<PathBuf>,
    account: String,
    version: String,
    unavailable: Option<String>,
    tool_url: Option<String>,
}

enum Session {
    New(String),
    Resume(String),
}

impl Session {
    fn id(&self) -> &str {
        match self {
            Self::New(id) | Self::Resume(id) => id,
        }
    }
}

impl Claude {
    /// Locate the CLI and the host login and check the CLI version inside
    /// the sandbox. Anything missing makes the provider unavailable with a
    /// reason instead of failing startup.
    pub fn detect(
        sandbox: Option<HostSandbox>,
        program: Option<PathBuf>,
        credentials: Option<PathBuf>,
        account: String,
    ) -> Self {
        let version = match (&sandbox, &program) {
            (Some(sandbox), Some(program)) => sandbox.probe(program, &["--version"]),
            _ => None,
        };
        let unavailable = if sandbox.is_none() {
            Some("bubblewrap (bwrap) is required to run Claude Code.".to_owned())
        } else if program.is_none() {
            Some("Claude Code (`claude`) was not found on PATH.".to_owned())
        } else if !credentials
            .as_ref()
            .is_some_and(|c| std::fs::metadata(c).is_ok_and(|m| m.is_file() && m.len() > 0))
        {
            Some("Claude Code is not logged in on the host (run `claude` and log in).".to_owned())
        } else {
            match &version {
                None => Some("Claude Code does not run inside the sandbox.".to_owned()),
                Some(v) if !VERIFIED_VERSIONS.iter().any(|ok| v.starts_with(ok)) => Some(format!(
                    "Claude Code {v} is not a verified version (verified: {}).",
                    VERIFIED_VERSIONS.join(", ")
                )),
                Some(_) => None,
            }
        };
        Self {
            sandbox,
            program,
            credentials,
            account,
            version: version.unwrap_or_default(),
            unavailable,
            tool_url: None,
        }
    }

    pub fn with_tools(mut self, url: String) -> Self {
        self.tool_url = Some(url);
        self
    }

    fn fingerprint(&self, profile: &Profile, input: &TurnInput) -> String {
        let mut hasher = Sha256::new();
        let registry = prompt::registry_text(input);
        for part in [
            registry.as_str(),
            PROMPT_VERSION,
            &self.version,
            "runtime:bwrap:/work:/var/lib/provider-home",
            if self.tool_url.is_some() {
                "tools:elsebot-v2"
            } else {
                "tools:none"
            },
            "agents:disabled",
            &self.account,
            &profile.model,
            &profile.reasoning_effort,
            &profile.service_tier,
            &input.bot_name,
            &input.bot_role,
        ] {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        hasher
            .finalize()
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// CLI arguments for one process. `tools` selects else.bot's MCP tools;
    /// without them nothing at all can be called.
    fn args(
        &self,
        profile: &Profile,
        session: &Session,
        system: &str,
        tools: bool,
    ) -> Vec<OsString> {
        let mut args: Vec<OsString> = [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--replay-user-messages",
            "--include-partial-messages",
            "--model",
            &profile.model,
            "--effort",
            &profile.reasoning_effort,
            "--tools",
            "",
            "--strict-mcp-config",
            "--setting-sources",
            "",
            "--settings",
            SANDBOX_SETTINGS,
            "--permission-mode",
            "dontAsk",
            "--disable-slash-commands",
            "--system-prompt",
            system,
            "--disallowedTools",
            &claude_home::DENIED_TOOLS.join(","),
        ]
        .iter()
        .map(OsString::from)
        .collect();
        if tools {
            args.extend(["--mcp-config".into(), SANDBOX_MCP.into()]);
            args.extend([
                "--allowedTools".into(),
                claude_home::tool_names().join(",").into(),
            ]);
        }
        match session {
            Session::New(id) => args.extend(["--session-id".into(), id.into()]),
            Session::Resume(id) => args.extend(["--resume".into(), id.into()]),
        }
        args
    }

    fn launch(
        &self,
        turn_id: Uuid,
        home: &str,
        bot_id: Uuid,
        args: Vec<OsString>,
        tools: bool,
        grant: Option<&str>,
    ) -> Result<(Supervised, super::host::BotDirs), String> {
        let (Some(sandbox), Some(program), Some(credentials)) =
            (&self.sandbox, &self.program, &self.credentials)
        else {
            return Err("Claude Code is not configured.".into());
        };
        let dirs = sandbox.bot_dirs(bot_id, home).map_err(|e| e.to_string())?;
        let tool_url = self.tool_url.as_deref().filter(|_| tools);
        let login =
            claude_home::prepare(&dirs, credentials, tool_url).map_err(|e| e.to_string())?;
        let mut env = vec![
            (
                "CLAUDE_CONFIG_DIR".to_owned(),
                SANDBOX_PROVIDER_HOME.to_owned(),
            ),
            ("DISABLE_AUTOUPDATER".to_owned(), "1".to_owned()),
        ];
        if let (Some(_), Some(grant)) = (tool_url, grant) {
            env.push((claude_home::GRANT_ENV.to_owned(), grant.to_owned()));
        }
        let spec = LaunchSpec {
            turn_id,
            dirs: dirs.clone(),
            program: program.clone(),
            args,
            env,
            extra_binds: vec![login],
        };
        let mut command = sandbox.command(&spec).map_err(|e| e.to_string())?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = Supervised::spawn(command).map_err(|e| e.to_string())?;
        Ok((child, dirs))
    }
}

fn failed(code: &str, message: impl Into<String>) -> TurnOutcome {
    TurnOutcome::Failed {
        code: code.to_owned(),
        message: message.into(),
    }
}

/// A user message on the CLI's stdin, with the UUID its echo carries.
fn user_message(uuid: &str, text: &str) -> String {
    let mut line = json!({
        "type": "user",
        "uuid": uuid,
        "message": { "role": "user", "content": text },
    })
    .to_string();
    line.push('\n');
    line
}

fn interrupt_request() -> String {
    let mut line = json!({
        "type": "control_request",
        "request_id": format!("else-{}", Uuid::new_v4()),
        "request": { "subtype": "interrupt" },
    })
    .to_string();
    line.push('\n');
    line
}

/// What the terminal `result` event says.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResultInfo {
    success: bool,
    message: String,
    /// Model turns the CLI ran; 0 means it ended before running any.
    num_turns: Option<i64>,
}

/// What one event means for the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Text(String),
    Accepted,
    /// A permission prompt or other request the CLI waits on.
    Control(String),
    DelegationPending,
    Result(ResultInfo),
}

/// Interprets the CLI's events for one turn. Pure, so it is unit-tested on
/// recorded event lines.
struct Tracker {
    expected_tools: Vec<String>,
    model: String,
    message_uuid: String,
    init_seen: bool,
    accepted: bool,
    violation: Option<(&'static str, String)>,
    tools_by_id: HashMap<String, String>,
    wrote_text: bool,
    separate: bool,
    text: String,
    /// The model produced anything (a message start, text, or a tool use).
    activity: bool,
}

impl Tracker {
    fn new(expected_tools: Vec<String>, model: &str, message_uuid: &str) -> Self {
        let mut expected_tools = expected_tools;
        expected_tools.sort();
        Self {
            expected_tools,
            model: model.to_owned(),
            message_uuid: message_uuid.to_owned(),
            init_seen: false,
            accepted: false,
            violation: None,
            tools_by_id: HashMap::new(),
            wrote_text: false,
            separate: false,
            text: String::new(),
            activity: false,
        }
    }

    fn flag(&mut self, code: &'static str, reason: String) {
        if self.violation.is_none() {
            self.violation = Some((code, reason));
        }
    }

    fn check_model(&mut self, model: Option<&str>) {
        if let Some(model) = model.filter(|m| *m != self.model) {
            let reason = format!("Claude used {model} instead of {}.", self.model);
            self.flag("profile_mismatch", reason);
        }
    }

    fn on_init(&mut self, event: &Value) {
        self.init_seen = true;
        self.check_model(Some(event["model"].as_str().unwrap_or("(none)")));
        let mut tools: Vec<String> = event["tools"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|t| t.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        tools.sort();
        if tools != self.expected_tools {
            let reason = format!("Claude offered unexpected tools: {tools:?}");
            self.flag("policy_violation", reason);
        }
        let servers: Vec<(String, String)> = event["mcp_servers"]
            .as_array()
            .map(|list| {
                list.iter()
                    .map(|s| {
                        (
                            s["name"].as_str().unwrap_or_default().to_owned(),
                            s["status"].as_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let ok = if self.expected_tools.is_empty() {
            servers.is_empty()
        } else {
            matches!(servers.as_slice(), [(name, status)]
                if name == crate::http::mcp::SERVER_NAME && status == "connected")
        };
        if !ok {
            let reason = format!("Claude reported MCP servers {servers:?}");
            self.flag("policy_violation", reason);
        }
    }

    fn on(&mut self, event: &Value) -> Vec<Step> {
        let mut steps = Vec::new();
        // Anything produced on behalf of a tool use is a sub-agent's.
        if event["parent_tool_use_id"].is_string() {
            self.flag(
                "policy_violation",
                "Claude ran a native sub-agent".to_owned(),
            );
            return steps;
        }
        match event["type"].as_str().unwrap_or_default() {
            "system" => match event["subtype"].as_str().unwrap_or_default() {
                "init" => self.on_init(event),
                sub if sub.starts_with("task_") || sub.contains("agent") => {
                    self.flag(
                        "policy_violation",
                        format!("Claude reported native agent activity ({sub})"),
                    );
                }
                _ => {}
            },
            "user" => {
                if !self.accepted
                    && event["uuid"].as_str() == Some(self.message_uuid.as_str())
                    && event["isReplay"].as_bool() == Some(true)
                {
                    self.accepted = true;
                    steps.push(Step::Accepted);
                }
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] != "tool_result" {
                        continue;
                    }
                    let id = block["tool_use_id"].as_str().unwrap_or_default();
                    let delegate = format!("mcp__{}__delegate", crate::http::mcp::SERVER_NAME);
                    if self.tools_by_id.get(id) == Some(&delegate)
                        && block["content"].to_string().contains("pending")
                    {
                        steps.push(Step::DelegationPending);
                    }
                }
            }
            "stream_event" => {
                self.activity = true;
                let inner = &event["event"];
                match inner["type"].as_str().unwrap_or_default() {
                    "message_start" => self.check_model(inner["message"]["model"].as_str()),
                    "content_block_start" if inner["content_block"]["type"] == "text" => {
                        self.separate = self.wrote_text;
                    }
                    "content_block_delta" if inner["delta"]["type"] == "text_delta" => {
                        if let Some(text) = inner["delta"]["text"].as_str() {
                            if self.separate {
                                self.separate = false;
                                steps.push(Step::Text("\n\n".into()));
                                self.text.push_str("\n\n");
                            }
                            self.wrote_text = true;
                            self.text.push_str(text);
                            steps.push(Step::Text(text.to_owned()));
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                self.activity = true;
                self.check_model(event["message"]["model"].as_str());
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_use" {
                        let name = block["name"].as_str().unwrap_or_default().to_owned();
                        if !self.expected_tools.contains(&name) {
                            self.flag(
                                "policy_violation",
                                format!(
                                    "Claude called a tool outside else.bot's allowlist: {name}"
                                ),
                            );
                        }
                        let id = block["id"].as_str().unwrap_or_default().to_owned();
                        self.tools_by_id.insert(id, name);
                    }
                }
            }
            "control_request" => {
                let request = &event["request"];
                let what = request["tool_name"]
                    .as_str()
                    .or(request["subtype"].as_str())
                    .unwrap_or("?")
                    .to_owned();
                self.flag(
                    "policy_violation",
                    format!("Claude asked for permission: {what}"),
                );
                steps.push(Step::Control(
                    event["request_id"].as_str().unwrap_or_default().to_owned(),
                ));
            }
            "result" => {
                let success =
                    event["subtype"] == "success" && event["is_error"].as_bool() != Some(true);
                let message = event["result"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| {
                        event["errors"].as_array().map(|errors| {
                            errors
                                .iter()
                                .filter_map(|e| e.as_str())
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                    })
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| {
                        format!(
                            "Claude ended the turn: {}",
                            event["subtype"].as_str().unwrap_or("unknown")
                        )
                    });
                steps.push(Step::Result(ResultInfo {
                    success,
                    message,
                    num_turns: event["num_turns"].as_i64(),
                }));
            }
            _ => {}
        }
        steps
    }
}

/// Reads bounded event lines from the CLI. Partially read bytes stay in
/// `buf`, so a read abandoned by a `select!` (a timer or cancellation) loses
/// nothing: the next call continues the same line.
struct Events<R> {
    reader: R,
    buf: Vec<u8>,
}

impl<R: tokio::io::AsyncBufRead + Unpin> Events<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            buf: Vec::new(),
        }
    }

    /// The next event, `Ok(None)` at end of output.
    async fn next(&mut self) -> Result<Option<Value>, String> {
        loop {
            let room = (MAX_LINE as usize).saturating_sub(self.buf.len()) as u64;
            if room == 0 {
                return Err("Claude sent an oversized event".into());
            }
            let read = (&mut self.reader)
                .take(room)
                .read_until(b'\n', &mut self.buf)
                .await
                .map_err(|e| e.to_string())?;
            if read == 0 && self.buf.is_empty() {
                return Ok(None);
            }
            if read != 0 && !self.buf.ends_with(b"\n") {
                // The limit cut the line, or more is coming.
                continue;
            }
            let line = std::mem::take(&mut self.buf);
            if line.iter().all(u8::is_ascii_whitespace) {
                if read == 0 {
                    return Ok(None);
                }
                continue;
            }
            return serde_json::from_slice(&line)
                .map(Some)
                .map_err(|e| format!("Claude sent malformed output: {e}"));
        }
    }
}

/// Keeps the end of the CLI's stderr for error messages.
fn collect_stderr(stderr: tokio::process::ChildStderr) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut tail: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf).await {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > STDERR_TAIL * 2 {
                tail.drain(..tail.len() - STDERR_TAIL);
            }
        }
        let text = String::from_utf8_lossy(&tail).into_owned();
        text.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default()
            .to_owned()
    })
}

/// The profile rules without an adapter (also used to check restored
/// settings).
pub fn validate(profile: &Profile) -> Result<(), String> {
    if !MODELS.contains(&profile.model.as_str()) {
        return Err(format!(
            "Unsupported Claude model {}. Use an explicit model ID: {}.",
            profile.model,
            MODELS.join(", ")
        ));
    }
    if !EFFORTS.contains(&profile.reasoning_effort.as_str()) {
        return Err(format!(
            "Unsupported reasoning effort {}. Supported: {}.",
            profile.reasoning_effort,
            EFFORTS.join(", ")
        ));
    }
    if !TIERS.contains(&profile.service_tier.as_str()) {
        return Err(format!(
            "Unsupported service tier {}. Supported: {}.",
            profile.service_tier,
            TIERS.join(", ")
        ));
    }
    Ok(())
}

#[async_trait]
impl Provider for Claude {
    fn id(&self) -> &'static str {
        ID
    }

    fn label(&self) -> &'static str {
        "Claude"
    }

    fn availability(&self) -> Availability {
        match &self.unavailable {
            Some(reason) => Availability::Unavailable(reason.clone()),
            None => Availability::Available,
        }
    }

    fn uses_profile(&self) -> bool {
        true
    }

    fn validate_profile(&self, profile: &Profile) -> Result<(), String> {
        validate(profile)
    }

    async fn checkpoint(
        &self,
        request: CheckpointRequest,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        if let Some(reason) = &self.unavailable {
            return Err(reason.clone());
        }
        let profile = request.profile.ok_or("no saved Claude execution profile")?;
        self.validate_profile(&profile)?;
        let result = tokio::time::timeout(
            super::CHECKPOINT_DEADLINE,
            self.checkpoint_once(request.bot_id, &profile, &request.prompt, cancel),
        )
        .await
        .unwrap_or_else(|_| Err("the checkpoint exceeded its deadline".into()))?;
        super::bounded_checkpoint(result, request.max_bytes)
    }

    async fn run(
        &self,
        input: TurnInput,
        sink: &dyn TurnSink,
        cancel: CancellationToken,
    ) -> TurnOutcome {
        if let Some(reason) = &self.unavailable {
            return failed("provider_unavailable", reason.clone());
        }
        let Some(profile) = input.profile.clone() else {
            return failed(
                "provider_misconfigured",
                "No execution profile was snapshotted.",
            );
        };
        if let Err(reason) = self.validate_profile(&profile) {
            return failed("invalid_profile", reason);
        }
        let Some(sandbox) = &self.sandbox else {
            return failed("provider_unavailable", "Claude Code is not configured.");
        };
        let dirs = match sandbox.bot_dirs(input.bot_id, ID) {
            Ok(dirs) => dirs,
            Err(err) => return failed("provider_unavailable", err.to_string()),
        };
        let fingerprint = self.fingerprint(&profile, &input);
        // Resume only a compatible session whose transcript is still in
        // this bot's own home; otherwise start over from PostgreSQL.
        let resumed = input.session.as_ref().filter(|s| {
            s.fingerprint == fingerprint && claude_home::session_exists(&dirs, &s.native_id)
        });
        let (session, considered) = match resumed {
            Some(s) => (
                Session::Resume(s.native_id.clone()),
                s.considered_through_turn,
            ),
            None => (Session::New(Uuid::new_v4().to_string()), 0),
        };
        if cancel.is_cancelled() {
            return TurnOutcome::Interrupted;
        }
        let inject = input
            .memory
            .as_ref()
            .filter(|_| resumed.is_none_or(|s| s.memory_revision.is_none()));
        let mut state = SessionState {
            native_id: session.id().to_owned(),
            fingerprint,
            considered_through_turn: considered,
            memory_revision: resumed.and_then(|s| s.memory_revision),
        };
        if sink.session(&state).await.is_err() {
            return TurnOutcome::Interrupted;
        }
        let (packet, selection) = prompt::turn_packet(&input, considered, inject);
        if sink.packet(&selection, &packet).await.is_err() {
            return TurnOutcome::Interrupted;
        }
        let tools = self.tool_url.is_some();
        let system = prompt::instructions(&input);
        let args = self.args(&profile, &session, &system, tools);
        let (mut child, _) = match self.launch(
            input.turn_id,
            ID,
            input.bot_id,
            args,
            tools,
            input.tool_grant.as_deref(),
        ) {
            Ok(launched) => launched,
            Err(err) => return failed("provider_unavailable", err),
        };
        let mut stdin = child.child.stdin.take();
        let stdout = child.child.stdout.take().expect("piped");
        let stderr = collect_stderr(child.child.stderr.take().expect("piped"));
        let mut events = Events::new(BufReader::new(stdout));
        let message_uuid = Uuid::new_v4().to_string();
        let expected = if tools {
            claude_home::tool_names()
        } else {
            Vec::new()
        };
        let mut tracker = Tracker::new(expected, &profile.model, &message_uuid);

        // From here a crash leaves delivery uncertain.
        if sink.sending().await.is_err() {
            child.terminate(EXIT_GRACE).await;
            return TurnOutcome::Interrupted;
        }
        let line = user_message(&message_uuid, &packet);
        let written = match stdin.as_mut() {
            Some(pipe) => tokio::time::timeout(PIPE_TIMEOUT, async {
                pipe.write_all(line.as_bytes()).await?;
                pipe.flush().await
            })
            .await
            .unwrap_or_else(|_| Err(std::io::Error::other("the CLI stopped reading its input"))),
            None => Err(std::io::Error::other("no stdin")),
        };
        if let Err(err) = written {
            child.terminate(EXIT_GRACE).await;
            let reason = stderr_tail(stderr).await;
            return TurnOutcome::Uncertain {
                message: format!("Delivery to Claude is unknown: {err} {reason}"),
            };
        }

        let deadline = tokio::time::Instant::now() + TURN_DEADLINE;
        let mut interrupted = false;
        let mut stop_by: Option<tokio::time::Instant> = None;
        let mut park_at: Option<tokio::time::Instant> = None;
        let mut deadline_hit = false;
        let mut parked = false;
        let outcome = loop {
            let wake_at = stop_by.or(park_at).unwrap_or(deadline).min(deadline);
            let event = tokio::select! {
                _ = cancel.cancelled(), if !interrupted => {
                    interrupted = true;
                    stop_by = Some(tokio::time::Instant::now() + INTERRUPT_GRACE);
                    send(&mut stdin, &interrupt_request()).await;
                    continue;
                }
                _ = tokio::time::sleep_until(wake_at) => {
                    if stop_by.is_some_and(|t| t <= tokio::time::Instant::now()) {
                        // No result after interruption: end the process
                        // tree; the outcome of the turn is unknown, unless
                        // it was a parked attempt the request waits on.
                        child.terminate(EXIT_GRACE).await;
                        // The native turn never finished: its session must
                        // not be resumed, whatever the request does next.
                        sink.retire_session().await;
                        if parked {
                            break TurnOutcome::Interrupted;
                        }
                        break TurnOutcome::Uncertain {
                            message: "Claude did not stop after being interrupted.".into(),
                        };
                    }
                    if park_at.is_some_and(|t| t <= tokio::time::Instant::now()) && !interrupted {
                        tracing::info!(turn = %input.turn_id, "parent kept going after delegating; interrupting");
                        park_at = None;
                        parked = true;
                        interrupted = true;
                        stop_by = Some(tokio::time::Instant::now() + PARK_STOP);
                        send(&mut stdin, &interrupt_request()).await;
                        continue;
                    }
                    if tokio::time::Instant::now() >= deadline && !deadline_hit {
                        deadline_hit = true;
                        interrupted = true;
                        stop_by = Some(tokio::time::Instant::now() + INTERRUPT_GRACE);
                        send(&mut stdin, &interrupt_request()).await;
                    }
                    continue;
                }
                event = events.next() => event,
            };
            let event = match event {
                Ok(Some(event)) => event,
                Ok(None) => {
                    child.terminate(EXIT_GRACE).await;
                    let reason = stderr_tail(stderr).await;
                    break TurnOutcome::Uncertain {
                        message: format!(
                            "The Claude process exited before the turn finished. {reason}"
                        )
                        .trim()
                        .to_owned(),
                    };
                }
                Err(err) => {
                    child.terminate(EXIT_GRACE).await;
                    break TurnOutcome::Uncertain { message: err };
                }
            };
            // Names and types only: payloads carry conversation text.
            if event["type"] != "stream_event" {
                tracing::debug!(
                    kind = event["type"].as_str().unwrap_or_default(),
                    subtype = event["subtype"].as_str().unwrap_or_default(),
                    "claude event"
                );
            }
            let had_violation = tracker.violation.is_some();
            let mut result = None;
            for step in tracker.on(&event) {
                match step {
                    Step::Text(text) => {
                        if sink.text(&text).await.is_err() {
                            cancel.cancel();
                        }
                    }
                    Step::Accepted => {
                        // A stop racing acceptance still drains the turn.
                        if sink.running().await.is_err() {
                            cancel.cancel();
                        }
                        state.considered_through_turn = input.turn_no;
                        if let Some(memory) = inject {
                            state.memory_revision = Some(memory.revision);
                        }
                        let _ = sink.session(&state).await;
                        let reported = json!({
                            "model": event_model(&event).unwrap_or(&profile.model),
                            "reasoning_effort": null,
                            "service_tier": null,
                        });
                        let _ = sink.reported(&reported, &message_uuid).await;
                    }
                    Step::Control(request_id) => {
                        let reply = json!({
                            "type": "control_response",
                            "response": {
                                "subtype": "error",
                                "request_id": request_id,
                                "error": "denied by else.bot",
                            },
                        });
                        send(&mut stdin, &format!("{reply}\n")).await;
                    }
                    Step::DelegationPending => {
                        if park_at.is_none() && !parked {
                            park_at = Some(tokio::time::Instant::now() + PARK_GRACE);
                        }
                    }
                    Step::Result(info) => result = Some(info),
                }
            }
            if !had_violation && tracker.violation.is_some() && !interrupted {
                interrupted = true;
                stop_by = Some(tokio::time::Instant::now() + INTERRUPT_GRACE);
                send(&mut stdin, &interrupt_request()).await;
            }
            if let Some(info) = result {
                break decide(&tracker, &info, interrupted, deadline_hit);
            }
        };
        // Close input so the CLI exits, then reap the sandbox.
        drop(stdin);
        if tokio::time::timeout(EXIT_GRACE, child.child.wait())
            .await
            .is_err()
        {
            child.terminate(EXIT_GRACE).await;
        }
        let observed = sandbox.live_processes(input.turn_id);
        if !observed.is_empty() {
            tracing::warn!(turn = %input.turn_id, count = observed.len(), "Claude left processes; terminated");
            child.terminate(EXIT_GRACE).await;
        }
        outcome
    }
}

/// The turn's outcome from its terminal `result` and what was observed.
fn decide(
    tracker: &Tracker,
    info: &ResultInfo,
    interrupted: bool,
    deadline_hit: bool,
) -> TurnOutcome {
    if let Some((code, reason)) = tracker.violation.clone() {
        return failed(code, reason);
    }
    if !tracker.accepted {
        // Refusal only when the CLI provably consumed nothing: an error
        // result with no model turn and no output at all (for example a
        // session it could not open). Anything else may have acted.
        let refused = !info.success
            && !interrupted
            && !tracker.activity
            && !tracker.accepted
            && info.num_turns == Some(0);
        return if refused {
            failed("provider_refused", info.message.clone())
        } else {
            TurnOutcome::Uncertain {
                message: "Claude ended without acknowledging the message.".into(),
            }
        };
    }
    if !tracker.init_seen {
        // The tool set, servers, and model were never verified.
        return failed(
            "policy_violation",
            "Claude did not report its tool set and model.",
        );
    }
    if deadline_hit {
        failed("turn_deadline", "The turn exceeded its 30 minute deadline.")
    } else if info.success {
        TurnOutcome::Completed
    } else if interrupted {
        TurnOutcome::Interrupted
    } else {
        failed("provider_failed", info.message.clone())
    }
}

fn event_model(event: &Value) -> Option<&str> {
    event["message"]["model"].as_str()
}

/// Write a control line; a CLI that stops reading cannot hold the turn.
async fn send(stdin: &mut Option<tokio::process::ChildStdin>, line: &str) {
    if let Some(pipe) = stdin.as_mut() {
        let _ = tokio::time::timeout(PIPE_TIMEOUT, async {
            pipe.write_all(line.as_bytes()).await?;
            pipe.flush().await
        })
        .await;
    }
}

/// The stderr tail, without waiting long for a process being reaped.
async fn stderr_tail(stderr: tokio::task::JoinHandle<String>) -> String {
    tokio::time::timeout(Duration::from_secs(2), stderr)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

impl Claude {
    /// One checkpoint with nothing callable: no built-in tools, no MCP
    /// server, no session persistence, a fresh session in a separate per-bot
    /// home. The init event must confirm no tools and no servers; any tool
    /// use, sub-agent, or permission request rejects the output.
    async fn checkpoint_once(
        &self,
        bot_id: Uuid,
        profile: &Profile,
        prompt_text: &str,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let session = Session::New(Uuid::new_v4().to_string());
        let mut args = self.args(profile, &session, super::codex::CHECKPOINT_SYSTEM, false);
        args.push("--no-session-persistence".into());
        let (mut child, _) =
            self.launch(Uuid::new_v4(), "claude-summary", bot_id, args, false, None)?;
        let mut stdin = child.child.stdin.take();
        let stdout = child.child.stdout.take().expect("piped");
        let stderr = collect_stderr(child.child.stderr.take().expect("piped"));
        let mut events = Events::new(BufReader::new(stdout));
        let uuid = Uuid::new_v4().to_string();
        send(&mut stdin, &user_message(&uuid, prompt_text)).await;
        let mut tracker = Tracker::new(Vec::new(), &profile.model, &uuid);
        let result = loop {
            let event = tokio::select! {
                _ = cancel.cancelled() => break Err("cancelled".to_owned()),
                event = events.next() => event,
            };
            let event = match event {
                Ok(Some(event)) => event,
                Ok(None) => {
                    child.terminate(EXIT_GRACE).await;
                    break Err(format!(
                        "Claude exited before the checkpoint finished: {}",
                        stderr_tail(stderr).await
                    ));
                }
                Err(err) => break Err(err),
            };
            let mut done = None;
            for step in tracker.on(&event) {
                match step {
                    Step::Result(info) => done = Some(info),
                    Step::Control(_) => {}
                    _ => {}
                }
            }
            if tracker.violation.is_some() {
                break Err(format!(
                    "policy_violation: {}",
                    tracker.violation.clone().unwrap().1
                ));
            }
            if let Some(info) = done {
                break if !tracker.init_seen {
                    Err("Claude did not report its tool set".into())
                } else if info.success {
                    Ok(if tracker.text.trim().is_empty() {
                        info.message
                    } else {
                        tracker.text.clone()
                    })
                } else {
                    Err(info.message)
                };
            }
        };
        drop(stdin);
        child.terminate(EXIT_GRACE).await;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> Tracker {
        Tracker::new(claude_home::tool_names(), "claude-haiku-5-5", "u-1")
    }

    fn init(tools: Vec<String>, servers: Value) -> Value {
        json!({ "type": "system", "subtype": "init", "model": "claude-haiku-5-5",
                "tools": tools, "mcp_servers": servers,
                "agents": ["general-purpose", "Explore"] })
    }

    fn connected() -> Value {
        json!([{ "name": "elsebot", "status": "connected" }])
    }

    #[test]
    fn a_turn_is_accepted_by_its_echo_and_streams_text() {
        let mut t = tracker();
        t.on(&init(claude_home::tool_names(), connected()));
        assert!(t.violation.is_none(), "{:?}", t.violation);
        // Another message's echo is not acceptance.
        assert!(
            t.on(&json!({"type":"user","uuid":"other","isReplay":true}))
                .is_empty()
        );
        assert_eq!(
            t.on(&json!({"type":"user","uuid":"u-1","isReplay":true})),
            vec![Step::Accepted]
        );
        t.on(&json!({"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-haiku-5-5"}}}));
        t.on(&json!({"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"text"}}}));
        let steps = t.on(&json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}}));
        assert_eq!(steps, vec![Step::Text("Hi".into())]);
        t.on(&json!({"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"text"}}}));
        let steps = t.on(&json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"again"}}}));
        assert_eq!(
            steps,
            vec![Step::Text("\n\n".into()), Step::Text("again".into())]
        );
        let steps = t.on(
            &json!({"type":"result","subtype":"success","is_error":false,"result":"Hi\n\nagain"}),
        );
        assert_eq!(
            steps,
            vec![Step::Result(ResultInfo {
                success: true,
                message: "Hi\n\nagain".into(),
                num_turns: None,
            })]
        );
        assert!(t.violation.is_none());
    }

    #[test]
    fn an_agent_tool_or_extra_server_in_the_inventory_is_refused() {
        let mut tools = claude_home::tool_names();
        tools.push("Agent".into());
        let mut t = tracker();
        t.on(&init(tools, connected()));
        assert_eq!(t.violation.as_ref().unwrap().0, "policy_violation");
        let mut t = tracker();
        t.on(&init(
            claude_home::tool_names(),
            json!([{ "name": "elsebot", "status": "connected" }, { "name": "other", "status": "connected" }]),
        ));
        assert!(t.violation.is_some());
        // else.bot's server failing to connect is not a working inventory.
        let mut t = tracker();
        t.on(&init(
            claude_home::tool_names(),
            json!([{ "name": "elsebot", "status": "failed" }]),
        ));
        assert!(t.violation.is_some());
    }

    #[test]
    fn the_built_in_agent_list_alone_is_not_a_violation_but_its_activity_is() {
        // The init event lists built-in agents even with no Agent tool.
        let mut t = tracker();
        t.on(&init(claude_home::tool_names(), connected()));
        assert!(t.violation.is_none());
        t.on(&json!({"type":"system","subtype":"task_started","task_id":"x"}));
        assert!(t.violation.as_ref().unwrap().1.contains("task_started"));
        let mut t = tracker();
        t.on(&json!({"type":"assistant","parent_tool_use_id":"toolu_1","message":{"model":"claude-haiku-5-5","content":[]}}));
        assert!(t.violation.as_ref().unwrap().1.contains("sub-agent"));
    }

    #[test]
    fn other_tools_models_and_permission_prompts_are_refused() {
        let mut t = tracker();
        t.on(
            &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"t1","name":"mcp__elsebot__memory_search","input":{}}]}}),
        );
        assert!(t.violation.is_none());
        t.on(
            &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"t2","name":"Bash","input":{}}]}}),
        );
        assert!(t.violation.as_ref().unwrap().1.contains("Bash"));
        let mut t = tracker();
        t.on(&json!({"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-5-5"}}}));
        assert_eq!(t.violation.as_ref().unwrap().0, "profile_mismatch");
        let mut t = tracker();
        let steps = t.on(&json!({"type":"control_request","request_id":"r9","request":{"subtype":"can_use_tool","tool_name":"Write"}}));
        assert_eq!(steps, vec![Step::Control("r9".into())]);
        assert!(t.violation.as_ref().unwrap().1.contains("Write"));
    }

    #[test]
    fn a_pending_delegation_is_noticed_from_its_tool_result() {
        let mut t = tracker();
        t.on(
            &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"d1","name":"mcp__elsebot__delegate","input":{}}]}}),
        );
        let steps = t.on(&json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"d1","content":[{"type":"text","text":"{\"status\":\"pending\"}"}]}]}}));
        assert_eq!(steps, vec![Step::DelegationPending]);
    }

    #[test]
    fn a_no_tool_checkpoint_requires_an_empty_inventory() {
        let mut t = Tracker::new(Vec::new(), "claude-haiku-5-5", "u");
        t.on(&init(Vec::new(), json!([])));
        assert!(t.violation.is_none());
        let mut t = Tracker::new(Vec::new(), "claude-haiku-5-5", "u");
        t.on(&init(claude_home::tool_names(), connected()));
        assert!(t.violation.is_some());
    }

    #[test]
    fn profiles_need_explicit_supported_values() {
        let claude = Claude::detect(None, None, None, String::new());
        let ok = Profile {
            model: "claude-opus-5-5".into(),
            reasoning_effort: "high".into(),
            service_tier: "default".into(),
        };
        assert!(claude.validate_profile(&ok).is_ok());
        for bad in [
            Profile {
                model: "opus".into(),
                ..ok.clone()
            },
            Profile {
                model: "claude-opus-4".into(),
                ..ok.clone()
            },
            Profile {
                reasoning_effort: "ultra".into(),
                ..ok.clone()
            },
            Profile {
                service_tier: "priority".into(),
                ..ok.clone()
            },
        ] {
            assert!(claude.validate_profile(&bad).is_err(), "{bad:?}");
        }
        assert!(matches!(
            claude.availability(),
            Availability::Unavailable(_)
        ));
    }

    #[test]
    fn arguments_disable_built_ins_and_resume_explicitly() {
        let claude = Claude::detect(None, None, None, String::new())
            .with_tools("http://127.0.0.1:1/tools/v1/mcp".into());
        let profile = Profile {
            model: "claude-sonnet-5-5".into(),
            reasoning_effort: "medium".into(),
            service_tier: "default".into(),
        };
        let args: Vec<String> = claude
            .args(&profile, &Session::Resume("abc".into()), "SYSTEM", true)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        let after = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
        assert_eq!(after("--tools"), "");
        assert_eq!(after("--model"), "claude-sonnet-5-5");
        assert_eq!(after("--effort"), "medium");
        assert_eq!(after("--resume"), "abc");
        assert_eq!(after("--setting-sources"), "");
        assert_eq!(after("--system-prompt"), "SYSTEM");
        assert!(after("--disallowedTools").contains("Agent"));
        assert!(
            after("--allowedTools")
                .split(',')
                .all(|t| t.starts_with("mcp__elsebot__"))
        );
        assert!(args.contains(&"--strict-mcp-config".to_owned()));
        assert!(
            !args
                .iter()
                .any(|a| a == "--continue" || a == "-c" || a == "--bare")
        );
    }

    fn info(success: bool, num_turns: Option<i64>) -> ResultInfo {
        ResultInfo {
            success,
            message: "m".into(),
            num_turns,
        }
    }

    #[test]
    fn only_a_provable_non_start_is_a_refusal() {
        // No session could be opened: nothing ran.
        let t = tracker();
        assert!(matches!(
            decide(&t, &info(false, Some(0)), false, false),
            TurnOutcome::Failed { ref code, .. } if code == "provider_refused"
        ));
        // A success, model output, or an unknown turn count without the
        // echo may have consumed the message.
        assert!(matches!(
            decide(&t, &info(true, Some(1)), false, false),
            TurnOutcome::Uncertain { .. }
        ));
        assert!(matches!(
            decide(&t, &info(false, None), false, false),
            TurnOutcome::Uncertain { .. }
        ));
        let mut active = tracker();
        active.on(&json!({"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-haiku-5-5"}}}));
        assert!(matches!(
            decide(&active, &info(false, Some(0)), false, false),
            TurnOutcome::Uncertain { .. }
        ));
    }

    #[test]
    fn completion_requires_a_verified_init() {
        let mut t = tracker();
        t.on(&json!({"type":"user","uuid":"u-1","isReplay":true}));
        assert!(matches!(
            decide(&t, &info(true, Some(1)), false, false),
            TurnOutcome::Failed { ref code, .. } if code == "policy_violation"
        ));
        t.on(&init(claude_home::tool_names(), connected()));
        assert_eq!(
            decide(&t, &info(true, Some(1)), false, false),
            TurnOutcome::Completed
        );
    }

    #[tokio::test]
    async fn an_abandoned_read_keeps_its_partial_line() {
        use tokio::io::AsyncWriteExt;
        let (mut tx, rx) = tokio::io::duplex(64);
        let mut events = Events::new(BufReader::new(rx));
        tx.write_all(br#"{"type":"result","#).await.unwrap();
        // A timer wins the select while half the line has been read.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.next())
                .await
                .is_err()
        );
        tx.write_all(b"\"subtype\":\"success\"}\n").await.unwrap();
        let event = events.next().await.unwrap().unwrap();
        assert_eq!(event["subtype"], "success");
        drop(tx);
        assert_eq!(events.next().await.unwrap(), None);
    }
}
