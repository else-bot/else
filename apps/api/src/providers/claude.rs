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
//! the saved model. A call to any other name must come back as the CLI's own
//! error result, which the model can recover from; that call's success, a
//! permission request, a sub-agent's event (anything carrying a parent tool
//! use, except the progress heartbeat for a slow tool call the turn itself
//! made), a task lifecycle event, or another model stops the turn at once
//! as a policy failure. An empty
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
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::cli::attempt::{self, Ended};
use super::cli::session::PreparedTurn;
use super::cli::stream::{Interrupt, Prompt, StreamAttempt};
use super::cli::turn::{self, Adapter, Cli, Delivery, Launch, Ready, Start, Stream, ToolSet};
use super::definitions::Definition;
use super::host::{BotDirs, Host, LaunchSpec, Run, RunError};
use super::models::Listed;
use super::prompt;
use super::{attachments, claude_home};
use crate::app::providers::{CheckpointRequest, Profile, Stop, TurnSink};
use protocol::Tracker;

pub const ID: &str = "claude";
const PROMPT_VERSION: &str = "else-claude-v17";
const MODELS_REQUEST: &str = "else-models";

pub static DEFINITION: Definition = Definition {
    id: ID,
    label: "Claude",
    fallback: &[
        "claude-opus-5-5",
        "claude-fable-5-2",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
        "claude-fable-5-1",
    ],
    efforts: &["low", "medium", "high", "xhigh", "max"],
    tiers: &["default"],
    pinned: |model| model.starts_with("claude-") && !model.contains(char::is_whitespace),
    model_note: " An alias would follow the CLI's floating default.",
    effort_note: "",
    tier_note: " Fast mode needs an SDK opt-in this adapter does not use.",
    build,
};

fn build<'w>(wiring: &'w super::Wiring<'_>) -> futures_util::future::BoxFuture<'w, super::Built> {
    let (host, bin) = (wiring.host.clone(), wiring.config.claude_bin.clone());
    let probe: turn::Probe<Login> = Box::new(move || {
        let (host, bin) = (host.clone(), bin.clone());
        Box::pin(async move {
            let config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
            let home = std::env::var("HOME").ok();
            let program = super::executable(&host, bin.as_deref(), "claude").await;
            ready(
                Some(host),
                program,
                claude_home::host_config(config_dir.as_deref(), home.as_deref()),
                claude_home::host_account(config_dir.as_deref(), home.as_deref()),
            )
            .await
        })
    });
    Box::pin(async move {
        let claude = Claude::probed(ClaudeCode, probe)
            .await
            .with_tools(wiring.tool_url.clone());
        super::Built {
            provider: std::sync::Arc::new(claude),
            summarizer: None,
        }
    })
}

/// The host login a running CLI uses.
pub struct Login {
    config: claude_home::HostConfig,
    account: String,
}

pub struct ClaudeCode;

pub type Claude = Cli<ClaudeCode>;

impl Claude {
    /// Locate the CLI, check that it runs (`claude --version`), and ask it
    /// whether the host login works (`claude auth status`, with the
    /// configuration turns use). Anything missing makes the provider
    /// unavailable with a reason instead of failing startup. The version is
    /// only session identity.
    pub async fn detect(
        host: Option<Host>,
        program: Option<PathBuf>,
        config: Option<claude_home::HostConfig>,
        account: String,
    ) -> Self {
        Self::probed(
            ClaudeCode,
            Box::new(move || {
                let (host, program) = (host.clone(), program.clone());
                let (config, account) = (config.clone(), account.clone());
                Box::pin(ready(host, Ok(program.map(Into::into)), config, account))
            }),
        )
        .await
    }
}

async fn ready(
    host: Option<Host>,
    program: Result<Option<super::Program>, String>,
    config: Option<claude_home::HostConfig>,
    account: String,
) -> Result<Ready<Login>, String> {
    let host = host.ok_or("Claude Code is not configured on this host.")?;
    let program = program?.ok_or("Claude Code (`claude`) was not found on PATH.")?;
    let env = config
        .as_ref()
        .map(claude_home::HostConfig::env)
        .unwrap_or_default();
    let version = turn::version(&host, &program, &env, ClaudeCode::PRODUCT).await?;
    let not_logged_in = "Claude Code is not logged in on the host (run `claude` and log in).";
    let config = config.ok_or(not_logged_in)?;
    // A logged-out CLI exits with failure; a check that never answered,
    // or was killed by a signal, says nothing about the login.
    match host.probe(program.path(), &["auth", "status"], &env).await {
        Ok(status) if claude_home::logged_in(&status) => {}
        Ok(_) | Err(RunError::Empty) => return Err(not_logged_in.into()),
        Err(RunError::Failed { status, .. }) if status.code().is_some() => {
            return Err(not_logged_in.into());
        }
        Err(why) => {
            return Err(format!(
                "Claude Code could not check its login (`claude auth status`: {why})."
            ));
        }
    }
    Ok(Ready {
        host,
        program,
        version,
        login: Login { config, account },
    })
}

fn offered_tools(tools: ToolSet) -> Vec<String> {
    let mut offered: Vec<String> = Vec::new();
    if tools.host() {
        offered.extend(claude_home::HOST_TOOLS.iter().map(|t| (*t).to_owned()));
    }
    if tools.elsebot() {
        offered.extend(claude_home::tool_names());
    }
    offered
}

fn args(
    profile: &Profile,
    resume: bool,
    session: &str,
    system: &str,
    tools: ToolSet,
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
    let offered = offered_tools(tools);
    if !offered.is_empty() {
        args.extend(["--allowedTools".into(), offered.join(",").into()]);
    }
    let flag = if resume { "--resume" } else { "--session-id" };
    args.extend([flag.into(), session.into()]);
    args
}

/// Write the bot's private settings and MCP configuration, point `args`
/// at them, and return the launch's environment.
fn configure(
    login: &Login,
    dirs: &BotDirs,
    args: &mut Vec<OsString>,
    tool_url: Option<&str>,
    tools: ToolSet,
) -> Result<Vec<(String, String)>, String> {
    claude_home::prepare(dirs, tool_url, tools.host()).map_err(|e| e.to_string())?;
    let file = |name: &str| dirs.provider_home.join(name).into_os_string();
    args.extend(["--settings".into(), file(claude_home::SETTINGS_FILE)]);
    if tool_url.is_some() {
        args.extend(["--mcp-config".into(), file(claude_home::MCP_FILE)]);
    }
    let mut env = login.config.env();
    env.push(("DISABLE_AUTOUPDATER".to_owned(), "1".to_owned()));
    // Otherwise the CLI may move a slow MCP call into a background task
    // and report task_started, which ends the turn as native agent
    // activity.
    env.push((
        "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS".to_owned(),
        "1".to_owned(),
    ));
    Ok(env)
}

fn user_message(uuid: &str, content: Value) -> String {
    let mut line = user_value(uuid, content).to_string();
    line.push('\n');
    line
}

fn user_value(uuid: &str, content: Value) -> Value {
    json!({
        "type": "user",
        "uuid": uuid,
        "message": { "role": "user", "content": content },
    })
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
impl Adapter for ClaudeCode {
    type Login = Login;

    const DEFINITION: &'static Definition = &DEFINITION;
    const PRODUCT: &'static str = "Claude Code";
    const PROMPT_VERSION: &'static str = PROMPT_VERSION;

    fn tool_tag(&self, tools: ToolSet) -> &'static str {
        match tools {
            ToolSet::HostAndElsebot => "tools:host+elsebot-v5",
            ToolSet::Host => "tools:host",
            ToolSet::Elsebot => "tools:elsebot-v2",
            ToolSet::None => "tools:none",
        }
    }

    async fn list_models(&self, ready: &Ready<Login>) -> Result<Vec<Listed>, String> {
        let mut env = ready.login.config.env();
        env.push(("DISABLE_AUTOUPDATER".to_owned(), "1".to_owned()));
        let request = format!("{}\n", protocol::initialize_request(MODELS_REQUEST));
        let run = Run {
            env,
            input: request.into_bytes(),
            limit: protocol::MODELS_LIMIT,
            timeout: super::models::ASK_TIMEOUT,
            ..Run::new(
                &ready.program.current().await?,
                &[
                    "-p",
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--setting-sources",
                    "",
                    "--strict-mcp-config",
                    "--tools",
                    "",
                    "--no-session-persistence",
                ],
            )
        };
        ready
            .host
            .ask(&run, |line| protocol::listed_models(line, MODELS_REQUEST))
            .await
            .map_err(|e| e.to_string())
            .and_then(|listed| listed)
            .map_err(|why| format!("Claude Code did not list its models: {why}"))
    }

    async fn checkpoint(
        &self,
        cli: &Claude,
        request: CheckpointRequest,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let ready = cli.ready()?;
        let profile = request.profile.ok_or("no saved Claude execution profile")?;
        cli.models.validate(&profile)?;
        let text =
            checkpoint_once(&ready, request.bot_id, &profile, &request.prompt, cancel).await?;
        super::bounded_checkpoint(text, request.max_bytes)
    }
}

#[async_trait]
impl Stream for ClaudeCode {
    type Protocol = Tracker;

    fn policy(&self, ready: &Ready<Login>) -> Vec<String> {
        vec!["agents:disabled".into(), ready.login.account.clone()]
    }

    fn session_exists(
        &self,
        ready: &Ready<Login>,
        _: &BotDirs,
        workspace: &Path,
        id: &str,
    ) -> bool {
        claude_home::session_exists(&ready.login.config.dir, workspace, id)
    }

    async fn launch(
        &self,
        ready: &Ready<Login>,
        start: Start<'_>,
    ) -> Result<Launch<Tracker>, String> {
        let turn = start.turn;
        let system = prompt::instructions(start.input, &start.dirs.project.to_string_lossy());
        let mut args = args(
            start.profile,
            turn.resumed(),
            turn.native_id(),
            &system,
            start.tools,
        );
        let env = configure(
            &ready.login,
            start.dirs,
            &mut args,
            start.tool_url,
            start.tools,
        )?;
        let message_uuid = Uuid::new_v4().to_string();
        Ok(Launch {
            args,
            env,
            delivery: Delivery::Line {
                message: user_message(&message_uuid, message_content(&turn.segments)),
                interrupt: interrupt_request(),
            },
            protocol: Tracker::new(
                offered_tools(start.tools),
                &start.profile.model,
                &message_uuid,
            ),
        })
    }

    async fn drive(
        &self,
        attempt: &mut StreamAttempt,
        _: Prompt,
        protocol: &mut Tracker,
        turn: &mut PreparedTurn<'_>,
        sink: &dyn TurnSink,
        stop: &Stop,
    ) -> Ended {
        attempt.drive(protocol, turn, sink, stop).await
    }
}

async fn checkpoint_once(
    ready: &Ready<Login>,
    bot_id: Uuid,
    profile: &Profile,
    prompt_text: &str,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let session = Uuid::new_v4().to_string();
    let mut args = args(
        profile,
        false,
        &session,
        prompt::CHECKPOINT_SYSTEM,
        ToolSet::None,
    );
    args.push("--no-session-persistence".into());
    let dirs = ready
        .host
        .bot_dirs(bot_id, "claude-summary")
        .map_err(|e| e.to_string())?;
    let program = ready
        .program
        .current()
        .await
        .map_err(|why| format!("Could not start Claude Code: {why}"))?;
    let env = configure(&ready.login, &dirs, &mut args, None, ToolSet::None)?;
    let spec = LaunchSpec {
        turn_id: Uuid::new_v4(),
        dirs,
        program,
        args,
        env,
    };
    let mut attempt = StreamAttempt::spawn(
        &ready.host,
        &spec,
        Interrupt::Line(interrupt_request()),
        DEFINITION.label,
    )
    .map_err(|e| e.to_string())?;
    let uuid = Uuid::new_v4().to_string();
    let mut tracker = Tracker::new(Vec::new(), &profile.model, &uuid);
    let exchange = async {
        let line = user_message(&uuid, Value::from(prompt_text));
        attempt.write(&line).await.map_err(|e| e.to_string())?;
        let (text, info) = attempt::collect(&mut attempt, &mut tracker, DEFINITION.label).await?;
        if !tracker.init_seen {
            Err("Claude did not report its tool set".into())
        } else if !info.success {
            Err(info.message)
        } else if text.trim().is_empty() {
            Ok(info.message)
        } else {
            Ok(text)
        }
    };
    let result = tokio::select! {
        _ = cancel.cancelled() => Err("cancelled".to_owned()),
        result = tokio::time::timeout(super::CHECKPOINT_DEADLINE, exchange) => {
            result.unwrap_or_else(|_| Err("the checkpoint exceeded its deadline".to_owned()))
        }
    };
    let closed = attempt.close().await;
    let text = result?;
    closed.map(|()| text)
}

#[cfg(test)]
pub(super) mod tests;
