//! The host Codex adapter, over `codex app-server` on the host, started in
//! the bot's project directory.
//!
//! One app-server runs per turn and exits afterwards; the native
//! thread persists in the bot's `CODEX_HOME` and is resumed next time when
//! its compatibility fingerprint (runtime, prompt version, tool policy,
//! profile, native-agent policy) still matches. Otherwise a new thread is
//! started and the conversation is rebuilt from SQLite. The model,
//! reasoning effort, and service tier are sent explicitly on thread
//! creation, resume, and turn start, and the values the server reports back
//! must match: no silent substitution. Completion comes only from
//! `turn/completed`; text alone never implies success. Reasoning is never
//! stored. A turn's tools are Codex's shell and file tools with full host
//! access, never asking for approval, and else.bot's own (memory, delegation,
//! and the bot's computer, over MCP, authorized by a per-turn task grant).
//! Work an incoming connector event started gets no host tools: the shell
//! is off, the sandbox is read-only, Codex runs on a model catalog without
//! native tools (see `codex_home`), and any command or file change item
//! still ends the turn as a policy failure, so it reaches only the else.bot
//! tools its grant names.
//! Any other tool, web, or native-agent item, and any approval or tool
//! request, is refused and ends the turn as a policy failure. A memory
//! summary gets no tools at all. Only skills installed in the bot's project
//! are enabled; owner, ancestor-repository, and bundled skills are excluded.

mod policy;
mod skills;
mod summary;
mod turn;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::cli::files::GRANT_ENV;
use super::cli::session::{self, Identity};
use super::codex_home;
use super::codex_rpc::AppServer;
use super::definitions::Definition;
use super::host::{Host, LaunchSpec};
use super::prompt;
use crate::app::providers::{
    Availability, Catalog, CheckpointRequest, Profile, Provider, Stop, TurnInput, TurnOutcome,
    TurnSink,
};

pub const ID: &str = "codex";
const PROMPT_VERSION: &str = "else-codex-v19";
const RPC_TIMEOUT: Duration = Duration::from_secs(60);
const MODELS: &[&str] = &[
    "gpt-6.1-sol",
    "gpt-6-astra",
    "gpt-6.1-terra",
    "gpt-6-luna",
    "gpt-6-sol",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
];

pub static DEFINITION: Definition = Definition {
    id: ID,
    label: "Codex",
    catalog: Catalog {
        models: MODELS,
        featured_models: MODELS.split_at(4).0,
        efforts: &["low", "medium", "high", "xhigh", "max"],
        tiers: &["default", "priority"],
    },
    model_note: "",
    effort_note: " Ultra delegates to native sub-agents, which are disabled.",
    tier_note: "",
    build,
};

/// The one Codex is both a provider and the memory summarizer.
fn build(wiring: &super::Wiring) -> super::Built {
    let auth = codex_home::host_auth_file(
        std::env::var("CODEX_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    );
    let codex = std::sync::Arc::new(
        Codex {
            ready: Codex::ready(
                Some(wiring.host.clone()),
                super::executable(wiring.config.codex_bin.as_deref(), "codex"),
                auth,
            ),
            tool_url: None,
        }
        .with_tools(wiring.tool_url.clone()),
    );
    super::Built {
        provider: codex.clone(),
        summarizer: Some(codex),
    }
}

/// A CLI that runs and a host login.
struct Ready {
    host: Host,
    program: PathBuf,
    auth: PathBuf,
    version: String,
}

pub struct Codex {
    ready: Result<Ready, String>,
    tool_url: Option<String>,
}

impl Codex {
    /// Locate the CLI and the host login and check that the CLI runs. A
    /// missing piece makes the provider unavailable with a reason instead of
    /// failing startup. The version is only session identity.
    pub fn detect(host: Option<Host>, program: Option<PathBuf>, auth: Option<PathBuf>) -> Self {
        Self {
            ready: Self::ready(host, Ok(program), auth),
            tool_url: None,
        }
    }

    fn ready(
        host: Option<Host>,
        program: Result<Option<PathBuf>, String>,
        auth: Option<PathBuf>,
    ) -> Result<Ready, String> {
        let host = host.ok_or("Codex is not configured on this host")?;
        let program = program?.ok_or("the codex CLI was not found on the host")?;
        let auth = auth.ok_or("the Codex login location is unknown")?;
        if !auth.is_file() {
            return Err("Codex is not logged in on the host (run `codex login`)".into());
        }
        let version = host.probe(&program, &["--version"], &[]).ok_or(
            "the codex executable does not run (`codex --version` failed); \
             set ELSE_CODEX_BIN to the native codex binary",
        )?;
        Ok(Ready {
            host,
            program,
            auth,
            version,
        })
    }

    /// Offer else.bot's tools (memory) at this MCP URL.
    pub fn with_tools(mut self, url: String) -> Self {
        self.tool_url = Some(url);
        self
    }

    fn fingerprint(
        &self,
        ready: &Ready,
        profile: &Profile,
        input: &TurnInput,
        account: &str,
    ) -> String {
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
                    (false, true) => "tools:elsebot-only-v3",
                    (false, false) => "tools:none",
                },
                policy: &["agents:disabled", "skills:bot-project-only-v1", account],
            },
        )
    }

    /// The launch of `codex app-server` for one turn, on the catalog
    /// without native tools unless the turn may use host tools.
    async fn launch(
        &self,
        ready: &Ready,
        input: &TurnInput,
        model: &str,
        stop: &Stop,
    ) -> Result<LaunchSpec, TurnOutcome> {
        let unavailable = |reason: String| failed("provider_unavailable", reason);
        let dirs = ready
            .host
            .bot_dirs(input.bot_id, ID)
            .map_err(|e| unavailable(e.to_string()))?;
        let host_tools = input.delegation.host_tools();
        codex_home::prepare(&dirs, &ready.auth, self.tool_url.as_deref(), host_tools)
            .map_err(|e| unavailable(e.to_string()))?;
        super::attachments::stage(&dirs.project, &input.files)
            .await
            .map_err(|err| {
                failed(
                    "attachment_unavailable",
                    format!("The attached files could not be prepared: {err}"),
                )
            })?;
        let mut env = vec![(
            "CODEX_HOME".into(),
            dirs.provider_home.to_string_lossy().into_owned(),
        )];
        if let (Some(_), Some(grant)) = (&self.tool_url, &input.tool_grant) {
            env.push((GRANT_ENV.into(), grant.into()));
        }
        let mut spec = LaunchSpec {
            turn_id: input.turn_id,
            program: ready.program.clone(),
            args: vec![OsString::from("app-server")],
            env,
            dirs,
        };
        if !host_tools {
            let mut args = tokio::select! {
                () = stop.requested() => {
                    if let Err(err) = super::cli::process::sweep(&ready.host, input.turn_id).await {
                        return Err(TurnOutcome::Uncertain {
                            message: format!("Codex's processes could not be stopped: {err}"),
                        });
                    }
                    return Err(TurnOutcome::Interrupted);
                }
                catalog = codex_home::no_native_tools(&ready.host, &spec, model) => {
                    catalog.map_err(unavailable)?
                }
            };
            args.append(&mut spec.args);
            spec.args = args;
        }
        Ok(spec)
    }
}

fn failed(code: &str, message: impl Into<String>) -> TurnOutcome {
    TurnOutcome::Failed {
        code: code.to_owned(),
        message: message.into(),
    }
}

#[async_trait]
impl Provider for Codex {
    fn project_dir(&self, bot_id: uuid::Uuid) -> Option<std::path::PathBuf> {
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
        let profile = request.profile.ok_or("no saved Codex execution profile")?;
        let text = self
            .no_tools_once(
                request.bot_id,
                &profile,
                prompt::CHECKPOINT_SYSTEM,
                &request.prompt,
                super::CHECKPOINT_DEADLINE,
                cancel,
            )
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
        let spec = match self.launch(ready, &input, &profile.model, &stop).await {
            Ok(spec) => spec,
            Err(outcome) => return outcome,
        };
        let mut server = match AppServer::spawn(&ready.host, &spec) {
            Ok(server) => server,
            Err(err) => return failed("provider_unavailable", err.to_string()),
        };
        let ended = self
            .converse(ready, &mut server, &input, &profile, sink, &stop)
            .await;
        server.conclude(ended, sink).await
    }
}

#[cfg(test)]
pub(super) mod tests;
