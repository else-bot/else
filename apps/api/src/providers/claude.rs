//! The host Claude adapter, over Claude Code's structured print mode on the
//! host, started in the bot's project directory (POC-020).
//!
//! One supervised CLI process runs per turn: `claude -p` with stream-json
//! input and output, user-message replay, and partial messages. A new
//! native session gets an ID allocated here (`--session-id`); the next turn
//! resumes it explicitly (`--resume <id>`), never by directory. Sessions
//! live in the bot's own `CLAUDE_CONFIG_DIR`, so another bot cannot resume
//! them; one whose transcript is missing, whose fingerprint changed, or that
//! may hold an uncertain delivery is replaced and the conversation rebuilt
//! from SQLite.
//!
//! Tools, enforced by the CLI's flags and verified from its events: a turn
//! gets the built-in shell and file tools ([`claude_home::HOST_TOOLS`], so
//! the bot can work on the host) and else.bot's MCP tools, all pre-approved
//! by name; nothing else (`--tools`, and the rest denied again in the
//! settings), so no web, skills, or native sub-agents (the `Agent` tool). MCP
//! comes only from else.bot's configuration (`--strict-mcp-config`), its
//! tools authorized per call by the turn's task grant; no user, project, or
//! local settings, hooks, or slash commands; an else.bot system prompt
//! replacing Claude Code's. A summary gets no tools at all. The
//! `system/init` event must report exactly the expected tools and server and
//! the saved model; any other tool use, a permission request, a sub-agent's
//! event (anything carrying a parent tool use), a task lifecycle event, or
//! another model ends the turn as a policy failure. An empty custom-agent list is not taken as proof:
//! the built-in agents are listed but unusable without the `Agent` tool.
//!
//! The owner's message carries a UUID chosen here; acceptance is its replayed
//! echo, never initialization. Completion comes only from the terminal
//! `result`. Interruption is the CLI's `interrupt` control request; if no
//! result follows within a grace period the process tree is terminated and
//! the turn is uncertain (its session is then retired).

mod protocol;

#[cfg(test)]
use protocol::ResultInfo;
use protocol::{Events, Step, Tracker, decide, event_model};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::claude_home;
use super::host::{Host, LaunchSpec, Supervised};
use super::prompt;
use crate::app::providers::{
    Availability, CheckpointRequest, Profile, Provider, SessionState, TurnInput, TurnOutcome,
    TurnSink,
};

pub const ID: &str = "claude";
/// Bump when the system prompt or packet format change.
const PROMPT_VERSION: &str = "else-claude-v11";
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

pub struct Claude {
    host: Option<Host>,
    program: Option<PathBuf>,
    config: Option<claude_home::HostConfig>,
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
    /// Locate the CLI, check that it runs (`claude --version`), and ask it
    /// whether the host login works (`claude auth status`, with the
    /// configuration turns use). Anything missing makes the provider
    /// unavailable with a reason instead of failing startup. The version is
    /// only session identity.
    pub fn detect(
        host: Option<Host>,
        program: Option<PathBuf>,
        config: Option<claude_home::HostConfig>,
        account: String,
    ) -> Self {
        let env = config
            .as_ref()
            .map(claude_home::HostConfig::env)
            .unwrap_or_default();
        let probe = |args: &[&str]| match (&host, &program) {
            (Some(host), Some(program)) => host.probe(program, args, &env),
            _ => None,
        };
        let version = probe(&["--version"]);
        let logged_in = config.is_some()
            && probe(&["auth", "status"]).is_some_and(|s| claude_home::logged_in(&s));
        let unavailable = if host.is_none() {
            Some("Claude Code is not configured on this host.".to_owned())
        } else if program.is_none() {
            Some("Claude Code (`claude`) was not found on PATH.".to_owned())
        } else if version.is_some() && !logged_in {
            Some("Claude Code is not logged in on the host (run `claude` and log in).".to_owned())
        } else if version.is_none() {
            Some("Claude Code does not run (`claude --version` failed).".to_owned())
        } else {
            None
        };
        Self {
            host,
            program,
            config,
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
        let runtime = self
            .host
            .as_ref()
            .map_or_else(String::new, |h| h.runtime(input.bot_id));
        for part in [
            registry.as_str(),
            PROMPT_VERSION,
            &self.version,
            &runtime,
            if self.tool_url.is_some() {
                "tools:host+elsebot-v4"
            } else {
                "tools:host"
            },
            "agents:disabled",
            &self.account,
            &profile.model,
            &profile.reasoning_effort,
            &profile.service_tier,
            &input.bot_name,
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

    /// The tools a turn is offered: the host tools and else.bot's.
    fn turn_tools(&self) -> Vec<String> {
        let mut tools: Vec<String> = claude_home::HOST_TOOLS
            .iter()
            .map(|t| (*t).to_owned())
            .collect();
        if self.tool_url.is_some() {
            tools.extend(claude_home::tool_names());
        }
        tools
    }

    /// CLI arguments for one process, apart from the files in the bot's
    /// provider home (added at launch). `tools` selects a turn's tools;
    /// without them nothing at all can be called.
    fn args(
        &self,
        profile: &Profile,
        session: &Session,
        system: &str,
        tools: bool,
    ) -> Vec<OsString> {
        let host_tools = if tools {
            claude_home::HOST_TOOLS.join(",")
        } else {
            String::new()
        };
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
            &host_tools,
            "--strict-mcp-config",
            "--setting-sources",
            "",
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
            args.extend(["--allowedTools".into(), self.turn_tools().join(",").into()]);
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
        let (Some(host), Some(program), Some(config)) = (&self.host, &self.program, &self.config)
        else {
            return Err("Claude Code is not configured.".into());
        };
        let dirs = host.bot_dirs(bot_id, home).map_err(|e| e.to_string())?;
        let tool_url = self.tool_url.as_deref().filter(|_| tools);
        claude_home::prepare(&dirs, tool_url, tools).map_err(|e| e.to_string())?;
        let mut args = args;
        let file = |name: &str| dirs.provider_home.join(name).into_os_string();
        args.extend(["--settings".into(), file(claude_home::SETTINGS_FILE)]);
        if tool_url.is_some() {
            args.extend(["--mcp-config".into(), file(claude_home::MCP_FILE)]);
        }
        let mut env = config.env();
        env.push(("DISABLE_AUTOUPDATER".to_owned(), "1".to_owned()));
        if let (Some(_), Some(grant)) = (tool_url, grant) {
            env.push((claude_home::GRANT_ENV.to_owned(), grant.to_owned()));
        }
        let spec = LaunchSpec {
            turn_id,
            dirs: dirs.clone(),
            program: program.clone(),
            args,
            env,
        };
        let mut command = host.command(&spec);
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

    fn catalog(&self) -> crate::app::providers::Catalog {
        crate::app::providers::Catalog {
            models: MODELS,
            efforts: EFFORTS,
            tiers: TIERS,
        }
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
        let Some(host) = &self.host else {
            return failed("provider_unavailable", "Claude Code is not configured.");
        };
        let dirs = match host.bot_dirs(input.bot_id, ID) {
            Ok(dirs) => dirs,
            Err(err) => return failed("provider_unavailable", err.to_string()),
        };
        let fingerprint = self.fingerprint(&profile, &input);
        // Resume only a compatible session whose transcript is still in
        // this bot's own home; otherwise start over from SQLite.
        let resumed = input.session.as_ref().filter(|s| {
            s.fingerprint == fingerprint
                && self.config.as_ref().is_some_and(|c| {
                    claude_home::session_exists(&c.dir, &dirs.project, &s.native_id)
                })
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
        let inject = prompt::memory_for(input.memory.as_ref(), resumed);
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
        let tools = true;
        let system = prompt::instructions(&input, &dirs.project.to_string_lossy());
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
        let expected = self.turn_tools();
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
                    Step::EndMessage => {
                        if sink.end_message().await.is_err() {
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
                    Step::Parked => {
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
        // Close input so the CLI exits, then reap its process group.
        drop(stdin);
        if tokio::time::timeout(EXIT_GRACE, child.child.wait())
            .await
            .is_err()
        {
            child.terminate(EXIT_GRACE).await;
        }
        let observed = host.live_processes(input.turn_id);
        if !observed.is_empty() {
            tracing::warn!(turn = %input.turn_id, count = observed.len(), "Claude left processes; terminated");
            child.terminate(EXIT_GRACE).await;
        }
        outcome
    }
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
        let mut args = self.args(profile, &session, prompt::CHECKPOINT_SYSTEM, false);
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
mod tests;
