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

use super::cli::turn::{Adapter, Cli, ToolSet, Transport};
use super::codex_home;
use super::codex_rpc::AppServer;
use super::definitions::Definition;
use super::host::{Host, LaunchSpec, Run};
use super::models::Listed;
use super::prompt;
use crate::app::providers::{
    CheckpointRequest, FailureCode, Profile, Stop, TurnInput, TurnOutcome, TurnSink,
};

pub const ID: &str = "codex";
const PROMPT_VERSION: &str = "else-codex-v20";
const RPC_TIMEOUT: Duration = Duration::from_secs(60);

pub static DEFINITION: Definition = Definition {
    id: ID,
    label: "Codex",
    fallback: &[
        "gpt-6.2-sol",
        "gpt-6.2-astra",
        "gpt-6.1-sol",
        "gpt-6-astra",
        "gpt-6.1-terra",
        "gpt-6-sol",
        "gpt-6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ],
    efforts: &["low", "medium", "high", "xhigh", "max"],
    tiers: &["default", "priority"],
    pinned: super::definitions::any_id,
    model_note: "",
    effort_note: " Ultra delegates to native sub-agents, which are disabled.",
    tier_note: "",
    build,
};

/// The one Codex is both a provider and the memory summarizer.
fn build<'w>(wiring: &'w super::Wiring<'_>) -> futures_util::future::BoxFuture<'w, super::Built> {
    let (host, bin) = (wiring.host.clone(), wiring.config.codex_bin.clone());
    let probe: super::cli::turn::Probe<PathBuf> = Box::new(move || {
        let (host, bin) = (host.clone(), bin.clone());
        Box::pin(async move {
            let auth = codex_home::host_auth_file(
                std::env::var("CODEX_HOME").ok().as_deref(),
                std::env::var("HOME").ok().as_deref(),
            );
            let program = super::executable(&host, bin.as_deref(), "codex").await;
            ready(Some(host), program, auth).await
        })
    });
    Box::pin(async move {
        let codex = std::sync::Arc::new(
            Codex::probed(CodexServer, probe)
                .await
                .with_tools(wiring.tool_url.clone()),
        );
        super::Built {
            provider: codex.clone(),
            summarizer: Some(codex),
        }
    })
}

/// A CLI that runs, with the host login file.
type Ready = super::cli::turn::Ready<PathBuf>;

pub struct CodexServer;

pub type Codex = Cli<CodexServer>;

/// Locate the CLI and the host login and check that the CLI runs. A missing
/// piece makes the provider unavailable with a reason instead of failing
/// startup. The version is only session identity.
async fn ready(
    host: Option<Host>,
    program: Result<Option<super::Program>, String>,
    auth: Option<PathBuf>,
) -> Result<Ready, String> {
    let host = host.ok_or("Codex is not configured on this host")?;
    let program = program?.ok_or("the codex CLI was not found on the host")?;
    let auth = auth.ok_or("the Codex login location is unknown")?;
    if !auth.is_file() {
        return Err("Codex is not logged in on the host (run `codex login`)".into());
    }
    let version = host
        .probe(program.path(), &["--version"], &[])
        .await
        .map_err(|why| {
            format!(
                "the codex executable does not run (`{} --version`: {why}); \
                 set ELSE_CODEX_BIN to the native codex binary",
                program.path().display()
            )
        })?;
    Ok(Ready {
        host,
        program,
        version,
        login: auth,
    })
}

impl Codex {
    pub async fn detect(
        host: Option<Host>,
        program: Option<PathBuf>,
        auth: Option<PathBuf>,
    ) -> Self {
        Self::probed(
            CodexServer,
            Box::new(move || {
                let (host, program, auth) = (host.clone(), program.clone(), auth.clone());
                Box::pin(ready(host, Ok(program.map(Into::into)), auth))
            }),
        )
        .await
    }

    fn fingerprint(
        &self,
        ready: &Ready,
        profile: &Profile,
        input: &TurnInput,
        account: &str,
    ) -> String {
        self.identify(
            ready,
            profile,
            input,
            &["agents:disabled", "skills:bot-project-only-v1", account],
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
        let unavailable =
            |reason: String| TurnOutcome::failed(FailureCode::ProviderUnavailable, reason);
        let dirs = ready
            .host
            .bot_dirs(input.bot_id, ID)
            .map_err(|e| unavailable(e.to_string()))?;
        let host_tools = input.delegation.host_tools();
        codex_home::prepare(&dirs, &ready.login, self.tool_url.as_deref(), host_tools)
            .map_err(|e| unavailable(e.to_string()))?;
        let env = vec![(
            "CODEX_HOME".into(),
            dirs.provider_home.to_string_lossy().into_owned(),
        )];
        let mut spec = self
            .prepare_launch(ready, input, dirs, vec![OsString::from("app-server")], env)
            .await?;
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

/// How a turn with host tools reaches else.bot's tools. Codex's full catalog
/// defers MCP tools, so their declarations reach the model only through
/// `ALL_TOOLS`; naming the tools saves a scan to discover them.
pub(crate) fn deferred_tools() -> String {
    format!(
        "Your else.bot tools are deferred: exec's description does not declare them. Each is \
         mcp__elsebot__ followed by one of: {}. Before you first call any of them, read the \
         declarations of every one the task needs in a single lookup by exact name, such as \
         `text(ALL_TOOLS.filter(t => [\"mcp__elsebot__find_tool\", \
         \"mcp__elsebot__use_tool\"].includes(t.name)))`. Never print ALL_TOOLS in full or \
         filtered by a pattern.",
        crate::app::tools::TOOL_NAMES.join(", ")
    )
}

#[async_trait]
impl Transport for CodexServer {
    async fn run(
        &self,
        cli: &Codex,
        ready: &Ready,
        profile: &Profile,
        input: &TurnInput,
        sink: &dyn TurnSink,
        stop: Stop,
    ) -> TurnOutcome {
        let spec = match cli.launch(ready, input, &profile.model, &stop).await {
            Ok(spec) => spec,
            Err(outcome) => return outcome,
        };
        let mut server = match AppServer::spawn(&ready.host, &spec) {
            Ok(server) => server,
            Err(err) => {
                return TurnOutcome::failed(FailureCode::ProviderUnavailable, err.to_string());
            }
        };
        let ended = cli
            .converse(ready, &mut server, input, profile, sink, &stop)
            .await;
        server.conclude(ended, sink).await
    }
}

#[async_trait]
impl Adapter for CodexServer {
    type Login = PathBuf;

    const DEFINITION: &'static Definition = &DEFINITION;
    const PRODUCT: &'static str = "Codex";
    const PROMPT_VERSION: &'static str = PROMPT_VERSION;

    fn tool_tag(&self, tools: ToolSet) -> &'static str {
        match tools {
            ToolSet::HostAndElsebot => "tools:host+elsebot-v5",
            ToolSet::Host => "tools:host",
            ToolSet::Elsebot => "tools:elsebot-only-v3",
            ToolSet::None => "tools:none",
        }
    }

    async fn list_models(&self, ready: &Ready) -> Result<Vec<Listed>, String> {
        let program = ready.program.current().await?;
        let run = Run {
            limit: 16 * 1024 * 1024,
            timeout: super::models::ASK_TIMEOUT,
            ..Run::new(&program, &["debug", "models"])
        };
        let exported = ready
            .host
            .run(&run)
            .await
            .map_err(|why| format!("`codex debug models` failed: {why}"))?;
        codex_home::listed_models(&exported)
    }

    async fn checkpoint(
        &self,
        cli: &Codex,
        request: CheckpointRequest,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let profile = request.profile.ok_or("no saved Codex execution profile")?;
        let text = cli
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
}

#[cfg(test)]
pub(super) mod tests;
