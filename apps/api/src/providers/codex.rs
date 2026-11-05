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

mod skills;
mod summary;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::codex_home;
use super::codex_rpc::{AppServer, Incoming, RpcError};
use super::host::{Host, LaunchSpec};
use super::prompt;
use crate::app::providers::{
    Availability, CheckpointRequest, Profile, Provider, SessionState, TurnInput, TurnOutcome,
    TurnSink,
};

pub const ID: &str = "codex";
const PROMPT_VERSION: &str = "else-codex-v17";
const PARK_GRACE: Duration = Duration::from_secs(20);
const PARK_STOP: Duration = Duration::from_secs(15);
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
/// `ultra` delegates to native agents, which are disabled.
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const TIERS: &[&str] = &["default", "priority"];
const RPC_TIMEOUT: Duration = Duration::from_secs(60);
const TURN_DEADLINE: Duration = Duration::from_secs(30 * 60);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

const FORBIDDEN_ITEMS: &[&str] = &[
    "dynamicToolCall",
    "collabAgentToolCall",
    "subAgentActivity",
    "webSearch",
    "imageGeneration",
    "imageView",
];

const HOST_TOOL_ITEMS: &[&str] = &["commandExecution", "fileChange"];

pub struct Codex {
    host: Option<Host>,
    program: Option<PathBuf>,
    auth: Option<PathBuf>,
    version: String,
    unavailable: Option<String>,
    tool_url: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Reported {
    model: Option<String>,
    effort: Option<String>,
    tier: Option<String>,
}

impl Reported {
    fn from_response(value: &Value) -> Self {
        let text = |key: &str| value[key].as_str().map(str::to_owned);
        Self {
            model: text("model"),
            effort: text("reasoningEffort").or_else(|| text("effort")),
            tier: text("serviceTier"),
        }
    }

    fn mismatch(&self, profile: &Profile) -> Option<String> {
        let check = |field: &str, got: &Option<String>, want: &str| {
            got.as_ref()
                .filter(|got| *got != want)
                .map(|got| format!("{field} {got} instead of {want}"))
        };
        let issues: Vec<String> = [
            check("model", &self.model, &profile.model),
            check("reasoning effort", &self.effort, &profile.reasoning_effort),
            check("service tier", &self.tier, &profile.service_tier),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!issues.is_empty()).then(|| format!("Codex reported {}.", issues.join(", ")))
    }

    fn to_json(&self) -> Value {
        json!({ "model": self.model, "reasoning_effort": self.effort, "service_tier": self.tier })
    }
}

impl Codex {
    /// Locate the CLI and the host login and check that the CLI runs. A
    /// missing piece makes the provider unavailable with a reason instead of
    /// failing startup. The version is only session identity.
    pub fn detect(host: Option<Host>, program: Option<PathBuf>, auth: Option<PathBuf>) -> Self {
        let version = match (&host, &program) {
            (Some(host), Some(program)) => {
                host.probe(program, &["--version"], &[]).unwrap_or_default()
            }
            _ => String::new(),
        };
        let unavailable = match (&host, &program, &auth) {
            (None, _, _) => Some("Codex is not configured on this host".to_owned()),
            (_, None, _) => Some("the codex CLI was not found on the host".to_owned()),
            (_, _, Some(auth)) if !auth.is_file() => {
                Some("Codex is not logged in on the host (run `codex login`)".to_owned())
            }
            (_, _, None) => Some("the Codex login location is unknown".to_owned()),
            _ if version.is_empty() => Some(
                "the codex executable does not run (`codex --version` failed); \
                 set ELSE_CODEX_BIN to the native codex binary"
                    .to_owned(),
            ),
            _ => None,
        };
        Self {
            host,
            program,
            auth,
            version,
            unavailable,
            tool_url: None,
        }
    }

    /// Offer else.bot's tools (memory) at this MCP URL.
    pub fn with_tools(mut self, url: String) -> Self {
        self.tool_url = Some(url);
        self
    }

    fn fingerprint(&self, profile: &Profile, input: &TurnInput, account: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(crate::app::tools::TOOL_NAMES.join("\0").as_bytes());
        let registry = prompt::registry_text(input);
        let runtime = self
            .host
            .as_ref()
            .map_or_else(String::new, |h| h.runtime(input.bot_id));
        for part in [
            registry.as_str(),
            PROMPT_VERSION,
            &self.version,
            &runtime,
            match (input.delegation.host_tools(), self.tool_url.is_some()) {
                (true, true) => "tools:host+elsebot-v5",
                (true, false) => "tools:host",
                (false, true) => "tools:elsebot-only-v3",
                (false, false) => "tools:none",
            },
            "agents:disabled",
            "skills:bot-project-only-v1",
            account,
            &profile.model,
            &profile.reasoning_effort,
            &profile.service_tier,
            &input.bot_name,
        ] {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        hasher
            .finalize()
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// `turn/start` input in the owner's order: text items, with each image
/// also given as a local image right after its reference. Every file is
/// named by its staged copy in the project directory.
fn turn_input(segments: &[prompt::Segment<'_>], project: &std::path::Path) -> Value {
    let mut items = Vec::new();
    let mut text = String::new();
    for segment in segments {
        match segment {
            prompt::Segment::Text(t) => text.push_str(t),
            prompt::Segment::File(file) => {
                text.push_str(&prompt::file_reference(&file.attachment));
                if super::attachments::is_image(&file.attachment) {
                    items.push(json!({ "type": "text", "text": std::mem::take(&mut text) }));
                    let path = project.join(super::attachments::relative_path(&file.attachment));
                    items.push(json!({ "type": "localImage", "path": path }));
                }
            }
        }
    }
    if items.is_empty() || !text.is_empty() {
        items.push(json!({ "type": "text", "text": text }));
    }
    Value::Array(items)
}

async fn interrupt(server: &AppServer, thread_id: &str, native_turn: &str) {
    let params = json!({ "threadId": thread_id, "turnId": native_turn });
    let _ = server.request("turn/interrupt", params, RPC_TIMEOUT).await;
}

fn failed(code: &str, message: impl Into<String>) -> TurnOutcome {
    TurnOutcome::Failed {
        code: code.to_owned(),
        message: message.into(),
    }
}

/// The profile rules without an adapter (also used to check restored
/// settings).
pub fn validate(profile: &Profile) -> Result<(), String> {
    if !MODELS.contains(&profile.model.as_str()) {
        return Err(format!(
            "Unsupported Codex model {}. Supported: {}.",
            profile.model,
            MODELS.join(", ")
        ));
    }
    if !EFFORTS.contains(&profile.reasoning_effort.as_str()) {
        return Err(format!(
            "Unsupported reasoning effort {}. Supported: {} (ultra delegates to native \
             sub-agents, which are disabled).",
            profile.reasoning_effort,
            EFFORTS.join(", ")
        ));
    }
    if !TIERS.contains(&profile.service_tier.as_str()) {
        return Err(format!(
            "Unsupported service tier {}. Supported: {}.",
            profile.service_tier,
            TIERS.join(", ")
        ));
    }
    Ok(())
}

#[async_trait]
impl Provider for Codex {
    fn id(&self) -> &'static str {
        ID
    }

    fn label(&self) -> &'static str {
        "Codex"
    }

    fn availability(&self) -> Availability {
        match &self.unavailable {
            Some(reason) => Availability::Unavailable(reason.clone()),
            None => Availability::Available,
        }
    }

    fn uses_profile(&self) -> bool {
        true
    }

    fn validate_profile(&self, profile: &Profile) -> Result<(), String> {
        validate(profile)
    }

    fn catalog(&self) -> crate::app::providers::Catalog {
        crate::app::providers::Catalog {
            models: MODELS,
            featured_models: &MODELS[..4],
            efforts: EFFORTS,
            tiers: TIERS,
        }
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

    async fn run(
        &self,
        input: TurnInput,
        sink: &dyn TurnSink,
        cancel: CancellationToken,
    ) -> TurnOutcome {
        if let Some(reason) = &self.unavailable {
            return failed("provider_unavailable", reason.clone());
        }
        let (Some(host), Some(program), Some(auth)) = (&self.host, &self.program, &self.auth)
        else {
            return failed("provider_unavailable", "Codex is not configured.");
        };
        let Some(profile) = input.profile.clone() else {
            return failed(
                "provider_misconfigured",
                "No execution profile was snapshotted.",
            );
        };
        if let Err(reason) = self.validate_profile(&profile) {
            return failed("invalid_profile", reason);
        }
        let dirs = match host.bot_dirs(input.bot_id, ID) {
            Ok(dirs) => dirs,
            Err(err) => return failed("provider_unavailable", err.to_string()),
        };
        let host_tools = input.delegation.host_tools();
        if let Err(err) = codex_home::prepare(&dirs, auth, self.tool_url.as_deref(), host_tools) {
            return failed("provider_unavailable", err.to_string());
        }
        if let Err(err) = super::attachments::stage(&dirs.project, &input.files).await {
            return failed(
                "attachment_unavailable",
                format!("The attached files could not be prepared: {err}"),
            );
        }
        let project = dirs.project.clone();
        let mut spec = LaunchSpec {
            turn_id: input.turn_id,
            program: program.clone(),
            args: vec![OsString::from("app-server")],
            env: {
                let mut env = vec![(
                    "CODEX_HOME".into(),
                    dirs.provider_home.to_string_lossy().into_owned(),
                )];
                if let (Some(_), Some(grant)) = (&self.tool_url, &input.tool_grant) {
                    env.push((codex_home::GRANT_ENV.into(), grant.into()));
                }
                env
            },
            dirs,
        };
        if !host_tools {
            let catalog = tokio::select! {
                _ = cancel.cancelled() => return TurnOutcome::Interrupted,
                catalog = codex_home::no_native_tools(host, &spec, &profile.model) => catalog,
            };
            match catalog {
                Ok(mut args) => {
                    args.append(&mut spec.args);
                    spec.args = args;
                }
                Err(reason) => return failed("provider_unavailable", reason),
            }
        }
        let mut server = match AppServer::start(host, &spec).await {
            Ok(server) => server,
            Err(err) => return failed("provider_unavailable", err.to_string()),
        };
        let outcome = self
            .converse(&mut server, &input, &project, &profile, sink, cancel)
            .await;
        server.shutdown(SHUTDOWN_GRACE).await;
        outcome
    }
}

impl Codex {
    fn check_servers(&self, servers: &Value, with_tools: bool) -> Result<(), String> {
        let servers = servers.as_array().cloned().unwrap_or_default();
        if !with_tools || self.tool_url.is_none() {
            return if servers.is_empty() {
                Ok(())
            } else {
                Err("Codex has MCP servers configured.".into())
            };
        }
        let [server] = servers.as_slice() else {
            return Err(format!(
                "Codex reported {} MCP servers; expected only else.bot's.",
                servers.len()
            ));
        };
        if server["name"].as_str() != Some(crate::app::tools::SERVER_NAME) {
            return Err("Codex reported an MCP server other than else.bot's.".into());
        }
        let mut tools: Vec<String> = match &server["tools"] {
            Value::Object(map) => map.keys().cloned().collect(),
            Value::Array(list) => list
                .iter()
                .filter_map(|t| t["name"].as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        };
        tools.sort();
        let mut expected: Vec<String> = crate::app::tools::TOOL_NAMES
            .iter()
            .map(|t| (*t).to_owned())
            .collect();
        expected.sort();
        if tools != expected {
            return Err(format!(
                "else.bot's MCP server offered unexpected tools: {tools:?}"
            ));
        }
        Ok(())
    }

    async fn check_catalog(server: &AppServer, model: &str) -> Result<(), String> {
        let listed = server
            .request("model/list", json!({ "includeHidden": true }), RPC_TIMEOUT)
            .await
            .map_err(|err| err.to_string())?;
        match listed["data"].as_array().map(Vec::as_slice) {
            Some([only]) if only["model"] == model && listed["nextCursor"].is_null() => Ok(()),
            _ => Err("Codex is not running on the catalog without native tools.".into()),
        }
    }

    fn allowed_tool(&self, item: &Value) -> bool {
        self.tool_url.is_some()
            && item["server"].as_str() == Some(crate::app::tools::SERVER_NAME)
            && item["tool"]
                .as_str()
                .is_some_and(|t| crate::app::tools::TOOL_NAMES.contains(&t))
    }

    async fn preflight(&self, server: &AppServer) -> Result<String, TurnOutcome> {
        let status = server
            .request("mcpServerStatus/list", json!({}), RPC_TIMEOUT)
            .await
            .map_err(|err| failed("provider_unavailable", err.to_string()))?;
        if let Err(reason) = self.check_servers(&status["data"], true) {
            return Err(failed("policy_violation", reason));
        }
        let account = server
            .request("account/read", json!({}), RPC_TIMEOUT)
            .await
            .map_err(|err| failed("provider_unavailable", err.to_string()))?;
        let account = &account["account"];
        if account.is_null() {
            return Err(failed(
                "provider_unavailable",
                "Codex is not logged in on the host (run `codex login`).",
            ));
        }
        Ok(format!(
            "{}:{}",
            account["type"].as_str().unwrap_or_default(),
            account["email"].as_str().unwrap_or_default()
        ))
    }

    async fn open_thread(
        &self,
        server: &AppServer,
        input: &TurnInput,
        profile: &Profile,
        fingerprint: &str,
    ) -> Result<(String, i64, Reported), TurnOutcome> {
        let mut config = skills::config(server)
            .await
            .map_err(|reason| failed("provider_unavailable", reason))?;
        config["model_reasoning_effort"] = json!(profile.reasoning_effort);
        let settings = json!({
            "model": profile.model,
            "serviceTier": profile.service_tier,
            "cwd": server.cwd,
            "approvalPolicy": "never",
            "sandbox": codex_home::sandbox_mode(input.delegation.host_tools()),
            "developerInstructions": prompt::instructions(input, &server.cwd),
            "config": config,
        });
        if let Some(session) = input
            .session
            .as_ref()
            .filter(|s| s.fingerprint == fingerprint)
        {
            let mut params = settings.clone();
            params["threadId"] = json!(session.native_id);
            match server.request("thread/resume", params, RPC_TIMEOUT).await {
                Ok(response) => {
                    return Ok((
                        session.native_id.clone(),
                        session.considered_through_turn,
                        Reported::from_response(&response),
                    ));
                }
                Err(RpcError::Remote { message, .. }) => {
                    tracing::info!(turn = %input.turn_id, %message, "native session not resumable; rebuilding");
                }
                Err(err) => return Err(failed("provider_unavailable", err.to_string())),
            }
        }
        let response = server
            .request("thread/start", settings, RPC_TIMEOUT)
            .await
            .map_err(|err| failed("provider_unavailable", err.to_string()))?;
        let thread = response["thread"]["id"]
            .as_str()
            .ok_or_else(|| failed("provider_protocol", "thread/start returned no thread ID"))?
            .to_owned();
        Ok((thread, 0, Reported::from_response(&response)))
    }

    async fn converse(
        &self,
        server: &mut AppServer,
        input: &TurnInput,
        project: &std::path::Path,
        profile: &Profile,
        sink: &dyn TurnSink,
        cancel: CancellationToken,
    ) -> TurnOutcome {
        let account = match self.preflight(server).await {
            Ok(account) => account,
            Err(outcome) => return outcome,
        };
        if !input.delegation.host_tools()
            && let Err(reason) = Self::check_catalog(server, &profile.model).await
        {
            return failed("policy_violation", reason);
        }
        let fingerprint = self.fingerprint(profile, input, &account);
        let (thread_id, considered, reported) =
            match self.open_thread(server, input, profile, &fingerprint).await {
                Ok(opened) => opened,
                Err(outcome) => return outcome,
            };
        if let Some(mismatch) = reported.mismatch(profile) {
            return failed("profile_mismatch", mismatch);
        }
        if cancel.is_cancelled() {
            return TurnOutcome::Interrupted;
        }
        let resumed_session = input.session.as_ref().filter(|s| s.native_id == thread_id);
        let inject = prompt::memory_for(input.memory.as_ref(), resumed_session);
        let mut state = SessionState {
            native_id: thread_id.clone(),
            fingerprint,
            considered_through_turn: considered,
            memory_revision: resumed_session.and_then(|s| s.memory_revision),
        };
        if sink.session(&state).await.is_err() {
            return TurnOutcome::Interrupted;
        }
        let (segments, selection) = prompt::turn_content(input, considered, inject);
        let packet = prompt::segments_text(&segments);
        if sink.packet(&selection, &packet).await.is_err() {
            return TurnOutcome::Interrupted;
        }
        if sink.sending().await.is_err() {
            return TurnOutcome::Interrupted;
        }
        let started = server
            .request(
                "turn/start",
                json!({
                    "threadId": thread_id,
                    "model": profile.model,
                    "effort": profile.reasoning_effort,
                    "serviceTier": profile.service_tier,
                    "input": turn_input(&segments, project),
                }),
                RPC_TIMEOUT,
            )
            .await;
        let native_turn = match started {
            Ok(response) => match response["turn"]["id"].as_str() {
                Some(id) => id.to_owned(),
                None => {
                    return TurnOutcome::Uncertain {
                        message: "Codex accepted the turn without an ID.".into(),
                    };
                }
            },
            Err(RpcError::Remote { message, .. }) => return failed("provider_refused", message),
            Err(err) => {
                return TurnOutcome::Uncertain {
                    message: format!("Delivery to Codex is unknown: {err}"),
                };
            }
        };
        if sink.running().await.is_err() {
            cancel.cancel();
        }
        state.considered_through_turn = input.turn_no;
        if let Some(memory) = inject {
            state.memory_revision = Some(memory.revision);
        }
        let _ = sink.session(&state).await;
        let _ = sink.reported(&reported.to_json(), &native_turn).await;
        self.drain(server, &thread_id, &native_turn, input, sink, cancel)
            .await
    }

    async fn drain(
        &self,
        server: &mut AppServer,
        thread_id: &str,
        native_turn: &str,
        input: &TurnInput,
        sink: &dyn TurnSink,
        cancel: CancellationToken,
    ) -> TurnOutcome {
        let Some(profile) = &input.profile else {
            return failed(
                "provider_misconfigured",
                "No execution profile was snapshotted.",
            );
        };
        let host_tools = input.delegation.host_tools();
        let deadline = tokio::time::Instant::now() + TURN_DEADLINE;
        let mut park_at: Option<tokio::time::Instant> = None;
        let mut interrupted = false;
        let mut violation: Option<(&str, String)> = None;
        let mut current_message: Option<String> = None;
        loop {
            let message = tokio::select! {
                _ = cancel.cancelled(), if !interrupted => {
                    interrupted = true;
                    interrupt(server, thread_id, native_turn).await;
                    continue;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    interrupt(server, thread_id, native_turn).await;
                    return failed("turn_deadline", "The turn exceeded its 30 minute deadline.");
                }
                _ = tokio::time::sleep_until(park_at.unwrap_or(deadline)), if park_at.is_some() && !interrupted => {
                    tracing::info!(turn = %native_turn, "parent kept going after delegating; interrupting");
                    interrupted = true;
                    interrupt(server, thread_id, native_turn).await;
                    park_at = Some(tokio::time::Instant::now() + PARK_STOP);
                    continue;
                }
                _ = tokio::time::sleep_until(park_at.unwrap_or(deadline)), if park_at.is_some() && interrupted => {
                    tracing::warn!(turn = %native_turn, "parked attempt did not stop; ending it");
                    sink.retire_session().await;
                    return TurnOutcome::Interrupted;
                }
                message = server.next() => message,
            };
            let Some(message) = message else {
                return TurnOutcome::Uncertain {
                    message: "The Codex process exited before the turn finished.".into(),
                };
            };
            let mut flag = |code: &'static str, reason: String| {
                if violation.is_none() {
                    violation = Some((code, reason));
                }
            };
            let (method, params) = match message {
                Incoming::Refused { method } => {
                    flag("policy_violation", format!("Codex requested {method}"));
                    (String::new(), Value::Null)
                }
                Incoming::Notification { method, params } => (method, params),
            };
            if !method.ends_with("/delta") {
                tracing::debug!(
                    %method,
                    item = params["item"]["type"].as_str().unwrap_or_default(),
                    tool = params["item"]["tool"].as_str().unwrap_or_default(),
                    "codex event"
                );
            }
            let thread = params["threadId"].as_str();
            let turn = params["turnId"].as_str().or(params["turn"]["id"].as_str());
            let other_thread = thread.is_some_and(|t| t != thread_id);
            let other_turn = turn.is_some_and(|t| t != native_turn);
            match method.as_str() {
                "thread/started" if params["thread"]["id"].as_str() != Some(thread_id) => {
                    flag(
                        "policy_violation",
                        "Codex started a native sub-agent thread".into(),
                    );
                }
                _ if other_thread || other_turn => {}
                "item/completed"
                    if params["item"]["type"] == "mcpToolCall"
                        && park_at.is_none()
                        && params["item"]["tool"].as_str().is_some_and(|t| {
                            crate::app::tools::parks(t, &params["item"]["result"])
                        }) =>
                {
                    park_at = Some(tokio::time::Instant::now() + PARK_GRACE);
                }
                "item/started" => {
                    let item = &params["item"];
                    let kind = item["type"].as_str().unwrap_or_default();
                    if FORBIDDEN_ITEMS.contains(&kind)
                        || (!host_tools && HOST_TOOL_ITEMS.contains(&kind))
                    {
                        flag("policy_violation", format!("Codex attempted a {kind} item"));
                    } else if kind == "mcpToolCall" && !self.allowed_tool(item) {
                        flag(
                            "policy_violation",
                            format!(
                                "Codex called an MCP tool outside else.bot's allowlist: {}/{}",
                                item["server"].as_str().unwrap_or("?"),
                                item["tool"].as_str().unwrap_or("?"),
                            ),
                        );
                    }
                }
                "model/rerouted" => {
                    let to = params["toModel"].as_str().unwrap_or_default();
                    if to != profile.model {
                        flag(
                            "profile_mismatch",
                            format!("Codex rerouted the turn to {to}."),
                        );
                    }
                }
                "thread/settings/updated" => {
                    let settings = &params["threadSettings"];
                    if let Some(mismatch) = Reported::from_response(settings).mismatch(profile) {
                        flag("profile_mismatch", mismatch);
                    }
                }
                "item/agentMessage/delta" => {
                    let item = params["itemId"].as_str().unwrap_or_default().to_owned();
                    if current_message.as_ref().is_some_and(|c| *c != item)
                        && sink.end_message().await.is_err()
                    {
                        cancel.cancel();
                    }
                    current_message = Some(item);
                    if let Some(delta) = params["delta"].as_str() {
                        if sink.text(delta).await.is_err() {
                            cancel.cancel();
                        }
                    }
                }
                "turn/completed" => {
                    if let Some((code, reason)) = violation {
                        return failed(code, reason);
                    }
                    let turn = &params["turn"];
                    return match turn["status"].as_str() {
                        Some("completed") => TurnOutcome::Completed,
                        Some("interrupted") => TurnOutcome::Interrupted,
                        _ => failed(
                            "provider_failed",
                            turn["error"]["message"]
                                .as_str()
                                .unwrap_or("Codex reported a failed turn.")
                                .to_owned(),
                        ),
                    };
                }
                _ => {}
            }
            if violation.is_some() && !interrupted {
                interrupted = true;
                interrupt(server, thread_id, native_turn).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::providers::{HistoryEntry, MemoryContext};
    use uuid::Uuid;

    fn input(history: Vec<(i64, &str, &str)>) -> TurnInput {
        TurnInput {
            bot_id: Uuid::new_v4(),
            conversation_id: Uuid::new_v4(),
            turn_id: Uuid::new_v4(),
            turn_no: 9,
            bot_name: "Work".into(),
            text: "latest".into(),
            parts: Vec::new(),
            files: Vec::new(),
            profile: None,
            history: history
                .into_iter()
                .map(|(turn_no, kind, text)| HistoryEntry {
                    turn_no,
                    kind: kind.into(),
                    text: text.into(),
                    status: "completed".into(),
                    parts: Vec::new(),
                })
                .collect(),
            session: None,
            tool_grant: None,
            memory: None,
            attempt: 1,
            delegation: Default::default(),
            secrets: Vec::new(),
            questions: Vec::new(),
            outgoing_drafts: Vec::new(),
            handoff: None,
            jobs: Vec::new(),
            routine: None,
        }
    }

    #[test]
    fn packet_includes_only_missed_history() {
        let input = input(vec![
            (1, "user", "a"),
            (1, "assistant", "b"),
            (2, "user", "c"),
        ]);
        assert_eq!(prompt::packet(&input, 2), "latest");
        let packet = prompt::packet(&input, 1);
        assert!(packet.contains("[turn 2] Owner: c") && !packet.contains("Owner: a"));
        assert!(packet.ends_with("Owner's latest message:\nlatest"));
        let full = prompt::packet(&input, 0);
        assert!(full.contains("[turn 1] Owner: a\n[turn 1] Assistant: b\n[turn 2] Owner: c"));
    }

    #[test]
    fn history_names_earlier_files_by_their_staged_copies() {
        use crate::app::messages::{Attachment, MessagePart};
        let file = Attachment {
            id: Uuid::nil(),
            name: "plan.pdf".into(),
            media_type: "application/pdf".into(),
            size: 9,
        };
        let mut input = input(vec![
            (1, "user", "Read [File: plan.pdf]"),
            (1, "assistant", "Done."),
        ]);
        input.history[0].parts = vec![
            MessagePart::Text {
                text: "Read ".into(),
            },
            MessagePart::File { file: file.clone() },
        ];
        let packet = prompt::packet(&input, 0);
        assert!(
            packet.contains(&format!(
                "[turn 1] Owner: Read [File: plan.pdf (application/pdf, 9 bytes), saved at .attachments/{}/plan.pdf]\n",
                Uuid::nil()
            )),
            "{packet}"
        );
        assert!(packet.ends_with("latest"));
    }

    #[test]
    fn packet_respects_the_budget_and_states_omissions() {
        let big = "x".repeat(20 * 1024);
        let input = input(vec![
            (1, "user", "first"),
            (2, "user", &big),
            (3, "user", &big),
        ]);
        let packet = prompt::packet(&input, 0);
        assert!(
            packet.contains("[turn 1] Owner: first"),
            "first owner message kept"
        );
        assert!(packet.contains("omitted"));
        assert!(packet.len() < prompt::HISTORY_BUDGET + 2048);
    }

    #[test]
    fn a_welcome_does_not_displace_the_first_owner_message() {
        let big = "x".repeat(20 * 1024);
        let input = input(vec![
            (1, "assistant", "Welcome to else.bot."),
            (2, "user", "Remember my original request."),
            (3, "assistant", &big),
            (4, "user", &big),
        ]);
        let (packet, selection) = prompt::packet_with_selection(&input, 0);
        assert_eq!(
            packet
                .matches("Owner: Remember my original request.")
                .count(),
            1
        );
        assert_eq!(selection.selected, vec![2, 4]);
        assert_eq!(selection.omitted, vec![1, 3]);
        assert!(packet.len() < prompt::HISTORY_BUDGET + 2048);
    }

    #[test]
    fn a_welcome_keeps_its_place_in_history() {
        let input = input(vec![
            (1, "assistant", "Welcome to else.bot."),
            (2, "user", "My original request."),
            (2, "assistant", "First reply."),
        ]);
        let (packet, selection) = prompt::packet_with_selection(&input, 0);
        assert!(packet.contains(
            "[turn 1] Assistant: Welcome to else.bot.\n[turn 2] Owner: My original request.\n[turn 2] Assistant: First reply."
        ));
        assert_eq!(selection.selected, vec![1, 2]);
        assert!(selection.omitted.is_empty());
    }

    #[test]
    fn a_large_first_message_still_fits_the_budget() {
        let first = "f".repeat(30 * 1024);
        let other = "o".repeat(10 * 1024);
        let input = input(vec![
            (1, "user", &first),
            (2, "user", &other),
            (3, "user", &other),
            (4, "user", &other),
        ]);
        let packet = prompt::packet(&input, 0);
        assert!(
            packet.len() <= prompt::HISTORY_BUDGET + 1024,
            "{}",
            packet.len()
        );
        assert!(packet.contains("omitted"));
    }

    #[test]
    fn reported_settings_are_checked_field_by_field() {
        let profile = Profile {
            model: "gpt-6.1-sol".into(),
            reasoning_effort: "medium".into(),
            service_tier: "priority".into(),
        };
        let partial =
            Reported::from_response(&json!({ "model": "gpt-6.1-sol", "reasoningEffort": null }));
        assert_eq!(
            partial.mismatch(&profile),
            None,
            "unknown fields are not mismatches"
        );
        let other_model =
            Reported::from_response(&json!({ "model": "gpt-5", "reasoningEffort": null }));
        assert!(other_model.mismatch(&profile).unwrap().contains("gpt-5"));
        let other_tier = Reported::from_response(&json!({ "serviceTier": "default" }));
        assert!(other_tier.mismatch(&profile).is_some());
    }

    #[test]
    fn catalog_offers_current_and_legacy_models_as_exact_profiles() {
        let codex = Codex::detect(None, None, None);
        assert_eq!(
            codex.catalog().featured_models,
            &["gpt-6.1-sol", "gpt-6-astra", "gpt-6.1-terra", "gpt-6-luna"]
        );
        for model in [
            "gpt-6.1-sol",
            "gpt-6-astra",
            "gpt-6.1-terra",
            "gpt-6-luna",
            "gpt-6-sol",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
        ] {
            assert!(codex.catalog().models.contains(&model), "missing {model}");
            for effort in codex.catalog().efforts {
                for tier in codex.catalog().tiers {
                    let profile = Profile {
                        model: model.into(),
                        reasoning_effort: (*effort).into(),
                        service_tier: (*tier).into(),
                    };
                    assert!(codex.validate_profile(&profile).is_ok(), "{profile:?}");
                }
            }
        }
    }

    #[test]
    fn profile_validation_refuses_delegating_effort() {
        let codex = Codex::detect(None, None, None);
        assert!(codex.unavailable.is_some());
        let ok = Profile {
            model: "gpt-6.1-sol".into(),
            reasoning_effort: "medium".into(),
            service_tier: "priority".into(),
        };
        assert!(codex.validate_profile(&ok).is_ok());
        for bad in [
            Profile {
                reasoning_effort: "ultra".into(),
                ..ok.clone()
            },
            Profile {
                model: "gpt-4".into(),
                ..ok.clone()
            },
            Profile {
                service_tier: "turbo".into(),
                ..ok.clone()
            },
        ] {
            assert!(codex.validate_profile(&bad).is_err(), "{bad:?}");
        }
        assert!(matches!(codex.availability(), Availability::Unavailable(_)));
    }

    #[test]
    fn fingerprint_changes_with_profile_and_name() {
        let a = input(vec![]);
        let profile = Profile {
            model: "gpt-6.1-sol".into(),
            reasoning_effort: "medium".into(),
            service_tier: "priority".into(),
        };
        let codex = Codex::detect(None, None, None);
        let base = codex.fingerprint(&profile, &a, "chatgpt:a");
        assert_ne!(
            base,
            codex.fingerprint(&profile, &a, "chatgpt:b"),
            "account"
        );
        let high = Profile {
            reasoning_effort: "high".into(),
            ..profile.clone()
        };
        assert_ne!(base, codex.fingerprint(&high, &a, "chatgpt:a"));
        let mut b = a.clone();
        b.bot_name = "Different".into();
        assert_ne!(base, codex.fingerprint(&profile, &b, "chatgpt:a"));
        assert_eq!(base, codex.fingerprint(&profile, &a.clone(), "chatgpt:a"));
        let dir = tempfile::tempdir().unwrap();
        let version = |v: &str| {
            detect_with(dir.path(), &format!("echo '{v}'"), true).fingerprint(
                &profile,
                &a,
                "chatgpt:a",
            )
        };
        assert_ne!(version("codex-cli 9.0.0"), version("codex-cli 9.0.1"));
    }

    fn detect_with(dir: &std::path::Path, script: &str, logged_in: bool) -> Codex {
        use std::os::unix::fs::PermissionsExt;
        let program = dir.join("codex");
        std::fs::write(&program, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let auth = dir.join("auth.json");
        if logged_in {
            std::fs::write(&auth, "{}").unwrap();
        } else {
            let _ = std::fs::remove_file(&auth);
        }
        Codex::detect(Some(Host::new(dir, dir)), Some(program), Some(auth))
    }

    fn fake_codex(dir: &std::path::Path) -> (Codex, std::path::PathBuf) {
        fake_codex_with(dir, &json!([]), None)
    }

    fn fake_codex_with(
        dir: &std::path::Path,
        servers: &Value,
        item_kind: Option<&str>,
    ) -> (Codex, std::path::PathBuf) {
        let received = dir.join("received.jsonl");
        let inputs = dir.join("inputs.jsonl");
        let threads = dir.join("threads.jsonl");
        let launched = dir.join("launched-config.toml");
        let launches = dir.join("launches.jsonl");
        let loaded = dir.join("loaded-catalog.json");
        let export_fails = dir.join("export-fails");
        let ignore_catalog = dir.join("ignore-catalog");
        let one_model = dir.join("one-model");
        let catalog = json!({
            "models": MODELS.iter().chain(&["hidden-model"]).map(|slug| json!({
                "slug": slug,
                "context_window": 272000,
                "base_instructions": "kept",
                "apply_patch_tool_type": "freeform",
                "shell_type": "unified_exec",
                "experimental_supported_tools": ["clock"],
            })).collect::<Vec<_>>(),
        });
        let catalog = format!("{:?}", catalog.to_string());
        let item = item_kind.map_or_else(
            || "None".to_owned(),
            |kind| format!("{:?}", json!({ "type": kind, "id": "item-1" }).to_string()),
        );
        let servers = format!("{:?}", servers.to_string());
        let script = format!(
            r#"python3 -c 'import json, sys; print(json.dumps(sys.argv[1:]))' "$@" >> {launches:?}
if [ "$1" = debug ]; then
    [ -e {export_fails:?} ] && exit 1
    exec python3 -c 'import json,os; data=json.loads({catalog}); data["models"]=data["models"][:1] if os.path.exists({one_model:?}) else data["models"]; print(json.dumps(data))'
fi
if [ "$1" = -c ] && [ "$3" = debug ]; then
    exec python3 -c 'import json,os,sys; data=json.loads({catalog}) if os.path.exists({ignore_catalog:?}) else json.load(open(json.loads(sys.argv[1].split("=",1)[1]))); data["models"]=data["models"][:1] if os.path.exists({one_model:?}) else data["models"]; print(json.dumps(data))' "$2"
fi
cat "$CODEX_HOME/config.toml" > {launched:?}
exec python3 -c '
import json, os, sys
def out(m): print(json.dumps(m), flush=True)
item = {item}
models = [m["slug"] for m in json.loads({catalog})["models"]]
if os.path.exists({one_model:?}): models = models[:1]
for flag, value in zip(sys.argv, sys.argv[1:]):
    if flag == "-c" and value.startswith("model_catalog_json="):
        catalog = open(json.loads(value.split("=", 1)[1])).read()
        open({loaded:?}, "w").write(catalog)
        if not os.path.exists({ignore_catalog:?}):
            models = [m["slug"] for m in json.loads(catalog)["models"]]
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m: continue
    method, p = m["method"], m.get("params") or {{}}
    if method == "mcpServerStatus/list": r = {{"data": json.loads({servers})}}
    elif method == "skills/list": r = {{"data": [{{"skills": []}}]}}
    elif method == "account/read": r = {{"account": {{"type": "chatgpt", "email": "o@example.test"}}}}
    elif method == "model/list": r = {{"data": [{{"id": s, "model": s}} for s in models], "nextCursor": None}}
    elif method in ("thread/start", "thread/resume"):
        with open({threads:?}, "a") as f: f.write(json.dumps(p) + "\n")
        r = {{"thread": {{"id": p.get("threadId", "fresh-thread")}}}}
    elif method == "turn/start":
        with open({received:?}, "a") as f: f.write(json.dumps(p["input"][0]["text"]) + "\n")
        with open({inputs:?}, "a") as f: f.write(json.dumps(p["input"]) + "\n")
        r = {{"turn": {{"id": "native-turn"}}}}
    else: r = {{}}
    out({{"id": m["id"], "result": r}})
    if method == "turn/start":
        if item:
            out({{"method": "item/started", "params": {{"threadId": p["threadId"],
                 "turnId": "native-turn", "item": json.loads(item)}}}})
        out({{"method": "turn/completed", "params": {{"threadId": p["threadId"],
             "turn": {{"id": "native-turn", "status": "completed"}}}}}})
' "$@""#
        );
        let codex = detect_with(
            dir,
            &format!("[ \"$1\" = --version ] && echo 'codex-cli 9.0.0' && exit\n{script}"),
            true,
        );
        assert!(matches!(codex.availability(), Availability::Available));
        (codex, received)
    }

    const TOOL_URL: &str = "http://127.0.0.1:1/tools/v1/mcp";

    fn elsebot_server() -> Value {
        let tools: serde_json::Map<String, Value> = crate::app::tools::TOOL_NAMES
            .iter()
            .map(|t| ((*t).to_owned(), json!({})))
            .collect();
        json!([{ "name": crate::app::tools::SERVER_NAME, "tools": tools }])
    }

    fn turn_from(origin: crate::app::providers::Origin) -> TurnInput {
        let mut turn = TurnInput {
            profile: Some(Profile {
                model: MODELS[0].into(),
                reasoning_effort: "medium".into(),
                service_tier: "default".into(),
            }),
            tool_grant: Some("tg_test".into()),
            ..input(Vec::new())
        };
        turn.delegation.origin = origin;
        turn
    }

    #[tokio::test]
    async fn a_connector_event_launches_without_host_tools_but_keeps_elsebot_tools() {
        use crate::app::providers::Origin;
        for (origin, host_tools) in [
            (Origin::ConnectorEvent, false),
            (Origin::OwnerRequest, true),
            (Origin::RoutineRun, true),
            (Origin::Other, true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (codex, _) = fake_codex_with(dir.path(), &elsebot_server(), None);
            let codex = codex.with_tools(TOOL_URL.into());
            let sink = super::super::recording::Recorded::default();
            let outcome = codex
                .run(turn_from(origin), &sink, CancellationToken::new())
                .await;
            assert_eq!(outcome, TurnOutcome::Completed, "{origin:?}");
            let launched =
                std::fs::read_to_string(dir.path().join("launched-config.toml")).unwrap();
            assert_eq!(
                launched,
                codex_home::config_toml(Some(TOOL_URL), host_tools),
                "{origin:?}"
            );
            assert!(
                launched.contains(&format!("shell_tool = {host_tools}\n")),
                "{origin:?}"
            );
            assert!(launched.contains("code_mode_host = true\n"), "{origin:?}");
            assert!(launched.contains("[mcp_servers.elsebot]"), "{origin:?}");
            let thread: Value = serde_json::from_str(
                std::fs::read_to_string(dir.path().join("threads.jsonl"))
                    .unwrap()
                    .lines()
                    .last()
                    .unwrap(),
            )
            .unwrap();
            let sandbox = if host_tools {
                "danger-full-access"
            } else {
                "read-only"
            };
            assert_eq!(thread["sandbox"], sandbox, "{origin:?}");
            assert_eq!(thread["approvalPolicy"], "never", "{origin:?}");
            let launches = launches(dir.path());
            if host_tools {
                assert_eq!(launches, vec![vec!["app-server".to_owned()]], "{origin:?}");
                assert!(!dir.path().join("loaded-catalog.json").exists());
            } else {
                assert_no_native_catalog(dir.path(), &launches, MODELS[0]);
            }
        }
    }

    fn launches(dir: &std::path::Path) -> Vec<Vec<String>> {
        std::fs::read_to_string(dir.join("launches.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn assert_no_native_catalog(dir: &std::path::Path, launches: &[Vec<String>], model: &str) {
        let [export, verified, launch] = launches else {
            panic!("{launches:?}");
        };
        assert_eq!(export, &["debug", "models"]);
        assert_eq!(&verified[..2], &launch[..2]);
        assert_eq!(&verified[2..], ["debug", "models"]);
        assert_eq!(launch.len(), 3, "{launch:?}");
        assert_eq!(launch[0], "-c");
        assert!(launch[1].starts_with("model_catalog_json=\""), "{launch:?}");
        assert_eq!(launch[2], "app-server");
        let loaded: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("loaded-catalog.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            loaded,
            json!({ "models": [{
                "slug": model,
                "context_window": 272000,
                "base_instructions": "kept",
                "apply_patch_tool_type": null,
                "shell_type": "disabled",
                "experimental_supported_tools": [],
                "supports_search_tool":false,
            }] })
        );
    }

    #[tokio::test]
    async fn a_connector_event_fails_before_its_turn_without_the_no_native_catalog() {
        use crate::app::providers::Origin;
        for (marker, one) in [
            ("export-fails", false),
            ("ignore-catalog", false),
            ("ignore-catalog", true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (codex, received) = fake_codex_with(dir.path(), &elsebot_server(), None);
            let codex = codex.with_tools(TOOL_URL.into());
            std::fs::write(dir.path().join(marker), "").unwrap();
            if one {
                std::fs::write(dir.path().join("one-model"), "").unwrap();
            }
            let sink = super::super::recording::Recorded::default();
            let outcome = codex
                .run(
                    turn_from(Origin::ConnectorEvent),
                    &sink,
                    CancellationToken::new(),
                )
                .await;
            let TurnOutcome::Failed { code, .. } = &outcome else {
                panic!("{marker}: {outcome:?}");
            };
            let expected = "provider_unavailable";
            assert_eq!(code, expected, "{marker}: {outcome:?}");
            assert!(!dir.path().join("threads.jsonl").exists(), "{marker}");
            assert!(!received.exists(), "{marker}");
            if marker == "export-fails" {
                assert_eq!(launches(dir.path()), vec![vec!["debug", "models"]]);
            }
        }
    }

    #[tokio::test]
    async fn summaries_and_checkpoints_run_on_the_no_native_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let (codex, _) = fake_codex(dir.path());
        let profile = turn_from(crate::app::providers::Origin::Other)
            .profile
            .unwrap();
        let result = codex
            .summarize_once(
                Uuid::new_v4(),
                &profile,
                "summarize",
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(result, Err("Codex returned an empty summary".into()));
        assert_no_native_catalog(dir.path(), &launches(dir.path()), &profile.model);

        let dir = tempfile::tempdir().unwrap();
        let (codex, _) = fake_codex(dir.path());
        std::fs::write(dir.path().join("ignore-catalog"), "").unwrap();
        let result = codex
            .summarize_once(
                Uuid::new_v4(),
                &profile,
                "summarize",
                &CancellationToken::new(),
            )
            .await;
        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.contains("catalog without native tools")),
            "{result:?}"
        );
        assert!(!dir.path().join("threads.jsonl").exists());
    }

    #[tokio::test]
    async fn a_connector_event_fails_on_any_command_or_file_change() {
        use crate::app::providers::Origin;
        for kind in ["commandExecution", "fileChange", "webSearch"] {
            let dir = tempfile::tempdir().unwrap();
            let (codex, _) = fake_codex_with(dir.path(), &elsebot_server(), Some(kind));
            let codex = codex.with_tools(TOOL_URL.into());
            let sink = super::super::recording::Recorded::default();
            let outcome = codex
                .run(
                    turn_from(Origin::ConnectorEvent),
                    &sink,
                    CancellationToken::new(),
                )
                .await;
            assert_eq!(
                outcome,
                failed("policy_violation", format!("Codex attempted a {kind} item")),
            );
        }
        for kind in ["commandExecution", "fileChange"] {
            let dir = tempfile::tempdir().unwrap();
            let (codex, _) = fake_codex_with(dir.path(), &elsebot_server(), Some(kind));
            let codex = codex.with_tools(TOOL_URL.into());
            let sink = super::super::recording::Recorded::default();
            let outcome = codex
                .run(
                    turn_from(Origin::OwnerRequest),
                    &sink,
                    CancellationToken::new(),
                )
                .await;
            assert_eq!(outcome, TurnOutcome::Completed, "{kind}");
        }
    }

    #[test]
    fn a_connector_event_never_resumes_a_thread_with_host_tools() {
        use crate::app::providers::Origin;
        let codex = Codex::detect(None, None, None).with_tools(TOOL_URL.into());
        let owner = turn_from(Origin::OwnerRequest);
        let event = TurnInput {
            delegation: crate::app::providers::DelegationContext {
                origin: Origin::ConnectorEvent,
                ..owner.delegation.clone()
            },
            ..owner.clone()
        };
        let profile = owner.profile.clone().unwrap();
        assert_ne!(
            codex.fingerprint(&profile, &owner, "a"),
            codex.fingerprint(&profile, &event, "a")
        );
    }

    #[tokio::test]
    async fn a_warm_thread_receives_memory_whenever_its_revision_changed() {
        let dir = tempfile::tempdir().unwrap();
        let (codex, received) = fake_codex(dir.path());
        let turn = |session| TurnInput {
            turn_no: 3,
            bot_name: "GitHub".into(),
            text: "Clean up the stale branches.".into(),
            profile: Some(Profile {
                model: MODELS[0].into(),
                reasoning_effort: "medium".into(),
                service_tier: "default".into(),
            }),
            session,
            memory: Some(MemoryContext {
                revision: 7,
                text: "[else.bot memory] Owner correction: rebase branches, never merge them.\n\n"
                    .into(),
            }),
            bot_id: Uuid::from_u128(7),
            ..input(Vec::new())
        };
        let base = turn(None);
        let fingerprint = codex.fingerprint(
            base.profile.as_ref().unwrap(),
            &base,
            "chatgpt:o@example.test",
        );
        let warm = |memory_revision| SessionState {
            native_id: "warm-thread".into(),
            fingerprint: fingerprint.clone(),
            considered_through_turn: 2,
            memory_revision,
        };
        let memory = "Owner correction: rebase branches, never merge them.";

        for (session, native, refreshed) in [
            (None, "fresh-thread", true),
            (Some(warm(Some(6))), "warm-thread", true),
            (Some(warm(Some(7))), "warm-thread", false),
        ] {
            let sink = super::super::recording::Recorded::default();
            let outcome = codex
                .run(turn(session), &sink, CancellationToken::new())
                .await;
            assert_eq!(outcome, TurnOutcome::Completed);
            let text = super::super::recording::received(&received).pop().unwrap();
            assert_eq!(text.contains(memory), refreshed, "{native}: {text}");
            assert!(text.ends_with("Clean up the stale branches."));
            let state = sink.last_session();
            assert_eq!(state.native_id, native);
            assert_eq!(state.memory_revision, Some(7));
        }
    }

    #[tokio::test]
    async fn files_reach_codex_in_order_with_images_as_local_images() {
        use crate::app::messages::{Attachment, MessagePart};
        use crate::app::providers::InputFile;
        let dir = tempfile::tempdir().unwrap();
        let (codex, _) = fake_codex(dir.path());
        let file = |name: &str, media_type: &str, data: &[u8]| InputFile {
            attachment: Attachment {
                id: Uuid::new_v4(),
                name: name.into(),
                media_type: media_type.into(),
                size: data.len() as u64,
            },
            data: data.into(),
        };
        let image = file("shot.png", "image/png", b"\x89PNG fake");
        let notes = file("notes.txt", "text/plain", b"line one\n");
        let text = |t: &str| MessagePart::Text { text: t.into() };
        let parts = vec![
            text("Compare "),
            MessagePart::File {
                file: image.attachment.clone(),
            },
            text(" with\n  "),
            MessagePart::File {
                file: notes.attachment.clone(),
            },
        ];
        let input = TurnInput {
            text: crate::app::messages::flatten(&parts),
            parts,
            files: vec![image.clone(), notes.clone()],
            profile: Some(Profile {
                model: MODELS[0].into(),
                reasoning_effort: "medium".into(),
                service_tier: "default".into(),
            }),
            ..input(Vec::new())
        };
        let bot_id = input.bot_id;
        let sink = super::super::recording::Recorded::default();
        let outcome = codex.run(input, &sink, CancellationToken::new()).await;
        assert_eq!(outcome, TurnOutcome::Completed);

        let project = dir.path().join(bot_id.to_string());
        let staged =
            |f: &InputFile| project.join(super::super::attachments::relative_path(&f.attachment));
        assert_eq!(std::fs::read(staged(&image)).unwrap(), b"\x89PNG fake");
        assert_eq!(std::fs::read(staged(&notes)).unwrap(), b"line one\n");
        let sent: Value = serde_json::from_str(
            std::fs::read_to_string(dir.path().join("inputs.jsonl"))
                .unwrap()
                .lines()
                .last()
                .unwrap(),
        )
        .unwrap();
        let canonical = |p: std::path::PathBuf| std::fs::canonicalize(p).unwrap();
        assert_eq!(sent[0]["type"], "text");
        assert_eq!(
            sent[0]["text"],
            format!("Compare {}", prompt::file_reference(&image.attachment))
        );
        assert_eq!(sent[1]["type"], "localImage");
        assert_eq!(
            canonical(sent[1]["path"].as_str().unwrap().into()),
            canonical(staged(&image))
        );
        assert_eq!(
            sent[2],
            json!({ "type": "text", "text": format!(" with\n  {}", prompt::file_reference(&notes.attachment)) })
        );
        assert_eq!(sent.as_array().unwrap().len(), 3);
    }

    #[test]
    fn any_running_logged_in_version_is_available() {
        let dir = tempfile::tempdir().unwrap();
        let codex = detect_with(dir.path(), "echo 'codex-cli 9.0.0'", true);
        let availability = codex.availability();
        assert!(
            matches!(availability, Availability::Available),
            "{availability:?}"
        );
        assert_eq!(codex.version, "codex-cli 9.0.0");
        for (script, logged_in) in [
            ("exit 1", true),
            ("true", true),
            ("echo 'codex-cli 9.0.0'", false),
        ] {
            assert!(
                matches!(
                    detect_with(dir.path(), script, logged_in).availability(),
                    Availability::Unavailable(_)
                ),
                "{script} {logged_in}"
            );
        }
    }
}
