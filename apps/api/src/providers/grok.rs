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
use uuid::Uuid;

use super::cli::files::GRANT_ENV;
use super::cli::session::{self, Identity, Opened, PreparedTurn};
use super::cli::stream::{Interrupt, StreamAttempt};
use super::definitions::Definition;
use super::host::{BotDirs, Host, LaunchSpec};
use super::{attachments, grok_home, prompt};
use crate::app::providers::{
    Availability, Catalog, Profile, Provider, Stop, TurnInput, TurnOutcome, TurnSink,
};
use protocol::Tracker;

pub const ID: &str = "grok";
const PROMPT_VERSION: &str = "else-grok-v2";

pub static DEFINITION: Definition = Definition {
    id: ID,
    label: "Grok",
    catalog: Catalog {
        models: &["grok-4.7", "grok-4.7-build-fast", "grok-4.6"],
        featured_models: &[],
        efforts: &["low", "medium", "high", "xhigh"],
        tiers: &["default"],
    },
    model_note: "",
    effort_note: "",
    tier_note: " Choose grok-4.7-build-fast for its fast model.",
    build,
};

fn build(wiring: &super::Wiring) -> super::Built {
    let home = grok_home::host_home();
    let configured = configured_program(wiring.config.grok_bin.as_deref(), home.as_deref());
    let grok = Grok {
        ready: Grok::ready(
            Some(wiring.host.clone()),
            super::executable(configured.as_deref(), "grok"),
            home,
        ),
        tool_url: None,
    }
    .with_tools(wiring.tool_url.clone());
    super::Built {
        provider: std::sync::Arc::new(grok),
        summarizer: None,
    }
}

/// `ELSE_GROK_BIN`, else the binary Grok's installers keep in the host's
/// `GROK_HOME` (curl's install and the npm package's launcher both run it),
/// else `None` to search `PATH`. The `grok` on `PATH` can be a launcher that
/// needs node and would unpack its own copy into each bot's `GROK_HOME`.
fn configured_program(setting: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    setting.map(Path::to_path_buf).or_else(|| {
        home.map(|home| home.join("bin").join("grok"))
            .filter(|path| {
                use std::os::unix::fs::PermissionsExt;
                path.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    })
}

/// A CLI that runs and a host login.
struct Ready {
    host: Host,
    program: PathBuf,
    version: String,
    login: PathBuf,
}

pub struct Grok {
    ready: Result<Ready, String>,
    tool_url: Option<String>,
}

impl Grok {
    pub fn detect(host: Option<Host>, program: Option<PathBuf>, login: Option<PathBuf>) -> Self {
        Self {
            ready: Self::ready(host, Ok(program), login),
            tool_url: None,
        }
    }

    fn ready(
        host: Option<Host>,
        program: Result<Option<PathBuf>, String>,
        login: Option<PathBuf>,
    ) -> Result<Ready, String> {
        let host = host.ok_or("Grok CLI is not configured on this host.")?;
        let program = program?.ok_or("Grok CLI (`grok`) was not found on PATH.")?;
        let version = host.probe(&program, &["--version"], &[]).map_err(|why| {
            format!(
                "Grok CLI does not run (`{} --version`: {why}).",
                program.display()
            )
        })?;
        let login = login
            .filter(|dir| dir.join("auth.json").is_file())
            .ok_or("Grok CLI is not logged in on the host (run `grok login`).")?;
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
        let account = grok_home::host_account(&ready.login);
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
                policy: &["agents:disabled", &account],
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
        let host_tools = input.delegation.host_tools();
        let system = format!(
            "{}\nUse inline use_tool calls with tool_name and tool_input. else.bot MCP tool names start with {}__. Do not use invocation files that hide tool_name. End this attempt when an else.bot tool returns parked or pending; the app will continue it after the outcome.",
            prompt::instructions(input, &dirs.project.to_string_lossy()),
            crate::app::tools::SERVER_NAME,
        );
        let mut tools = grok_home::MCP_TOOLS.to_vec();
        if host_tools {
            tools.extend(grok_home::HOST_TOOLS.iter().map(|(flag, _)| *flag));
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
            &tools.join(","),
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
        if host_tools {
            args.push("--always-approve".into());
        } else {
            args.extend(["--permission-mode".into(), "dontAsk".into()]);
        }
        let mut env = grok_home::env(&dirs.provider_home);
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

#[async_trait]
impl Provider for Grok {
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

    async fn run(&self, input: TurnInput, sink: &dyn TurnSink, stop: Stop) -> TurnOutcome {
        let ready = match &self.ready {
            Ok(ready) => ready,
            Err(reason) => return failed("provider_unavailable", reason.clone()),
        };
        let Some(profile) = &input.profile else {
            return failed(
                "provider_misconfigured",
                "No execution profile was snapshotted.",
            );
        };
        if let Err(reason) = DEFINITION.validate(profile) {
            return failed("invalid_profile", reason);
        }
        if stop.is_requested() {
            return TurnOutcome::Interrupted;
        }
        let dirs = match ready.host.bot_dirs(input.bot_id, ID) {
            Ok(dirs) => dirs,
            Err(e) => return failed("provider_unavailable", e.to_string()),
        };
        let fingerprint = self.fingerprint(ready, &input, profile);
        let opened = match input.session.as_ref().filter(|s| {
            s.fingerprint == fingerprint
                && grok_home::session_exists(&dirs.provider_home, &dirs.project, &s.native_id)
        }) {
            Some(session) => Opened::Resumed(session),
            None => Opened::Fresh(Uuid::new_v4().to_string()),
        };
        let mut turn = PreparedTurn::new(&input, fingerprint, opened);
        if turn.record(sink).await.is_err() {
            return TurnOutcome::Interrupted;
        }
        if let Err(e) = attachments::stage(&dirs.project, &input.files).await {
            return failed("attachment_unavailable", e.to_string());
        }
        let host_tools = input.delegation.host_tools();
        if let Err(e) = grok_home::prepare(
            &dirs,
            &ready.login,
            self.tool_url.as_deref(),
            &turn.packet,
            host_tools,
        ) {
            return failed("provider_unavailable", e.to_string());
        }
        let spec = self.launch(ready, &input, profile, dirs, &turn);
        if sink.sending().await.is_err() {
            return TurnOutcome::Interrupted;
        }
        let mut attempt = match StreamAttempt::spawn(&ready.host, &spec, Interrupt::Signal, "Grok")
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
        let ended = attempt.drive(&mut tracker, &mut turn, sink, &stop).await;
        attempt.conclude(ended, sink).await
    }
}

fn failed(code: &str, message: impl Into<String>) -> TurnOutcome {
    TurnOutcome::Failed {
        code: code.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests;
