//! The [`Provider`] every CLI runs behind, and the one turn every streaming
//! CLI runs. [`Cli`] owns readiness, the profile, the fingerprint, and the
//! catalog; an [`Adapter`] runs the turn. A [`Stream`] adapter says how its
//! CLI is launched, which sessions it can resume, and what its events mean,
//! and [`Cli::stream`] owns the order of everything else: the session
//! choice, the sink's lifecycle calls, attachments, the task grant,
//! delivery, and cleanup.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::attempt::{Ended, Protocol};
use super::files::GRANT_ENV;
use super::session::{self, Identity, Opened, PreparedTurn};
use super::stream::{Interrupt, Prompt, StreamAttempt};
use crate::app::providers::{
    Availability, CheckpointRequest, FailureCode, Profile, Provider, Stop, TurnInput, TurnOutcome,
    TurnSink,
};
use crate::providers::attachments;
use crate::providers::definitions::Definition;
use crate::providers::host::{BotDirs, Host, LaunchSpec, RunError};
use crate::providers::models::{Listed, Models};

/// A CLI that runs on this host, and the adapter's own readiness, such as
/// where the host login is.
pub struct Ready<L> {
    pub host: Host,
    pub program: crate::providers::Program,
    pub version: String,
    pub login: L,
}

/// Runs a CLI's readiness checks: where it is, that it runs, and its
/// login. Called at startup and again on [`Provider::reprobe`].
pub type Probe<L> = Box<dyn Fn() -> BoxFuture<'static, Result<Ready<L>, String>> + Send + Sync>;

/// The CLI's version, which also proves that it runs. `product` names it
/// in the error, such as "Claude Code".
pub async fn version(
    host: &Host,
    program: &crate::providers::Program,
    env: &[(String, String)],
    product: &str,
) -> Result<String, String> {
    host.probe(program.path(), &["--version"], env)
        .await
        .map_err(|why: RunError| {
            format!(
                "{product} does not run (`{} --version`: {why}).",
                program.path().display()
            )
        })
}

/// The tools a CLI attempt is offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSet {
    None,
    /// The host shell and file tools only: else.bot's tools are not served.
    Host,
    Elsebot,
    HostAndElsebot,
}

impl ToolSet {
    pub fn new(host: bool, elsebot: bool) -> Self {
        match (host, elsebot) {
            (true, true) => Self::HostAndElsebot,
            (true, false) => Self::Host,
            (false, true) => Self::Elsebot,
            (false, false) => Self::None,
        }
    }

    /// A turn's tools: the host's only when the owner started the work.
    pub fn for_turn(input: &TurnInput, tool_url: Option<&str>) -> Self {
        Self::new(input.delegation.host_tools(), tool_url.is_some())
    }

    pub fn host(self) -> bool {
        matches!(self, Self::Host | Self::HostAndElsebot)
    }

    pub fn elsebot(self) -> bool {
        matches!(self, Self::Elsebot | Self::HostAndElsebot)
    }
}

/// How the owner's message reaches the CLI. [`TurnSink::sending`] is
/// recorded immediately before the CLI can first read it: before the spawn
/// when the launch carries it, after the spawn when it is written later.
pub enum Delivery {
    /// The launch carries the packet, such as a prompt file. No input;
    /// SIGINT interrupts.
    Launch,
    /// The packet on stdin as the CLI starts, then end of input. SIGINT
    /// interrupts.
    Stdin(String),
    /// This line on stdin after the CLI starts. The input stays open for
    /// steering and for the `interrupt` line.
    Line { message: String, interrupt: String },
}

/// What [`Adapter::launch`] knows about the turn.
pub struct Start<'t> {
    pub input: &'t TurnInput,
    pub profile: &'t Profile,
    pub dirs: &'t BotDirs,
    /// Where the CLI runs and keeps its sessions.
    pub workspace: &'t Path,
    pub tools: ToolSet,
    /// else.bot's MCP endpoint, when `tools` includes it.
    pub tool_url: Option<&'t str>,
    pub turn: &'t PreparedTurn<'t>,
}

/// The adapter's part of a launch. The runner adds the program, the
/// directories, and the task grant.
pub struct Launch<P> {
    pub args: Vec<OsString>,
    pub env: Vec<(String, String)>,
    pub delivery: Delivery,
    pub protocol: P,
}

/// One CLI: what [`Cli`]'s [`Provider`] cannot know about it. A CLI that
/// streams one turn's events implements [`Stream`]; any other implements
/// [`Transport`] itself.
#[async_trait]
pub trait Adapter: Send + Sync + Sized + 'static {
    /// The adapter's readiness beyond a running CLI, such as its login.
    type Login: Send + Sync + 'static;

    const DEFINITION: &'static Definition;
    /// The CLI's name in messages, such as "Claude Code".
    const PRODUCT: &'static str;
    /// Part of every session fingerprint; bump it to retire sessions.
    const PROMPT_VERSION: &'static str;

    /// Why a ready CLI cannot run a turn now, such as a cleared login.
    fn unavailable(&self, ready: &Ready<Self::Login>) -> Option<String> {
        let _ = ready;
        None
    }

    /// The fingerprint's tool tag. Keep each value byte for byte, or
    /// every native session is rebuilt once.
    fn tool_tag(&self, tools: ToolSet) -> &'static str;

    async fn list_models(&self, ready: &Ready<Self::Login>) -> Result<Vec<Listed>, String>;

    /// A no-tool checkpoint, for an adapter that can verify one.
    async fn checkpoint(
        &self,
        cli: &Cli<Self>,
        request: CheckpointRequest,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let _ = (cli, request, cancel);
        Err("this provider cannot produce a verified no-tool checkpoint".into())
    }
}

/// How a CLI runs a turn: a [`Stream`] adapter through [`Cli::stream`], any
/// other over its own transport.
#[async_trait]
pub trait Transport: Adapter {
    /// Run a turn on a ready CLI with a valid profile.
    async fn run(
        &self,
        cli: &Cli<Self>,
        ready: &Ready<Self::Login>,
        profile: &Profile,
        input: &TurnInput,
        sink: &dyn TurnSink,
        stop: Stop,
    ) -> TurnOutcome;
}

#[async_trait]
impl<A: Stream> Transport for A {
    async fn run(
        &self,
        cli: &Cli<Self>,
        ready: &Ready<Self::Login>,
        profile: &Profile,
        input: &TurnInput,
        sink: &dyn TurnSink,
        stop: Stop,
    ) -> TurnOutcome {
        cli.stream(ready, profile, input, sink, stop).await
    }
}

/// A CLI that streams one turn's events: everything [`Cli::stream`]
/// cannot know about it.
#[async_trait]
pub trait Stream: Adapter {
    type Protocol: Protocol + Send;

    /// The fingerprint's provider policies and host account, in order.
    fn policy(&self, ready: &Ready<Self::Login>) -> Vec<String>;

    /// Where the CLI runs and keeps its sessions: the bot's project
    /// unless the adapter isolates turns that may not touch it.
    fn workspace(&self, dirs: &BotDirs, tools: ToolSet) -> std::io::Result<PathBuf> {
        let _ = tools;
        Ok(dirs.project.clone())
    }

    /// Whether the CLI still holds native session `id` for `workspace`.
    fn session_exists(
        &self,
        ready: &Ready<Self::Login>,
        dirs: &BotDirs,
        workspace: &Path,
        id: &str,
    ) -> bool;

    /// Change the packet before it is recorded, for a CLI that takes its
    /// instructions in it.
    fn prepare_packet(&self, turn: &mut PreparedTurn<'_>, input: &TurnInput, dirs: &BotDirs) {
        let _ = (turn, input, dirs);
    }

    /// Write the bot's private CLI files and build the launch.
    async fn launch(
        &self,
        ready: &Ready<Self::Login>,
        start: Start<'_>,
    ) -> Result<Launch<Self::Protocol>, String>;

    /// Run the started attempt to its end. `prompt` says how writing a
    /// [`Delivery::Stdin`] packet ended; any other delivery is
    /// [`Prompt::Written`].
    async fn drive(
        &self,
        attempt: &mut StreamAttempt,
        prompt: Prompt,
        protocol: &mut Self::Protocol,
        turn: &mut PreparedTurn<'_>,
        sink: &dyn TurnSink,
        stop: &Stop,
    ) -> Ended;

    /// Reread the outcome once the attempt's processes have stopped.
    fn concluded(
        &self,
        ready: &Ready<Self::Login>,
        input: &TurnInput,
        accepted: bool,
        outcome: TurnOutcome,
    ) -> TurnOutcome {
        let _ = (ready, input, accepted);
        outcome
    }
}

/// A CLI's current readiness and the probe that replaces it. A probe runs
/// on its own task, so it finishes and is published even when every
/// caller that asked for it has gone.
struct Readiness<L> {
    /// A turn keeps the readiness it started with when a probe replaces it.
    ready: watch::Sender<Result<Arc<Ready<L>>, String>>,
    probe: Option<Probe<L>>,
    probing: tokio::sync::Mutex<()>,
    /// Probes started. A caller shares only a probe that started after it
    /// arrived, since an earlier one may have read a login from before.
    started: AtomicU64,
}

impl<L: Send + Sync + 'static> Readiness<L> {
    async fn reprobe(self: Arc<Self>) {
        let Some(probe) = &self.probe else { return };
        let arrived = self.started.load(Ordering::Acquire);
        let _probing = self.probing.lock().await;
        if self.started.load(Ordering::Acquire) != arrived {
            return;
        }
        self.started.fetch_add(1, Ordering::Release);
        let ready = probe().await;
        self.ready
            .send_modify(|current| *current = ready.map(Arc::new));
    }
}

/// A streaming CLI provider.
pub struct Cli<A: Adapter> {
    pub(in crate::providers) adapter: A,
    pub(in crate::providers) models: Models,
    readiness: Arc<Readiness<A::Login>>,
    pub(in crate::providers) tool_url: Option<String>,
}

impl<A: Adapter> Cli<A> {
    pub fn new(adapter: A, ready: Result<Ready<A::Login>, String>) -> Self {
        Self {
            adapter,
            models: Models::new(A::DEFINITION),
            readiness: Arc::new(Readiness {
                ready: watch::Sender::new(ready.map(Arc::new)),
                probe: None,
                probing: tokio::sync::Mutex::new(()),
                started: AtomicU64::new(0),
            }),
            tool_url: None,
        }
    }

    /// Run `probe` now, and again on each [`Provider::reprobe`].
    pub async fn probed(adapter: A, probe: Probe<A::Login>) -> Self {
        let ready = probe().await;
        let mut cli = Self::new(adapter, ready);
        Arc::get_mut(&mut cli.readiness)
            .expect("a new provider's readiness is unshared")
            .probe = Some(probe);
        cli
    }

    /// Offer else.bot's tools at this MCP URL.
    pub fn with_tools(mut self, url: String) -> Self {
        self.tool_url = Some(url);
        self
    }

    pub(in crate::providers) fn ready(&self) -> Result<Arc<Ready<A::Login>>, String> {
        self.readiness.ready.borrow().clone()
    }

    /// The session fingerprint, with the adapter's `policy` in order.
    pub(in crate::providers) fn identify(
        &self,
        ready: &Ready<A::Login>,
        profile: &Profile,
        input: &TurnInput,
        policy: &[&str],
    ) -> String {
        let tools = ToolSet::for_turn(input, self.tool_url.as_deref());
        session::fingerprint(
            input,
            profile,
            &Identity {
                prompt_version: A::PROMPT_VERSION,
                version: &ready.version,
                runtime: &ready.host.runtime(input.bot_id),
                tools: self.adapter.tool_tag(tools),
                policy,
            },
        )
    }
}

impl<A: Adapter> Cli<A> {
    /// The launch of `args` and `env` in `dirs` for `input`'s turn, with
    /// the turn's attachments staged in the project, the current program,
    /// and the task grant when else.bot's tools are offered.
    pub(in crate::providers) async fn prepare_launch(
        &self,
        ready: &Ready<A::Login>,
        input: &TurnInput,
        dirs: BotDirs,
        args: Vec<OsString>,
        mut env: Vec<(String, String)>,
    ) -> Result<LaunchSpec, TurnOutcome> {
        attachments::stage(&dirs.project, &input.files)
            .await
            .map_err(|err| {
                TurnOutcome::failed(
                    FailureCode::AttachmentUnavailable,
                    format!("The attached files could not be prepared: {err}"),
                )
            })?;
        let program = ready.program.current().await.map_err(|why| {
            TurnOutcome::failed(
                FailureCode::ProviderUnavailable,
                format!("Could not start {}: {why}", A::PRODUCT),
            )
        })?;
        if let (Some(_), Some(grant)) = (&self.tool_url, &input.tool_grant) {
            env.push((GRANT_ENV.to_owned(), grant.clone()));
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

impl<A: Stream> Cli<A> {
    pub(in crate::providers) fn fingerprint(
        &self,
        ready: &Ready<A::Login>,
        profile: &Profile,
        input: &TurnInput,
    ) -> String {
        let policy = self.adapter.policy(ready);
        let policy: Vec<&str> = policy.iter().map(String::as_str).collect();
        self.identify(ready, profile, input, &policy)
    }

    /// The streaming turn, for [`Adapter::run`].
    pub async fn stream(
        &self,
        ready: &Ready<A::Login>,
        profile: &Profile,
        input: &TurnInput,
        sink: &dyn TurnSink,
        stop: Stop,
    ) -> TurnOutcome {
        let unavailable = |why: String| TurnOutcome::failed(FailureCode::ProviderUnavailable, why);
        let dirs = match ready.host.bot_dirs(input.bot_id, A::DEFINITION.id) {
            Ok(dirs) => dirs,
            Err(err) => return unavailable(err.to_string()),
        };
        let tools = ToolSet::for_turn(input, self.tool_url.as_deref());
        let workspace = match self.adapter.workspace(&dirs, tools) {
            Ok(workspace) => workspace,
            Err(err) => return unavailable(err.to_string()),
        };
        let fingerprint = self.fingerprint(ready, profile, input);
        let opened = match input.session.as_ref().filter(|s| {
            s.fingerprint == fingerprint
                && self
                    .adapter
                    .session_exists(ready, &dirs, &workspace, &s.native_id)
        }) {
            Some(session) => Opened::Resumed(session),
            None => Opened::Fresh(Uuid::new_v4().to_string()),
        };
        let mut turn = PreparedTurn::new(input, fingerprint, opened);
        self.adapter.prepare_packet(&mut turn, input, &dirs);
        // The last moment a stop leaves nothing recorded, not even a
        // fresh session that retires the previous one.
        if stop.is_requested() {
            return TurnOutcome::Interrupted;
        }
        if turn.record(sink).await.is_err() {
            return TurnOutcome::Interrupted;
        }
        let start = Start {
            input,
            profile,
            dirs: &dirs,
            workspace: &workspace,
            tools,
            tool_url: self.tool_url.as_deref().filter(|_| tools.elsebot()),
            turn: &turn,
        };
        let launch = match self.adapter.launch(ready, start).await {
            Ok(launch) => launch,
            Err(why) => return unavailable(why),
        };
        let mut spec = match self
            .prepare_launch(ready, input, dirs, launch.args, launch.env)
            .await
        {
            Ok(spec) => spec,
            Err(outcome) => return outcome,
        };
        spec.dirs.project = workspace;
        let mut protocol = launch.protocol;
        let label = A::DEFINITION.label;
        let (mut attempt, delivered) = match launch
            .delivery
            .start(&ready.host, &spec, sink, &stop, label)
            .await
        {
            Ok(started) => started,
            Err(outcome) => return outcome,
        };
        let ended = match delivered {
            Ok(prompt) => {
                self.adapter
                    .drive(&mut attempt, prompt, &mut protocol, &mut turn, sink, &stop)
                    .await
            }
            Err(ended) => ended,
        };
        let outcome = attempt.conclude(ended, sink).await;
        self.adapter
            .concluded(ready, input, turn.accepted(), outcome)
    }
}

impl Delivery {
    /// Start the CLI and give it the owner's message. `Err` when nothing
    /// runs. A running attempt comes back with the [`Prompt`] to drive it
    /// with, or with how it ended: a write that fails after `sending` may
    /// have been read in part, so it is never a refusal.
    async fn start(
        self,
        host: &Host,
        spec: &LaunchSpec,
        sink: &dyn TurnSink,
        stop: &Stop,
        label: &'static str,
    ) -> Result<(StreamAttempt, Result<Prompt, Ended>), TurnOutcome> {
        let unavailable = |err: std::io::Error| {
            TurnOutcome::failed(FailureCode::ProviderUnavailable, err.to_string())
        };
        let unknown = |err: std::io::Error| {
            Ended::Abandoned(TurnOutcome::Uncertain {
                message: format!("Delivery to {label} is unknown: {err}"),
            })
        };
        match self {
            Self::Launch => {
                sending(sink).await?;
                let attempt = StreamAttempt::spawn(host, spec, Interrupt::Signal, label)
                    .map_err(unavailable)?;
                Ok((attempt, Ok(Prompt::Written)))
            }
            Self::Stdin(packet) => {
                sending(sink).await?;
                let mut attempt =
                    StreamAttempt::spawn_for_prompt(host, spec, label).map_err(unavailable)?;
                let prompt = match write(&mut attempt, &packet, stop).await {
                    Ok(()) => Ok(Prompt::Written),
                    Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(Prompt::Refused),
                    Err(err) => Err(unknown(err)),
                };
                attempt.end_input();
                Ok((attempt, prompt))
            }
            Self::Line { message, interrupt } => {
                let mut attempt =
                    StreamAttempt::spawn(host, spec, Interrupt::Line(interrupt), label)
                        .map_err(unavailable)?;
                let prompt = match sending(sink).await {
                    Err(outcome) => Err(Ended::Undelivered(outcome)),
                    Ok(()) => write(&mut attempt, &message, stop)
                        .await
                        .map(|()| Prompt::Written)
                        .map_err(unknown),
                };
                Ok((attempt, prompt))
            }
        }
    }
}

/// Record [`TurnSink::sending`]; a fenced turn ends interrupted.
async fn sending(sink: &dyn TurnSink) -> Result<(), TurnOutcome> {
    sink.sending().await.map_err(|_| TurnOutcome::Interrupted)
}

/// Write to the CLI's input until it is written or the stop escalates.
async fn write(attempt: &mut StreamAttempt, text: &str, stop: &Stop) -> std::io::Result<()> {
    tokio::select! {
        written = attempt.write(text) => written,
        () = stop.escalated() => Err(std::io::Error::other("the stop escalated during delivery")),
    }
}

#[async_trait]
impl<A: Transport> Provider for Cli<A> {
    fn id(&self) -> &'static str {
        A::DEFINITION.id
    }

    fn label(&self) -> &'static str {
        A::DEFINITION.label
    }

    fn project_dir(&self, bot_id: Uuid) -> Option<PathBuf> {
        self.ready().ok().map(|r| r.host.project_path(bot_id))
    }

    fn availability(&self) -> Availability {
        match self.ready() {
            Ok(ready) => self
                .adapter
                .unavailable(&ready)
                .map_or(Availability::Available, Availability::Unavailable),
            Err(reason) => Availability::Unavailable(reason),
        }
    }

    async fn reprobe(&self) -> Availability {
        let probe = tokio::spawn(self.readiness.clone().reprobe());
        if let Err(err) = probe.await {
            tracing::warn!(provider = A::DEFINITION.id, %err, "the readiness probe failed");
        }
        self.availability()
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
        let ready = self.ready()?;
        self.adapter.list_models(&ready).await
    }

    async fn checkpoint(
        &self,
        request: CheckpointRequest,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        self.adapter.checkpoint(self, request, cancel).await
    }

    async fn run(&self, input: TurnInput, sink: &dyn TurnSink, stop: Stop) -> TurnOutcome {
        let unavailable = |why: String| TurnOutcome::failed(FailureCode::ProviderUnavailable, why);
        let ready = match self.ready() {
            Ok(ready) => ready,
            Err(reason) => return unavailable(reason),
        };
        if let Some(reason) = self.adapter.unavailable(&ready) {
            return unavailable(reason);
        }
        let Some(profile) = &input.profile else {
            return TurnOutcome::failed(
                FailureCode::ProviderMisconfigured,
                "No execution profile was snapshotted.",
            );
        };
        if let Err(reason) = self.models.validate(profile) {
            return TurnOutcome::failed(FailureCode::InvalidProfile, reason);
        }
        let outcome = self
            .adapter
            .run(self, &ready, profile, &input, sink, stop)
            .await;
        // A login that lapsed or a CLI that moved since the last probe
        // shows as unavailable from now on, not only in this turn. Claude
        // reports an expired login as a failed API call.
        if let TurnOutcome::Failed {
            code: FailureCode::ProviderUnavailable | FailureCode::ProviderFailed,
            ..
        } = &outcome
        {
            tokio::spawn(self.readiness.clone().reprobe());
        }
        outcome
    }
}
