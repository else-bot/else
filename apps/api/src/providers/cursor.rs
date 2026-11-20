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
use serde_json::{Value, json};
use uuid::Uuid;

use super::cli::attempt::{self, Channel};
use super::cli::files::GRANT_ENV;
use super::cli::session::{self, Identity, Opened, PreparedTurn};
use super::cli::stream::{Prompt, StreamAttempt};
use super::cursor_home::{self, Login};
use super::definitions::Definition;
use super::host::{BotDirs, Host, LaunchSpec};
use super::models::{Listed, Models};
use super::{attachments, prompt};
use crate::app::providers::{
    Availability, Profile, Provider, Stop, TurnInput, TurnOutcome, TurnSink,
};
use protocol::Tracker;

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

fn build(wiring: &super::Wiring) -> super::Built {
    let program = lookup(
        wiring.config.cursor_bin.as_deref(),
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
    );
    let cursor = Cursor {
        models: Models::new(&DEFINITION),
        ready: Cursor::ready(Some(wiring.host.clone()), program, Login::host()),
        tool_url: Some(wiring.tool_url.clone()),
    };
    super::Built {
        provider: std::sync::Arc::new(cursor),
        summarizer: None,
    }
}

/// `ELSE_CURSOR_BIN`, else the link Cursor's installer keeps in
/// `~/.local/bin`, which a service's `PATH` often lacks, else
/// `cursor-agent` on `PATH`. A link is kept, not resolved: an update points
/// it at a new versioned directory and removes the old one.
fn lookup(
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
    super::executable_as(Some(&given), "cursor-agent", "ELSE_CURSOR_BIN")
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

/// A CLI that runs and is new enough, and how it finds the host's login.
struct Ready {
    host: Host,
    program: super::Program,
    version: String,
    login: Login,
}

pub struct Cursor {
    models: Models,
    ready: Result<Ready, String>,
    tool_url: Option<String>,
}

impl Cursor {
    pub fn detect(host: Option<Host>, program: Option<PathBuf>, login: Login) -> Self {
        Self {
            models: Models::new(&DEFINITION),
            ready: Self::ready(host, Ok(program.map(Into::into)), login),
            tool_url: None,
        }
    }

    fn ready(
        host: Option<Host>,
        program: Result<Option<super::Program>, String>,
        login: Login,
    ) -> Result<Ready, String> {
        let host = host.ok_or("Cursor CLI is not configured on this host.")?;
        let program = program?.ok_or(cursor_home::NOT_INSTALLED)?;
        let version = host
            .probe(program.path(), &["--version"], &[])
            .map_err(|why| {
                format!(
                    "Cursor CLI does not run (`{} --version`: {why}).",
                    program.path().display()
                )
            })?;
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

    pub fn with_tools(mut self, url: String) -> Self {
        self.tool_url = Some(url);
        self
    }

    fn fingerprint(&self, ready: &Ready, input: &TurnInput, profile: &Profile) -> String {
        let account = ready
            .login
            .file
            .as_deref()
            .map(cursor_home::host_account)
            .unwrap_or_default();
        session::fingerprint(
            input,
            profile,
            &Identity {
                prompt_version: PROMPT_VERSION,
                version: &ready.version,
                runtime: &ready.host.runtime(input.bot_id),
                tools: match (input.delegation.host_tools(), self.tool_url.is_some()) {
                    (true, true) => "tools:host+elsebot-v1",
                    (true, false) => "tools:host",
                    (false, true) => "tools:elsebot-v1",
                    (false, false) => "tools:none",
                },
                policy: &["agents:disabled", "home:bot", &account],
            },
        )
    }

    fn launch(
        &self,
        ready: &Ready,
        input: &TurnInput,
        profile: &Profile,
        mut dirs: BotDirs,
        workspace: PathBuf,
        turn: &PreparedTurn<'_>,
    ) -> Result<LaunchSpec, String> {
        let program = ready
            .program
            .current()
            .map_err(|why| format!("Could not start Cursor CLI: {why}"))?;
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
            &protocol::allowed_tools(input.delegation.host_tools(), self.tool_url.is_some()),
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
        dirs.project = workspace;
        let mut env = cursor_home::env(&dirs.provider_home, &ready.login);
        if let (Some(_), Some(grant)) = (&self.tool_url, &input.tool_grant) {
            env.push((GRANT_ENV.into(), grant.clone()));
        }
        Ok(LaunchSpec {
            turn_id: input.turn_id,
            dirs,
            program,
            args,
            env,
        })
    }
}

/// The attempt, reporting how the CLI exited: Cursor writes no result
/// after an error or SIGINT, and writes its result before storing the chat,
/// so the result is held until the exit and carries its `exit_code`.
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
    type Message = Value;

    async fn next(&mut self) -> Result<Option<Value>, String> {
        use std::os::unix::process::ExitStatusExt;
        loop {
            let next = match &self.phase {
                Phase::Exited => return Ok(None),
                Phase::Refused(_) => None,
                Phase::Streaming => self.attempt.next().await?,
                Phase::Settling(_, deadline) => {
                    match tokio::time::timeout_at(*deadline, self.attempt.next()).await {
                        Ok(next) => next?,
                        Err(_) => return Ok(self.exit()),
                    }
                }
            };
            match next {
                Some(event)
                    if matches!(self.phase, Phase::Streaming) && event["type"] == "result" =>
                {
                    self.phase = Phase::Settling(event, tokio::time::Instant::now() + SETTLE);
                }
                Some(event) => return Ok(Some(event)),
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
                        Some(mut result) => {
                            result["exit_code"] = json!(code);
                            result
                        }
                        None => json!({
                            "type": "exit",
                            "code": code,
                            "signal": status.and_then(|s| s.signal()),
                            "stderr": stderr,
                        }),
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
impl Provider for Cursor {
    fn id(&self) -> &'static str {
        ID
    }
    fn label(&self) -> &'static str {
        DEFINITION.label
    }
    fn project_dir(&self, bot_id: Uuid) -> Option<PathBuf> {
        self.ready
            .as_ref()
            .ok()
            .map(|r| r.host.project_path(bot_id))
    }
    fn availability(&self) -> Availability {
        match &self.ready {
            Ok(ready) if cursor_home::signed_out(ready.login.file.as_deref()) => {
                Availability::Unavailable(cursor_home::SIGNED_OUT.into())
            }
            Ok(_) => Availability::Available,
            Err(reason) => Availability::Unavailable(reason.clone()),
        }
    }
    fn uses_profile(&self) -> bool {
        true
    }
    fn validate_profile(&self, profile: &Profile) -> Result<(), String> {
        self.models.validate(profile)
    }
    fn models(&self) -> Option<&Models> {
        Some(&self.models)
    }
    async fn list_models(&self) -> Result<Vec<Listed>, String> {
        let ready = self.ready.as_ref().map_err(Clone::clone)?;
        // Listing runs no agent, so it reads the host login where it is.
        // `probe` keeps the line that says why it failed.
        let (host, program) = (ready.host.clone(), ready.program.current()?);
        let mut env = vec![("NO_COLOR".to_owned(), "1".to_owned())];
        if let Some(store) = &ready.login.store {
            env.push(("AGENT_CLI_CREDENTIAL_STORE".into(), store.clone()));
        }
        let printed = tokio::task::spawn_blocking(move || host.probe(&program, &["models"], &env))
            .await
            .map_err(|e| e.to_string())?
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

    async fn run(&self, input: TurnInput, sink: &dyn TurnSink, stop: Stop) -> TurnOutcome {
        let ready = match &self.ready {
            Ok(ready) => ready,
            Err(reason) => return failed("provider_unavailable", reason.clone()),
        };
        if cursor_home::signed_out(ready.login.file.as_deref()) {
            return failed("provider_unavailable", cursor_home::SIGNED_OUT);
        }
        let Some(profile) = &input.profile else {
            return failed(
                "provider_misconfigured",
                "No execution profile was snapshotted.",
            );
        };
        if let Err(reason) = self.models.validate(profile) {
            return failed("invalid_profile", reason);
        }
        if stop.is_requested() {
            return TurnOutcome::Interrupted;
        }
        let dirs = match ready.host.bot_dirs(input.bot_id, ID) {
            Ok(dirs) => dirs,
            Err(e) => return failed("provider_unavailable", e.to_string()),
        };
        let host_tools = input.delegation.host_tools();
        let workspace = if host_tools {
            dirs.project.clone()
        } else {
            match cursor_home::private_workspace(&dirs.provider_home) {
                Ok(workspace) => workspace,
                Err(e) => return failed("provider_unavailable", e.to_string()),
            }
        };
        let fingerprint = self.fingerprint(ready, &input, profile);
        let opened = match input.session.as_ref().filter(|s| {
            s.fingerprint == fingerprint
                && cursor_home::session_exists(&dirs.provider_home, &workspace, &s.native_id)
        }) {
            Some(session) => Opened::Resumed(session),
            None => Opened::Fresh(Uuid::new_v4().to_string()),
        };
        let mut turn = PreparedTurn::new(&input, fingerprint, opened);
        turn.lead_once(INSTRUCTIONS, instructions(&input, &dirs.project));
        if turn.record(sink).await.is_err() {
            return TurnOutcome::Interrupted;
        }
        if let Err(e) = attachments::stage(&dirs.project, &input.files).await {
            return failed("attachment_unavailable", e.to_string());
        }
        if let Err(e) =
            cursor_home::prepare(&dirs, &ready.login, self.tool_url.as_deref(), host_tools)
        {
            return failed("provider_unavailable", e.to_string());
        }
        let spec = match self.launch(ready, &input, profile, dirs, workspace, &turn) {
            Ok(spec) => spec,
            Err(why) => return failed("provider_unavailable", why),
        };
        if sink.sending().await.is_err() {
            return TurnOutcome::Interrupted;
        }
        let (mut attempt, prompt) = match StreamAttempt::spawn_with_prompt(
            &ready.host,
            &spec,
            &turn.packet,
            DEFINITION.label,
        )
        .await
        {
            Ok(attempt) => attempt,
            Err(e) => return failed("provider_unavailable", e.to_string()),
        };
        let mut tracker = Tracker::new(
            &profile.model,
            turn.native_id(),
            host_tools,
            self.tool_url.is_some(),
        );
        let mut running = Running {
            attempt: &mut attempt,
            phase: match prompt {
                Prompt::Written => Phase::Streaming,
                Prompt::Refused => Phase::Refused(tokio::time::Instant::now() + SETTLE),
            },
        };
        let ended = attempt::drive(
            &mut running,
            &mut tracker,
            &mut turn,
            sink,
            &stop,
            DEFINITION.label,
        )
        .await;
        attempt.conclude(ended, sink).await
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

fn failed(code: &str, message: impl Into<String>) -> TurnOutcome {
    TurnOutcome::Failed {
        code: code.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests;
