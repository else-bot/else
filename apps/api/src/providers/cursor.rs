//! The host Cursor adapter, over Cursor CLI's headless print mode
//! (`cursor-agent -p --output-format stream-json`).
//!
//! One CLI process runs per turn and reads the packet on stdin. A new chat
//! gets an ID allocated here (`--new-session-id`); the next turn resumes it
//! (`--resume <id>`) when it is in the bot's own home and its fingerprint
//! matches. The CLI runs with the bot's provider home as `HOME` (see
//! [`super::cursor_home`]), sharing only the host's login, and ignores the
//! project's CLI config and `.envrc`. `--allowed-tools` offers else.bot's
//! MCP server, plus the host shell and file tools when the owner started
//! the work, and every call is checked again. Owner turns start in the
//! bot's project; other turns start in a private workspace, so nothing a
//! guest wrote there loads. Cursor takes no system prompt from most
//! accounts, so else.bot's instructions lead the packet whenever the chat
//! has not seen this exact text. Stop is SIGINT. The CLI takes no input
//! while it runs, so owner messages that join a turn wait for its next
//! attempt.

mod protocol;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;

use super::cli::attempt::{self, Channel, Ended};
use super::cli::session::PreparedTurn;
use super::cli::stream::{Prompt, StreamAttempt};
use super::cli::turn::{self, Adapter, Cli, Delivery, Launch, Ready, Start, ToolSet};
use super::cursor_home::{self, Login};
use super::definitions::Definition;
use super::host::{BotDirs, Host, Run};
use super::models::Listed;
use super::prompt;
use crate::app::providers::{Stop, TurnInput, TurnSink};
use protocol::{Message, Tracker};

pub const ID: &str = "cursor";
const PROMPT_VERSION: &str = "else-cursor-v2";
const INSTRUCTIONS: &str = "instructions";

pub static DEFINITION: Definition = Definition {
    id: ID,
    label: "Cursor",
    fallback: &["composer-2.5", "composer-2"],
    efforts: &["default"],
    tiers: &["default"],
    pinned: super::definitions::any_id,
    model_note: "",
    effort_note: " Cursor names the effort in the model ID, such as gpt-5.5-high.",
    tier_note: " Choose a model ID ending in -fast for its fast variant.",
    build,
};

fn build<'w>(wiring: &'w super::Wiring<'_>) -> futures_util::future::BoxFuture<'w, super::Built> {
    Box::pin(async move {
        let program = lookup(
            wiring.config.cursor_bin.as_deref(),
            std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        )
        .await;
        let cursor = Cursor::new(
            CursorCli,
            ready(Some(wiring.host.clone()), program, Login::host()).await,
        )
        .with_tools(wiring.tool_url.clone());
        super::Built {
            provider: std::sync::Arc::new(cursor),
            summarizer: None,
        }
    })
}

/// `ELSE_CURSOR_BIN`, else the link Cursor's installer keeps in
/// `~/.local/bin`, which a service's `PATH` often lacks, else
/// `cursor-agent` on `PATH`. A link is kept, not resolved: an update points
/// it at a new versioned directory and removes the old one.
async fn lookup(
    configured: Option<&Path>,
    home: Option<&Path>,
) -> Result<Option<super::Program>, String> {
    use std::os::unix::fs::PermissionsExt;
    let runs = |file: &PathBuf| {
        file.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let path = std::env::var_os("PATH");
    let on_path = || {
        std::env::split_paths(path.as_deref()?)
            .map(|dir| dir.join("cursor-agent"))
            .find(runs)
    };
    let Some(given) = configured
        .map(Path::to_owned)
        .or_else(|| {
            home.map(|home| home.join(cursor_home::INSTALLED))
                .filter(runs)
        })
        .or_else(on_path)
    else {
        return Ok(None);
    };
    super::executable_as(Some(&given), "cursor-agent", "ELSE_CURSOR_BIN").await
}

/// The models `cursor-agent models` prints after "Available models", one
/// `id - Display Name (current, default)` line each.
fn listed_models(printed: &str) -> Vec<Listed> {
    printed
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("Available models"))
        .skip(1)
        .map(str::trim)
        .skip_while(|line| line.is_empty())
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_whitespace().next())
        .map(Listed::id)
        .collect()
}

pub struct CursorCli;

pub type Cursor = Cli<CursorCli>;

impl Cursor {
    pub async fn detect(host: Option<Host>, program: Option<PathBuf>, login: Login) -> Self {
        Self::new(
            CursorCli,
            ready(host, Ok(program.map(Into::into)), login).await,
        )
    }
}

/// A CLI that runs and is new enough, and how it finds the host's login.
async fn ready(
    host: Option<Host>,
    program: Result<Option<super::Program>, String>,
    login: Login,
) -> Result<Ready<Login>, String> {
    let host = host.ok_or("Cursor CLI is not configured on this host.")?;
    let program = program?.ok_or(cursor_home::NOT_INSTALLED)?;
    let version = turn::version(&host, &program, &[], CursorCli::PRODUCT).await?;
    if !cursor_home::supported(&version) {
        return Err(format!(
            "Cursor CLI {version} is older than {}. Run `cursor-agent update` on this server.",
            cursor_home::MIN_VERSION
        ));
    }
    Ok(Ready {
        host,
        program,
        version,
        login,
    })
}

/// The attempt, reporting how the CLI exited: Cursor writes no result
/// after an error or SIGINT, and writes its result before storing the chat,
/// so the result is held until the exit and carries its exit code.
struct Running<'a> {
    attempt: &'a mut StreamAttempt,
    phase: Phase,
}

enum Phase {
    /// The CLI closed its input before taking the prompt, so its output
    /// is not about this turn. Only its exit is read, until the deadline.
    Refused(tokio::time::Instant),
    Streaming,
    /// The result, held until the CLI exits or the deadline passes.
    Settling(Value, tokio::time::Instant),
    Exited,
}

impl Running<'_> {
    /// Mark the CLI exited and take the result it held, if any.
    fn exit(&mut self) -> Option<Value> {
        match std::mem::replace(&mut self.phase, Phase::Exited) {
            Phase::Settling(result, _) => Some(result),
            _ => None,
        }
    }
}

/// How long the CLI may take to exit after its result, while it stores its
/// chat, or after it refuses its prompt.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(30);

#[async_trait]
impl Channel for Running<'_> {
    type Message = Message;

    async fn next(&mut self) -> Result<Option<Message>, String> {
        use std::os::unix::process::ExitStatusExt;
        loop {
            let next = match &self.phase {
                Phase::Exited => return Ok(None),
                Phase::Refused(_) => None,
                Phase::Streaming => self.attempt.next().await?,
                Phase::Settling(_, deadline) => {
                    match tokio::time::timeout_at(*deadline, self.attempt.next()).await {
                        Ok(next) => next?,
                        Err(_) => {
                            return Ok(self.exit().map(|event| Message::Result {
                                event,
                                exit_code: None,
                            }));
                        }
                    }
                }
            };
            match next {
                Some(event)
                    if matches!(self.phase, Phase::Streaming) && event["type"] == "result" =>
                {
                    self.phase = Phase::Settling(event, tokio::time::Instant::now() + SETTLE);
                }
                Some(event) => return Ok(Some(Message::Event(event))),
                None => {
                    let exited = self.attempt.exited();
                    let (status, stderr) = match &self.phase {
                        Phase::Settling(_, deadline) | Phase::Refused(deadline) => {
                            tokio::time::timeout_at(*deadline, exited)
                                .await
                                .unwrap_or_default()
                        }
                        _ => exited.await,
                    };
                    let code = status.and_then(|s| s.code());
                    return Ok(Some(match self.exit() {
                        Some(event) => Message::Result {
                            event,
                            exit_code: code,
                        },
                        None => Message::Exit {
                            code,
                            signal: status.and_then(|s| s.signal()),
                            stderr,
                        },
                    }));
                }
            }
        }
    }

    async fn interrupt(&mut self) {
        self.attempt.interrupt().await;
    }
}

#[async_trait]
impl Adapter for CursorCli {
    type Login = Login;
    type Protocol = Tracker;

    const DEFINITION: &'static Definition = &DEFINITION;
    const PRODUCT: &'static str = "Cursor CLI";
    const PROMPT_VERSION: &'static str = PROMPT_VERSION;

    fn unavailable(&self, ready: &Ready<Login>) -> Option<String> {
        cursor_home::signed_out(ready.login.file.as_deref()).then(|| cursor_home::SIGNED_OUT.into())
    }

    fn tool_tag(&self, tools: ToolSet) -> &'static str {
        match tools {
            ToolSet::HostAndElsebot => "tools:host+elsebot-v1",
            ToolSet::Host => "tools:host",
            ToolSet::Elsebot => "tools:elsebot-v1",
            ToolSet::None => "tools:none",
        }
    }

    fn policy(&self, ready: &Ready<Login>) -> Vec<String> {
        let account = ready
            .login
            .file
            .as_deref()
            .map(cursor_home::host_account)
            .unwrap_or_default();
        vec!["agents:disabled".into(), "home:bot".into(), account]
    }

    /// Owner turns start in the bot's project; other turns in a private
    /// workspace, so nothing a guest wrote there loads.
    fn workspace(&self, dirs: &BotDirs, tools: ToolSet) -> std::io::Result<PathBuf> {
        if tools.host() {
            Ok(dirs.project.clone())
        } else {
            cursor_home::private_workspace(&dirs.provider_home)
        }
    }

    fn session_exists(&self, _: &Ready<Login>, dirs: &BotDirs, workspace: &Path, id: &str) -> bool {
        cursor_home::session_exists(&dirs.provider_home, workspace, id)
    }

    fn prepare_packet(&self, turn: &mut PreparedTurn<'_>, input: &TurnInput, dirs: &BotDirs) {
        turn.lead_once(INSTRUCTIONS, instructions(input, &dirs.project));
    }

    async fn launch(
        &self,
        ready: &Ready<Login>,
        start: Start<'_>,
    ) -> Result<Launch<Tracker>, String> {
        let Start {
            profile,
            dirs,
            workspace,
            tools,
            tool_url,
            turn,
            ..
        } = start;
        cursor_home::prepare(dirs, &ready.login, tool_url, tools.host())
            .map_err(|e| e.to_string())?;
        let mut args: Vec<OsString> = [
            "--print",
            "--output-format",
            "stream-json",
            "--stream-partial-output",
            "--trust",
            "--disable-project-configs",
            "--workspace",
            &workspace.to_string_lossy(),
            "--sandbox",
            "disabled",
            "--allowed-tools",
            &protocol::allowed_tools(tools),
            "--model",
            &profile.model,
        ]
        .iter()
        .map(OsString::from)
        .collect();
        args.extend([
            if turn.resumed() {
                "--resume"
            } else {
                "--new-session-id"
            }
            .into(),
            turn.native_id().into(),
        ]);
        Ok(Launch {
            args,
            env: cursor_home::env(&dirs.provider_home, &ready.login),
            delivery: Delivery::Stdin(turn.packet.clone()),
            protocol: Tracker::new(&profile.model, turn.native_id(), tools),
        })
    }

    async fn drive(
        &self,
        attempt: &mut StreamAttempt,
        prompt: Prompt,
        protocol: &mut Tracker,
        turn: &mut PreparedTurn<'_>,
        sink: &dyn TurnSink,
        stop: &Stop,
    ) -> Ended {
        let mut running = Running {
            attempt,
            phase: match prompt {
                Prompt::Written => Phase::Streaming,
                Prompt::Refused => Phase::Refused(tokio::time::Instant::now() + SETTLE),
            },
        };
        attempt::drive(&mut running, protocol, turn, sink, stop, DEFINITION.label).await
    }

    async fn list_models(&self, ready: &Ready<Login>) -> Result<Vec<Listed>, String> {
        // Listing runs no agent, so it reads the host login where it is.
        let mut env = vec![("NO_COLOR".to_owned(), "1".to_owned())];
        if let Some(store) = &ready.login.store {
            env.push(("AGENT_CLI_CREDENTIAL_STORE".into(), store.clone()));
        }
        let run = Run {
            env,
            timeout: super::models::ASK_TIMEOUT,
            ..Run::new(&ready.program.current().await?, &["models"])
        };
        let printed = ready
            .host
            .run(&run)
            .await
            .map(|out| String::from_utf8_lossy(&out).into_owned())
            .map_err(|e| e.to_string())
            .map_err(|why| {
                if cursor_home::refused_login(&why) {
                    cursor_home::SIGNED_OUT.to_owned()
                } else {
                    format!("`cursor-agent models` failed: {why}")
                }
            })?;
        match listed_models(&printed) {
            listed if listed.is_empty() => Err(format!(
                "`cursor-agent models` listed no models: {}",
                printed.trim()
            )),
            listed => Ok(listed),
        }
    }
}

/// The shared instructions, given in the packet: most accounts cannot
/// replace Cursor's system prompt.
fn instructions(input: &TurnInput, project: &Path) -> String {
    format!(
        "[else.bot instructions]\n{}\n\nelse.bot's tools are on the MCP server named {}. End \
         this attempt when one of them returns parked or pending; the app continues it after the \
         outcome.\n[/else.bot instructions]",
        prompt::instructions(input, &project.to_string_lossy()),
        crate::app::tools::SERVER_NAME,
    )
}

#[cfg(test)]
pub(super) mod tests;
