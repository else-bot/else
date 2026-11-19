//! The host Cursor adapter, over Cursor CLI's headless print mode
//! (`cursor-agent -p --output-format stream-json`), started in the bot's
//! project directory.
//!
//! One CLI process runs per turn, with the packet as its prompt. A new chat
//! gets an ID allocated here (`--new-session-id`); the next turn resumes it
//! (`--resume <id>`) when it is in the bot's own home and its fingerprint
//! matches. The CLI runs with the bot's provider home as `HOME` (see
//! [`super::cursor_home`]), sharing only the host's login. Its tools are
//! else.bot's MCP server, authorized by the turn's grant, plus the host
//! shell and file tools when the owner started the work; every reported
//! tool call is checked against that. Owner turns run with `--force`, which
//! runs tools without asking; turns started by an event run without it, so
//! only else.bot's tools are allowed. Cursor takes no system prompt from
//! most accounts, so else.bot's instructions lead the packet whenever the
//! chat has not seen this exact text. Stop is SIGINT. The CLI takes no
//! input while it runs, so owner messages that join a turn wait for its
//! next attempt.

mod protocol;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Value, json};
use uuid::Uuid;

use super::cli::attempt::{self, Channel};
use super::cli::files::GRANT_ENV;
use super::cli::session::{self, Identity, Opened, PreparedTurn};
use super::cli::stream::{Interrupt, StreamAttempt};
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
/// Linux limits one argument to 32 pages; the packet is one argument.
const MAX_PROMPT: usize = 128 * 1024 - 1;

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
fn lookup(configured: Option<&Path>, home: Option<&Path>) -> Result<Option<PathBuf>, String> {
    if let Some(given) = configured {
        let resolved = super::executable(Some(given), "cursor-agent")?;
        let linked = std::fs::canonicalize(given).ok() == resolved;
        return Ok(if linked {
            Some(given.to_owned())
        } else {
            resolved
        });
    }
    match home.map(|home| home.join(cursor_home::INSTALLED)) {
        Some(installed) if installed.is_file() => Ok(Some(installed)),
        _ => super::executable(None, "cursor-agent"),
    }
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
    program: PathBuf,
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
            ready: Self::ready(host, Ok(program), login),
            tool_url: None,
        }
    }

    fn ready(
        host: Option<Host>,
        program: Result<Option<PathBuf>, String>,
        login: Login,
    ) -> Result<Ready, String> {
        let host = host.ok_or("Cursor CLI is not configured on this host.")?;
        let program = program?.ok_or(cursor_home::NOT_INSTALLED)?;
        let version = host.probe(&program, &["--version"], &[]).map_err(|why| {
            format!(
                "Cursor CLI does not run (`{} --version`: {why}).",
                program.display()
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
        dirs: BotDirs,
        turn: &PreparedTurn<'_>,
    ) -> LaunchSpec {
        let mut args: Vec<OsString> = [
            "--print",
            "--output-format",
            "stream-json",
            "--stream-partial-output",
            "--trust",
            "--sandbox",
            "disabled",
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
        if input.delegation.host_tools() {
            args.push("--force".into());
        }
        // A message may start with `-`; it is the prompt, not an option.
        args.extend(["--".into(), turn.packet.clone().into()]);
        let mut env = cursor_home::env(&dirs.provider_home, &ready.login);
        if let (Some(_), Some(grant)) = (&self.tool_url, &input.tool_grant) {
            env.push((GRANT_ENV.into(), grant.clone()));
        }
        LaunchSpec {
            turn_id: input.turn_id,
            dirs,
            program: ready.program.clone(),
            args,
            env,
        }
    }
}

/// The attempt, with its exit reported as a final `exit` event: Cursor
/// writes no result after an error or SIGINT.
struct Running<'a> {
    attempt: &'a mut StreamAttempt,
    exited: bool,
}

#[async_trait]
impl Channel for Running<'_> {
    type Message = Value;

    async fn next(&mut self) -> Result<Option<Value>, String> {
        match self.attempt.next().await? {
            None if !self.exited => {
                use std::os::unix::process::ExitStatusExt;
                let (status, stderr) = self.attempt.exited().await;
                self.exited = true;
                Ok(Some(json!({
                    "type": "exit",
                    "code": status.and_then(|s| s.code()),
                    "signal": status.and_then(|s| s.signal()),
                    "stderr": stderr,
                })))
            }
            next => Ok(next),
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
        let (host, program) = (ready.host.clone(), ready.program.clone());
        let printed = tokio::task::spawn_blocking(move || {
            host.probe(&program, &["models"], &[("NO_COLOR".into(), "1".into())])
        })
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
        if let Some(hooks) = cursor_home::project_hooks(&dirs.project) {
            return failed(
                "provider_refused",
                format!(
                    "Cursor would run the hooks in {} on this server, outside else.bot's tool \
                     policy. Remove them from the bot's project, or switch this bot to another \
                     model.",
                    hooks.display()
                ),
            );
        }
        let fingerprint = self.fingerprint(ready, &input, profile);
        let opened = match input.session.as_ref().filter(|s| {
            s.fingerprint == fingerprint
                && cursor_home::session_exists(&dirs.provider_home, &dirs.project, &s.native_id)
        }) {
            Some(session) => Opened::Resumed(session),
            None => Opened::Fresh(Uuid::new_v4().to_string()),
        };
        let mut turn = PreparedTurn::new(&input, fingerprint, opened);
        turn.lead_once(INSTRUCTIONS, instructions(&input, &dirs.project));
        if turn.packet.len() > MAX_PROMPT {
            return failed(
                "provider_refused",
                format!(
                    "This turn's message is {} KiB, more than the {} KiB Cursor CLI takes as \
                     its prompt. Start a new conversation, or switch this bot to another model.",
                    turn.packet.len() / 1024,
                    (MAX_PROMPT + 1) / 1024,
                ),
            );
        }
        if turn.record(sink).await.is_err() {
            return TurnOutcome::Interrupted;
        }
        if let Err(e) = attachments::stage(&dirs.project, &input.files).await {
            return failed("attachment_unavailable", e.to_string());
        }
        let host_tools = input.delegation.host_tools();
        if let Err(e) =
            cursor_home::prepare(&dirs, &ready.login, self.tool_url.as_deref(), host_tools)
        {
            return failed("provider_unavailable", e.to_string());
        }
        let spec = self.launch(ready, &input, profile, dirs, &turn);
        if sink.sending().await.is_err() {
            return TurnOutcome::Interrupted;
        }
        let mut attempt =
            match StreamAttempt::spawn(&ready.host, &spec, Interrupt::Signal, DEFINITION.label) {
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
            exited: false,
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
