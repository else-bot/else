//! The host Grok adapter, over Grok CLI's headless print mode, started in
//! the bot's project directory.
//!
//! One CLI process runs per turn, with the packet as its prompt file. A new
//! native session gets an ID allocated here (`--session-id`); the next turn
//! resumes it explicitly (`--resume <id>`) when its transcript is in the
//! bot's own `GROK_HOME` under its project and its fingerprint matches.
//! Tools are only those named with `--tools` and verified from the
//! inventory Grok reports: else.bot's MCP tools (through Grok's
//! `search_tool` and `use_tool`, or by their `elsebot__` names), plus the
//! host shell and file tools unless an incoming event started the work.
//! Interruption is SIGINT, which Grok answers with a `cancelled` result.

mod protocol;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::cli::attempt::Ended;
use super::cli::session::PreparedTurn;
use super::cli::stream::{Prompt, StreamAttempt};
use super::cli::turn::{self, Adapter, Cli, Delivery, Launch, Ready, Start, Stream, ToolSet};
use super::definitions::Definition;
use super::host::{BotDirs, Host, Run};
use super::models::Listed;
use super::{grok_home, prompt};
use crate::app::providers::{FailureCode, Stop, TurnInput, TurnOutcome, TurnSink};
use protocol::Tracker;

pub const ID: &str = "grok";
const PROMPT_VERSION: &str = "else-grok-v4";
const SIGNED_OUT: &str = "Grok isn't signed in on this server. Run `grok login --device-code` there, \
                          or switch this bot to another model.";

pub static DEFINITION: Definition = Definition {
    id: ID,
    label: "Grok",
    fallback: &["grok-4.7", "grok-4.7-build-fast", "grok-4.6"],
    efforts: &["low", "medium", "high", "xhigh"],
    tiers: &["default"],
    pinned: super::definitions::any_id,
    model_note: "",
    effort_note: "",
    tier_note: " Choose grok-4.7-build-fast for its fast model.",
    build,
};

fn build<'w>(wiring: &'w super::Wiring<'_>) -> futures_util::future::BoxFuture<'w, super::Built> {
    let (host, bin) = (wiring.host.clone(), wiring.config.grok_bin.clone());
    let probe: turn::Probe<PathBuf> = Box::new(move || {
        let (host, bin) = (host.clone(), bin.clone());
        Box::pin(async move {
            let home = grok_home::host_home();
            let program = lookup(bin.as_deref(), home.as_deref()).await;
            ready(Some(host), program, home).await
        })
    });
    Box::pin(async move {
        let grok = Grok::probed(GrokCli, probe)
            .await
            .with_tools(wiring.tool_url.clone());
        super::Built {
            provider: std::sync::Arc::new(grok),
            summarizer: None,
        }
    })
}

/// The models `grok models` prints under "Available models:", one per
/// `* id (default)` or `- id` line. Grok names no efforts per model. Signed
/// out, it prints only the models it offers without an account.
fn listed_models(printed: &str) -> Result<Vec<Listed>, String> {
    if printed.contains("You are not authenticated") {
        return Err(SIGNED_OUT.into());
    }
    Ok(printed
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("Available models"))
        .filter_map(|line| {
            let rest = line.trim().strip_prefix(['*', '-'])?;
            let id = rest.split_whitespace().next()?;
            Some(Listed::id(id))
        })
        .collect())
}

/// The binary Grok's installers keep in the host's `GROK_HOME` (curl's
/// install and the npm package's launcher both run it). The link is kept,
/// not resolved, so it follows Grok's own updates.
fn installed(home: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = home.join("bin").join("grok");
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .then_some(path)
}

/// `ELSE_GROK_BIN` or `PATH`, under the same rule as [`choose`]: the
/// installer writes `ELSE_GROK_BIN` from its own `PATH`, so it can name the
/// npm package's script too. A setting that does not resolve stays an error.
async fn lookup(
    configured: Option<&Path>,
    home: Option<&Path>,
) -> Result<Option<super::Program>, String> {
    let found = super::executable(configured, "grok").await;
    match configured {
        Some(_) if found.is_err() => found,
        _ => choose(found, home.and_then(installed)),
    }
}

/// A native `grok` from `PATH`, else the installed binary when there is one.
/// `PATH` can hold the npm package's script, which needs node and would
/// unpack its own copy into each bot's `GROK_HOME`.
fn choose(
    on_path: Result<Option<super::Program>, String>,
    installed: Option<PathBuf>,
) -> Result<Option<super::Program>, String> {
    match on_path {
        Ok(Some(found)) if !is_script(found.path()) => Ok(Some(found)),
        other => installed.map_or(other, |path| Ok(Some(path.into()))),
    }
}

fn is_script(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0; 2];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok_and(|()| &head == b"#!")
}

pub struct GrokCli;

pub type Grok = Cli<GrokCli>;

impl Grok {
    pub async fn detect(
        host: Option<Host>,
        program: Option<PathBuf>,
        login: Option<PathBuf>,
    ) -> Self {
        Self::probed(
            GrokCli,
            Box::new(move || {
                let (host, program, login) = (host.clone(), program.clone(), login.clone());
                Box::pin(ready(host, Ok(program.map(Into::into)), login))
            }),
        )
        .await
    }
}

/// A CLI that runs, and the host's Grok home, which holds its login.
async fn ready(
    host: Option<Host>,
    program: Result<Option<super::Program>, String>,
    login: Option<PathBuf>,
) -> Result<Ready<PathBuf>, String> {
    let host = host.ok_or("Grok CLI is not configured on this host.")?;
    let program = program?.ok_or("Grok CLI (`grok`) was not found on PATH.")?;
    let version = turn::version(&host, &program, &[], GrokCli::PRODUCT).await?;
    let login = login.ok_or("Grok CLI's home directory is unknown on the host.")?;
    Ok(Ready {
        host,
        program,
        version,
        login,
    })
}

#[async_trait]
impl Adapter for GrokCli {
    type Login = PathBuf;

    const DEFINITION: &'static Definition = &DEFINITION;
    const PRODUCT: &'static str = "Grok CLI";
    const PROMPT_VERSION: &'static str = PROMPT_VERSION;

    fn unavailable(&self, ready: &Ready<PathBuf>) -> Option<String> {
        grok_home::signed_out(&ready.login).then(|| SIGNED_OUT.into())
    }

    fn tool_tag(&self, tools: ToolSet) -> &'static str {
        match tools {
            ToolSet::HostAndElsebot => "tools:host+elsebot-v1",
            ToolSet::Host => "tools:host",
            ToolSet::Elsebot => "tools:elsebot-v1",
            ToolSet::None => "tools:none",
        }
    }

    async fn list_models(&self, ready: &Ready<PathBuf>) -> Result<Vec<Listed>, String> {
        let run = Run {
            timeout: super::models::ASK_TIMEOUT,
            ..Run::new(&ready.program.current().await?, &["models"])
        };
        let printed = ready
            .host
            .run(&run)
            .await
            .map_err(|why| format!("`grok models` failed: {why}"))?;
        let printed = String::from_utf8(printed).map_err(|_| "`grok models` printed non-UTF-8")?;
        listed_models(&printed)
    }
}

#[async_trait]
impl Stream for GrokCli {
    type Protocol = Tracker;

    fn policy(&self, ready: &Ready<PathBuf>) -> Vec<String> {
        vec![
            "agents:disabled".into(),
            grok_home::host_account(&ready.login),
        ]
    }

    fn session_exists(
        &self,
        _: &Ready<PathBuf>,
        dirs: &BotDirs,
        workspace: &Path,
        id: &str,
    ) -> bool {
        grok_home::session_exists(&dirs.provider_home, workspace, id)
    }

    async fn launch(
        &self,
        ready: &Ready<PathBuf>,
        start: Start<'_>,
    ) -> Result<Launch<Tracker>, String> {
        let Start {
            input,
            profile,
            dirs,
            tools,
            tool_url,
            turn,
            ..
        } = start;
        grok_home::prepare(
            dirs,
            &ready.login,
            &profile.model,
            tool_url,
            &turn.packet,
            tools.host(),
        )
        .map_err(|e| e.to_string())?;
        let system = format!(
            "{}\n{}",
            prompt::instructions(input, &dirs.project.to_string_lossy()),
            tool_note(),
        );
        let mut offered = grok_home::MCP_TOOLS.to_vec();
        if tools.host() {
            offered.extend(grok_home::HOST_TOOLS.iter().map(|(flag, _)| *flag));
        }
        let mut args: Vec<OsString> = [
            "--output-format",
            "streaming-messages-json",
            "--include-partial-messages",
            "--model",
            &profile.model,
            "--reasoning-effort",
            &profile.reasoning_effort,
            "--tools",
            &offered.join(","),
            "--no-subagents",
            "--no-plan",
            "--disable-web-search",
            "--system-prompt-override",
            &system,
        ]
        .iter()
        .map(OsString::from)
        .collect();
        args.extend([
            "--prompt-file".into(),
            dirs.provider_home
                .join(grok_home::PROMPT_FILE)
                .into_os_string(),
            if turn.resumed() {
                "--resume"
            } else {
                "--session-id"
            }
            .into(),
            turn.native_id().into(),
        ]);
        if tools.host() {
            args.push("--always-approve".into());
        } else {
            args.extend(["--permission-mode".into(), "dontAsk".into()]);
        }
        Ok(Launch {
            args,
            env: grok_home::env(&dirs.provider_home),
            delivery: Delivery::Launch,
            protocol: Tracker::new(&profile.model, turn.native_id(), tools),
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

    /// Grok clears the host login in the run it fails for want of one,
    /// before it acknowledges the prompt, so nothing of the turn ran.
    fn concluded(
        &self,
        ready: &Ready<PathBuf>,
        input: &TurnInput,
        accepted: bool,
        outcome: TurnOutcome,
    ) -> TurnOutcome {
        let unacknowledged = !accepted
            && matches!(
                outcome,
                TurnOutcome::Failed {
                    code: FailureCode::ProviderFailed,
                    ..
                } | TurnOutcome::Uncertain { .. }
            );
        if unacknowledged && grok_home::signed_out(&ready.login) {
            tracing::info!(turn = %input.turn_id, ?outcome, "Grok failed for want of a login");
            return TurnOutcome::failed(FailureCode::ProviderUnavailable, SIGNED_OUT);
        }
        outcome
    }
}

/// How Grok reaches else.bot's tools, appended to the shared instructions.
pub(crate) fn tool_note() -> String {
    format!(
        "Use inline use_tool calls with tool_name and tool_input. else.bot MCP tool names start with {0}__. A tool that {0}__find_tool returns, such as send_draft or report_draft_send, has no {0}__ name of its own: run it with tool_name {0}__use_tool and tool_input {{\"name\": \"send_draft\", \"arguments\": {{...}}}}. Do not use invocation files that hide tool_name. End this attempt when an else.bot tool returns parked or pending; the app will continue it after the outcome.",
        crate::app::tools::SERVER_NAME,
    )
}

#[cfg(test)]
pub(super) mod tests;
