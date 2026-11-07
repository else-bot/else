//! The host Claude adapter, over Claude Code's structured print mode on the
//! host, started in the bot's project directory.
//!
//! One supervised CLI process runs per turn: `claude -p` with stream-json
//! input and output, user-message replay, and partial messages. A new
//! native session gets an ID allocated here (`--session-id`); the next turn
//! resumes it explicitly (`--resume <id>`), never by directory. Sessions
//! live in the host's Claude Code configuration directory under the bot's
//! own project directory (see [`claude_home`]), so another bot cannot
//! resume them; one whose transcript is missing, whose fingerprint changed,
//! or that may hold an uncertain delivery is replaced and the conversation
//! rebuilt from SQLite.
//!
//! Tools, enforced by the CLI's flags and verified from its events: a turn
//! gets the built-in shell and file tools ([`claude_home::HOST_TOOLS`], so
//! the bot can work on the host) and else.bot's MCP tools, all pre-approved
//! by name; nothing else (`--tools`, and the rest denied again in the
//! settings), so no web, skills, or native sub-agents (the `Agent` tool).
//! Work an incoming connector event started gets no built-in tools at all
//! (`--tools ""`, the host tools denied in the settings too): only
//! else.bot's MCP tools, which its grant narrows further. MCP
//! comes only from else.bot's configuration (`--strict-mcp-config`), its
//! tools authorized per call by the turn's task grant; no user, project, or
//! local settings, hooks, or slash commands; an else.bot system prompt
//! replacing Claude Code's. A summary gets no tools at all. The
//! `system/init` event must report exactly the expected tools and server and
//! the saved model; any other tool use, a permission request, a sub-agent's
//! event (anything carrying a parent tool use), a task lifecycle event, or
//! another model stops the turn at once as a policy failure. An empty
//! custom-agent list is not taken as proof: the built-in agents are listed
//! but unusable without the `Agent` tool.
//!
//! The owner's message carries a UUID chosen here; acceptance is its replayed
//! echo, never initialization. Completion comes only from the terminal
//! `result`. Interruption is the CLI's `interrupt` control request; if no
//! result follows within a grace period the process tree is terminated and
//! the turn is uncertain (its session is then retired).

mod protocol;

use std::ffi::OsString;
use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::cli::attempt::{Channel, Ended, Event, Protocol};
use super::cli::files::GRANT_ENV;
use super::cli::session::{self, Identity, Opened, PreparedTurn};
use super::cli::stream::{Interrupt, StreamAttempt};
use super::definitions::Definition;
use super::host::{BotDirs, Host, LaunchSpec};
use super::prompt;
use super::{attachments, claude_home};
use crate::app::providers::{
    Availability, Catalog, CheckpointRequest, Profile, Provider, Stop, TurnInput, TurnOutcome,
    TurnSink,
};
use protocol::Tracker;

pub const ID: &str = "claude";
const PROMPT_VERSION: &str = "else-claude-v16";

pub static DEFINITION: Definition = Definition {
    id: ID,
    label: "Claude",
    catalog: Catalog {
        models: &[
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-fable-5-1",
        ],
        featured_models: &[],
        efforts: &["low", "medium", "high", "xhigh", "max"],
        tiers: &["default"],
    },
    model_note: " An alias would follow the CLI's floating default.",
    effort_note: "",
    tier_note: " Fast mode needs an SDK opt-in this adapter does not use.",
};

/// A CLI that runs, logged in with the host configuration.
struct Ready {
    host: Host,
    program: PathBuf,
    config: claude_home::HostConfig,
    account: String,
    version: String,
}

pub struct Claude {
    ready: Result<Ready, String>,
    tool_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tools {
    None,
    Elsebot,
    HostAndElsebot,
}

impl Tools {
    fn for_turn(input: &TurnInput) -> Self {
        if input.delegation.host_tools() {
            Self::HostAndElsebot
        } else {
            Self::Elsebot
        }
    }

    fn host(self) -> bool {
        self == Self::HostAndElsebot
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
        Self {
            ready: Self::ready(host, program, config, account),
            tool_url: None,
        }
    }

    fn ready(
        host: Option<Host>,
        program: Option<PathBuf>,
        config: Option<claude_home::HostConfig>,
        account: String,
    ) -> Result<Ready, String> {
        let host = host.ok_or("Claude Code is not configured on this host.")?;
        let program = program.ok_or("Claude Code (`claude`) was not found on PATH.")?;
        let env = config
            .as_ref()
            .map(claude_home::HostConfig::env)
            .unwrap_or_default();
        let version = host
            .probe(&program, &["--version"], &env)
            .ok_or("Claude Code does not run (`claude --version` failed).")?;
        let not_logged_in = "Claude Code is not logged in on the host (run `claude` and log in).";
        let config = config.ok_or(not_logged_in)?;
        if !host
            .probe(&program, &["auth", "status"], &env)
            .is_some_and(|s| claude_home::logged_in(&s))
        {
            return Err(not_logged_in.into());
        }
        Ok(Ready {
            host,
            program,
            config,
            account,
            version,
        })
    }

    pub fn with_tools(mut self, url: String) -> Self {
        self.tool_url = Some(url);
        self
    }

    fn fingerprint(&self, ready: &Ready, profile: &Profile, input: &TurnInput) -> String {
        session::fingerprint(
            input,
            profile,
            &Identity {
                prompt_version: PROMPT_VERSION,
                version: &ready.version,
                runtime: &ready.host.runtime(input.bot_id),
                tools: match (input.delegation.host_tools(), self.tool_url.is_some()) {
                    (true, true) => "tools:host+elsebot-v5",
                    (true, false) => "tools:host",
                    (false, true) => "tools:elsebot-v2",
                    (false, false) => "tools:none",
                },
                policy: &["agents:disabled", &ready.account],
            },
        )
    }

    fn offered_tools(&self, tools: Tools) -> Vec<String> {
        let mut offered: Vec<String> = Vec::new();
        if tools.host() {
            offered.extend(claude_home::HOST_TOOLS.iter().map(|t| (*t).to_owned()));
        }
        if tools != Tools::None && self.tool_url.is_some() {
            offered.extend(claude_home::tool_names());
        }
        offered
    }

    fn args(
        &self,
        profile: &Profile,
        resume: bool,
        session: &str,
        system: &str,
        tools: Tools,
    ) -> Vec<OsString> {
        let host_tools = if tools.host() {
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
        let offered = self.offered_tools(tools);
        if !offered.is_empty() {
            args.extend(["--allowedTools".into(), offered.join(",").into()]);
        }
        let flag = if resume { "--resume" } else { "--session-id" };
        args.extend([flag.into(), session.into()]);
        args
    }

    /// Prepare the bot's private files and start the CLI.
    fn launch(
        &self,
        ready: &Ready,
        turn_id: Uuid,
        dirs: BotDirs,
        args: Vec<OsString>,
        tools: Tools,
        grant: Option<&str>,
    ) -> Result<StreamAttempt, String> {
        let tool_url = self.tool_url.as_deref().filter(|_| tools != Tools::None);
        claude_home::prepare(&dirs, tool_url, tools.host()).map_err(|e| e.to_string())?;
        let mut args = args;
        let file = |name: &str| dirs.provider_home.join(name).into_os_string();
        args.extend(["--settings".into(), file(claude_home::SETTINGS_FILE)]);
        if tool_url.is_some() {
            args.extend(["--mcp-config".into(), file(claude_home::MCP_FILE)]);
        }
        let mut env = ready.config.env();
        env.push(("DISABLE_AUTOUPDATER".to_owned(), "1".to_owned()));
        if let (Some(_), Some(grant)) = (tool_url, grant) {
            env.push((GRANT_ENV.to_owned(), grant.to_owned()));
        }
        let spec = LaunchSpec {
            turn_id,
            dirs,
            program: ready.program.clone(),
            args,
            env,
        };
        StreamAttempt::spawn(
            &ready.host,
            &spec,
            Interrupt::Line(interrupt_request()),
            "Claude",
        )
        .map_err(|e| e.to_string())
    }
}

fn failed(code: &str, message: impl Into<String>) -> TurnOutcome {
    TurnOutcome::Failed {
        code: code.to_owned(),
        message: message.into(),
    }
}

fn user_message(uuid: &str, content: Value) -> String {
    let mut line = json!({
        "type": "user",
        "uuid": uuid,
        "message": { "role": "user", "content": content },
    })
    .to_string();
    line.push('\n');
    line
}

fn message_content(segments: &[prompt::Segment<'_>]) -> Value {
    use base64::Engine;
    let mut blocks = Vec::new();
    let mut text = String::new();
    for segment in segments {
        match segment {
            prompt::Segment::Text(t) => text.push_str(t),
            prompt::Segment::File(file) => {
                text.push_str(&prompt::file_reference(&file.attachment));
                if attachments::is_image(&file.attachment)
                    && file.data.len() <= attachments::MAX_INLINE_IMAGE_BYTES
                {
                    blocks.push(json!({ "type": "text", "text": std::mem::take(&mut text) }));
                    blocks.push(json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": file.attachment.media_type,
                            "data": base64::engine::general_purpose::STANDARD.encode(&file.data),
                        },
                    }));
                }
            }
        }
    }
    if blocks.is_empty() {
        return Value::String(text);
    }
    if !text.is_empty() {
        blocks.push(json!({ "type": "text", "text": text }));
    }
    Value::Array(blocks)
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

#[async_trait]
impl Provider for Claude {
    fn project_dir(&self, bot_id: Uuid) -> Option<std::path::PathBuf> {
        self.ready
            .as_ref()
            .ok()
            .map(|r| r.host.project_path(bot_id))
    }

    fn id(&self) -> &'static str {
        ID
    }

    fn label(&self) -> &'static str {
        DEFINITION.label
    }

    fn availability(&self) -> Availability {
        match &self.ready {
            Ok(_) => Availability::Available,
            Err(reason) => Availability::Unavailable(reason.clone()),
        }
    }

    fn uses_profile(&self) -> bool {
        true
    }

    fn validate_profile(&self, profile: &Profile) -> Result<(), String> {
        DEFINITION.validate(profile)
    }

    fn catalog(&self) -> Catalog {
        DEFINITION.catalog
    }

    async fn checkpoint(
        &self,
        request: CheckpointRequest,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let ready = self.ready.as_ref().map_err(Clone::clone)?;
        let profile = request.profile.ok_or("no saved Claude execution profile")?;
        DEFINITION.validate(&profile)?;
        let text = self
            .checkpoint_once(ready, request.bot_id, &profile, &request.prompt, cancel)
            .await?;
        super::bounded_checkpoint(text, request.max_bytes)
    }

    async fn run(&self, input: TurnInput, sink: &dyn TurnSink, stop: Stop) -> TurnOutcome {
        let ready = match &self.ready {
            Ok(ready) => ready,
            Err(reason) => return failed("provider_unavailable", reason.clone()),
        };
        let Some(profile) = input.profile.clone() else {
            return failed(
                "provider_misconfigured",
                "No execution profile was snapshotted.",
            );
        };
        if let Err(reason) = DEFINITION.validate(&profile) {
            return failed("invalid_profile", reason);
        }
        let dirs = match ready.host.bot_dirs(input.bot_id, ID) {
            Ok(dirs) => dirs,
            Err(err) => return failed("provider_unavailable", err.to_string()),
        };
        let fingerprint = self.fingerprint(ready, &profile, &input);
        let opened = match input.session.as_ref().filter(|s| {
            s.fingerprint == fingerprint
                && claude_home::session_exists(&ready.config.dir, &dirs.project, &s.native_id)
        }) {
            Some(session) => Opened::Resumed(session),
            None => Opened::Fresh(Uuid::new_v4().to_string()),
        };
        if stop.is_requested() {
            return TurnOutcome::Interrupted;
        }
        let mut turn = PreparedTurn::new(&input, fingerprint, opened);
        if turn.record(sink).await.is_err() {
            return TurnOutcome::Interrupted;
        }
        if let Err(err) = attachments::stage(&dirs.project, &input.files).await {
            return failed(
                "attachment_unavailable",
                format!("The attached files could not be prepared: {err}"),
            );
        }
        let tools = Tools::for_turn(&input);
        let system = prompt::instructions(&input, &dirs.project.to_string_lossy());
        let args = self.args(&profile, turn.resumed(), turn.native_id(), &system, tools);
        let mut attempt = match self.launch(
            ready,
            input.turn_id,
            dirs,
            args,
            tools,
            input.tool_grant.as_deref(),
        ) {
            Ok(attempt) => attempt,
            Err(err) => return failed("provider_unavailable", err),
        };
        let message_uuid = Uuid::new_v4().to_string();
        let mut tracker = Tracker::new(self.offered_tools(tools), &profile.model, &message_uuid);
        let ended = if sink.sending().await.is_err() {
            Ended::Undelivered(TurnOutcome::Interrupted)
        } else {
            let line = user_message(&message_uuid, message_content(&turn.segments));
            let written = tokio::select! {
                written = attempt.write(&line) => written.map_err(|e| e.to_string()),
                () = stop.escalated() => Err("the stop escalated during delivery".to_owned()),
            };
            match written {
                Ok(()) => attempt.drive(&mut tracker, &mut turn, sink, &stop).await,
                Err(err) => Ended::Abandoned(TurnOutcome::Uncertain {
                    message: format!("Delivery to Claude is unknown: {err}"),
                }),
            }
        };
        attempt.conclude(ended, sink).await
    }
}

impl Claude {
    async fn checkpoint_once(
        &self,
        ready: &Ready,
        bot_id: Uuid,
        profile: &Profile,
        prompt_text: &str,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let session = Uuid::new_v4().to_string();
        let mut args = self.args(
            profile,
            false,
            &session,
            prompt::CHECKPOINT_SYSTEM,
            Tools::None,
        );
        args.push("--no-session-persistence".into());
        let dirs = ready
            .host
            .bot_dirs(bot_id, "claude-summary")
            .map_err(|e| e.to_string())?;
        let mut attempt = self.launch(ready, Uuid::new_v4(), dirs, args, Tools::None, None)?;
        let uuid = Uuid::new_v4().to_string();
        let mut tracker = Tracker::new(Vec::new(), &profile.model, &uuid);
        let deadline = tokio::time::Instant::now() + super::CHECKPOINT_DEADLINE;
        let result = async {
            let line = user_message(&uuid, Value::from(prompt_text));
            tokio::select! {
                _ = cancel.cancelled() => return Err("cancelled".to_owned()),
                written = attempt.write(&line) => {
                    written.map_err(|e| e.to_string())?;
                }
            }
            loop {
                let event = tokio::select! {
                    _ = cancel.cancelled() => return Err("cancelled".to_owned()),
                    _ = tokio::time::sleep_until(deadline) => {
                        return Err("the checkpoint exceeded its deadline".to_owned());
                    }
                    event = attempt.next() => event?,
                };
                let Some(event) = event else {
                    return Err("Claude exited before the checkpoint finished".to_owned());
                };
                let events = tracker
                    .on(&event)
                    .map_err(|v| format!("{}: {}", v.code, v.message))?;
                for event in events {
                    if let Event::Finished(info) = event {
                        return if !tracker.init_seen {
                            Err("Claude did not report its tool set".into())
                        } else if !info.success {
                            Err(info.message)
                        } else if tracker.text.trim().is_empty() {
                            Ok(info.message)
                        } else {
                            Ok(tracker.text.clone())
                        };
                    }
                }
            }
        }
        .await;
        let closed = attempt.close().await;
        let text = result?;
        closed.map(|()| text)
    }
}

#[cfg(test)]
mod tests;
