//! The host Cursor adapter, over a Node sidecar that wraps `@cursor/sdk`
//! (see [`super::cursor_home`]), started in the bot's project directory.
//!
//! One sidecar runs per turn and speaks JSON-RPC over stdio through the
//! same client as Codex. The turn opens the bot's Cursor agent, resuming
//! the recorded one when its fingerprint matches and the SDK still has it,
//! and then sends the packet. The SDK takes no system prompt on most
//! accounts, so else.bot's instructions lead the packet whenever the agent
//! has not been given this exact text yet. No Cursor settings load
//! (`settingSources: []`), so no user, project or team rules, hooks, or MCP
//! servers reach the agent. Its tools are else.bot's MCP server, authorized
//! by the turn's grant, plus the host shell and file tools unless an
//! incoming event started the work; subagents (`task`) are always off. The
//! SDK has no approvals, so the tool list is the whole policy, and any
//! other tool the stream reports ends the turn as a policy failure. Stop
//! cancels the run; joined owner messages steer it.

mod protocol;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use uuid::Uuid;

use super::Vault;
use super::cli::attempt::{self, Channel, Ended, SendError};
use super::cli::session::{self, Identity, Opened, PreparedTurn};
use super::codex_rpc::{AppServer, Incoming, RpcError};
use super::definitions::Definition;
use super::host::{Host, LaunchSpec};
use super::models::{Listed, Models};
use super::{attachments, cursor_home, prompt};
use crate::app::providers::{
    Availability, Profile, Provider, Stop, TurnInput, TurnOutcome, TurnSink,
};
use protocol::Tracker;

pub const ID: &str = "cursor";
const PROMPT_VERSION: &str = "else-cursor-v1";
const INSTRUCTIONS: &str = "instructions";
const RPC_TIMEOUT: Duration = Duration::from_secs(60);
/// `run.steer` answers once the run has taken the message, which can take
/// a model step; no answer by then leaves its delivery unknown.
const STEER_TIMEOUT: Duration = Duration::from_secs(30);
const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);
const REFUSED: i64 = -32001;
const SIGNED_OUT: i64 = -32002;
const USAGE_LIMIT: i64 = -32003;

pub static DEFINITION: Definition = Definition {
    id: ID,
    label: "Cursor",
    fallback: &["composer-2", "claude-opus-4-8"],
    efforts: protocol::EFFORTS,
    tiers: protocol::TIERS,
    pinned: super::definitions::any_id,
    model_note: "",
    effort_note: "",
    tier_note: "",
    build,
};

fn build(wiring: &super::Wiring) -> super::Built {
    let found = super::executable(wiring.config.node_bin.as_deref(), "node");
    let cursor = Cursor {
        models: Models::new(&DEFINITION),
        ready: cursor_home::node(&wiring.host, found).map(|node| Ready {
            host: wiring.host.clone(),
            node,
            data_dir: wiring.config.data_dir.clone(),
        }),
        vault: wiring.vault.clone(),
        tool_url: Some(wiring.tool_url.clone()),
        installing: tokio::sync::Mutex::new(()),
    };
    super::Built {
        provider: std::sync::Arc::new(cursor),
        summarizer: None,
    }
}

struct Ready {
    host: Host,
    node: PathBuf,
    data_dir: PathBuf,
}

pub struct Cursor {
    models: Models,
    ready: Result<Ready, String>,
    vault: Option<Vault>,
    tool_url: Option<String>,
    installing: tokio::sync::Mutex<()>,
}

struct Sidecar {
    script: PathBuf,
    key: zeroize::Zeroizing<String>,
}

struct Account<'a> {
    ready: &'a Ready,
    key: &'a str,
    store: &'a std::path::Path,
}

impl Cursor {
    async fn sidecar(&self, ready: &Ready) -> Result<Sidecar, String> {
        let key = cursor_home::api_key(self.vault.as_ref()).await?;
        let _installing = self.installing.lock().await;
        // Its own task, so a caller that gives up (the model refresher's
        // timeout) never cuts an install off midway.
        let (host, node, data) = (
            ready.host.clone(),
            ready.node.clone(),
            ready.data_dir.clone(),
        );
        let script = tokio::spawn(async move { cursor_home::prepare(&host, &node, &data).await })
            .await
            .map_err(|e| format!("The Cursor SDK install stopped: {e}"))??;
        Ok(Sidecar { script, key })
    }

    /// `account` is the non-secret identity `initialize` reports: an agent
    /// never resumes under another account.
    fn fingerprint(
        &self,
        ready: &Ready,
        input: &TurnInput,
        profile: &Profile,
        account: &str,
    ) -> String {
        session::fingerprint(
            input,
            profile,
            &Identity {
                prompt_version: PROMPT_VERSION,
                version: cursor_home::SDK_VERSION,
                runtime: &ready.host.runtime(input.bot_id),
                tools: match (input.delegation.host_tools(), self.tool_url.is_some()) {
                    (true, true) => "tools:host+elsebot-v1",
                    (true, false) => "tools:host",
                    (false, true) => "tools:elsebot-v1",
                    (false, false) => "tools:none",
                },
                policy: &["agents:disabled", "settings:none", account],
            },
        )
    }

    /// The agent's options, sent again on every resume: the SDK keeps none
    /// of the tool restrictions.
    fn agent_options(&self, input: &TurnInput, profile: &Profile, cwd: &str) -> Value {
        let mut options = json!({
            "model": protocol::selection(profile),
            "mode": "agent",
            "tools": protocol::allowed_tools(input.delegation.host_tools(), self.tool_url.is_some()),
            "disallowedTools": protocol::DISALLOWED,
            "local": {
                "cwd": cwd,
                "settingSources": [],
                "sandboxOptions": { "enabled": false },
                "enableAgentRetries": true,
            },
        });
        if let (Some(url), Some(grant)) = (&self.tool_url, &input.tool_grant) {
            options["mcpServers"] = json!({
                crate::app::tools::SERVER_NAME: {
                    "type": "http",
                    "url": url,
                    "headers": { "Authorization": format!("Bearer {grant}") },
                },
            });
        }
        options
    }

    async fn converse(
        &self,
        server: &mut AppServer,
        account: Account<'_>,
        input: &TurnInput,
        profile: &Profile,
        sink: &dyn TurnSink,
        stop: &Stop,
    ) -> Ended {
        let open = async {
            let initialized = server
                .request("initialize", json!({ "apiKey": account.key }), RPC_TIMEOUT)
                .await?;
            let identity = initialized["account"].as_str().unwrap_or_default();
            let fingerprint = self.fingerprint(account.ready, input, profile, identity);
            let recorded = input
                .session
                .as_ref()
                .filter(|s| s.fingerprint == fingerprint);
            let params = json!({
                "agentId": recorded.map(|s| s.native_id.as_str()),
                "options": self.agent_options(input, profile, &server.cwd),
                "storeDir": account.store,
            });
            let opened = server.request("agent.open", params, RPC_TIMEOUT).await?;
            Ok::<_, RpcError>((fingerprint, recorded, opened))
        };
        let opened = tokio::select! {
            opened = open => opened,
            () = stop.escalated() => return Ended::Undelivered(TurnOutcome::Interrupted),
        };
        let (fingerprint, recorded, opened) = match opened {
            Ok(opened) => opened,
            Err(err) => return Ended::Undelivered(refusal(err)),
        };
        let Some(agent_id) = opened["agentId"].as_str() else {
            return Ended::Undelivered(failed(
                "provider_protocol",
                "Cursor opened an agent without an ID.",
            ));
        };
        let opened = match recorded {
            Some(session) if opened["resumed"] == true && session.native_id == agent_id => {
                Opened::Resumed(session)
            }
            _ => Opened::Fresh(agent_id.to_owned()),
        };
        if stop.is_requested() {
            return Ended::Undelivered(TurnOutcome::Interrupted);
        }
        let mut turn = PreparedTurn::new(input, fingerprint, opened);
        turn.lead_once(INSTRUCTIONS, instructions(input, &server.cwd));
        if turn.record(sink).await.is_err() || sink.sending().await.is_err() {
            return Ended::Undelivered(TurnOutcome::Interrupted);
        }
        let send = server.request(
            "run.send",
            json!({ "message": protocol::message(&turn.segments) }),
            RPC_TIMEOUT,
        );
        let sent = tokio::select! {
            sent = send => sent,
            () = stop.escalated() => {
                return Ended::Abandoned(TurnOutcome::Uncertain {
                    message: "Delivery to Cursor is unknown: the stop escalated during delivery."
                        .into(),
                });
            }
        };
        let run_id = match sent {
            Ok(response) => match response["runId"].as_str() {
                Some(id) => id.to_owned(),
                None => {
                    return Ended::Abandoned(TurnOutcome::Uncertain {
                        message: "Cursor accepted the turn without a run ID.".into(),
                    });
                }
            },
            Err(err @ RpcError::Remote { code, .. })
                if [REFUSED, SIGNED_OUT, USAGE_LIMIT].contains(&code) =>
            {
                return Ended::Undelivered(refusal(err));
            }
            Err(err) => {
                return Ended::Abandoned(TurnOutcome::Uncertain {
                    message: format!("Delivery to Cursor is unknown: {err}"),
                });
            }
        };
        let reported = json!({ "model": profile.model, "reasoning_effort": profile.reasoning_effort, "service_tier": profile.service_tier });
        if turn.accept(sink, &reported, &run_id).await.is_err() {
            stop.request();
        }
        let mut tracker = Tracker::new(
            profile,
            input.delegation.host_tools(),
            self.tool_url.is_some(),
        );
        let mut running = Running { server };
        attempt::drive(&mut running, &mut tracker, &mut turn, sink, stop, "Cursor").await
    }
}

/// The shared instructions, given in the packet: most accounts cannot
/// replace Cursor's system prompt.
fn instructions(input: &TurnInput, cwd: &str) -> String {
    format!(
        "[else.bot instructions]\n{}\n\nelse.bot's tools are on the MCP server named {}. End \
         this attempt when one of them returns parked or pending; the app continues it after the \
         outcome.\n[/else.bot instructions]",
        prompt::instructions(input, cwd),
        crate::app::tools::SERVER_NAME,
    )
}

fn refusal(err: RpcError) -> TurnOutcome {
    match err {
        RpcError::Remote { code, message, .. } if code == SIGNED_OUT => failed(
            "provider_unavailable",
            format!("{} ({message})", cursor_home::signed_out()),
        ),
        RpcError::Remote { code, message, .. } if code == USAGE_LIMIT => failed(
            "provider_unavailable",
            format!("Cursor's usage limit is reached: {message}"),
        ),
        RpcError::Remote { message, .. } => failed("provider_refused", message),
        err => failed("provider_unavailable", err.to_string()),
    }
}

struct Running<'s> {
    server: &'s mut AppServer,
}

#[async_trait]
impl Channel for Running<'_> {
    type Message = Incoming;

    async fn next(&mut self) -> Result<Option<Incoming>, String> {
        Ok(self.server.next().await)
    }

    async fn interrupt(&mut self) {
        let _ = self
            .server
            .request("run.cancel", json!({}), CANCEL_TIMEOUT)
            .await;
    }

    async fn send(&mut self, input: &Value) -> Result<(), SendError> {
        match self
            .server
            .request("run.steer", input.clone(), STEER_TIMEOUT)
            .await
        {
            Ok(response) if response["outcome"] == "complete_delivered" => Ok(()),
            Ok(response) if response["outcome"] == "revert_to_followup" => Err(SendError::Refused(
                "Cursor will take it as a follow-up".into(),
            )),
            Ok(response) => Err(SendError::Unknown(format!(
                "Cursor answered {}",
                response["outcome"]
            ))),
            Err(RpcError::Remote { code, message, .. }) if code == REFUSED => {
                Err(SendError::Refused(message))
            }
            Err(err) => Err(SendError::Unknown(err.to_string())),
        }
    }

    fn project(&self) -> Option<&std::path::Path> {
        Some(std::path::Path::new(&self.server.cwd))
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
        let sidecar = self.sidecar(ready).await?;
        let requests = format!(
            "{}\n{}\n",
            json!({ "id": 1, "method": "initialize", "params": { "apiKey": *sidecar.key } }),
            json!({ "id": 2, "method": "models.list" }),
        );
        let script = sidecar.script.to_string_lossy();
        let answer = ready
            .host
            .ask(
                &ready.node,
                &[&script],
                &[],
                &requests,
                16 * 1024 * 1024,
                |line| {
                    let message: Value = serde_json::from_str(line).ok()?;
                    match (&message["id"], message.get("error")) {
                        (id, Some(error)) if id == 1 || id == 2 => Some(Err(error.clone())),
                        (id, None) if id == 2 => Some(Ok(message["result"].clone())),
                        _ => None,
                    }
                },
            )
            .await
            .map_err(|why| format!("Cursor's models could not be listed: {why}"))?;
        match answer {
            Ok(result) => protocol::listed_models(&result["models"]),
            Err(error) if error["code"] == SIGNED_OUT => Err(cursor_home::signed_out()),
            Err(error) => Err(format!(
                "Cursor's models could not be listed: {}",
                error["message"].as_str().unwrap_or("unknown error")
            )),
        }
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
        if let Err(reason) = self.models.validate(&profile) {
            return failed("invalid_profile", reason);
        }
        let sidecar = tokio::select! {
            sidecar = self.sidecar(ready) => sidecar,
            () = stop.requested() => return TurnOutcome::Interrupted,
        };
        let sidecar = match sidecar {
            Ok(sidecar) => sidecar,
            Err(reason) => return failed("provider_unavailable", reason),
        };
        let dirs = match ready.host.bot_dirs(input.bot_id, ID) {
            Ok(dirs) => dirs,
            Err(e) => return failed("provider_unavailable", e.to_string()),
        };
        if let Err(e) = attachments::stage(&dirs.project, &input.files).await {
            return failed(
                "attachment_unavailable",
                format!("The attached files could not be prepared: {e}"),
            );
        }
        let store = match cursor_home::store_dir(&dirs.provider_home) {
            Ok(store) => store,
            Err(e) => return failed("provider_unavailable", e.to_string()),
        };
        let spec = LaunchSpec {
            turn_id: input.turn_id,
            program: ready.node.clone(),
            args: vec![OsString::from(&sidecar.script)],
            env: cursor_home::env(&dirs.provider_home),
            dirs,
        };
        let mut server = match AppServer::spawn(&ready.host, &spec, DEFINITION.label) {
            Ok(server) => server,
            Err(e) => return failed("provider_unavailable", e.to_string()),
        };
        let ended = self
            .converse(
                &mut server,
                Account {
                    key: &sidecar.key,
                    store: &store,
                    ready,
                },
                &input,
                &profile,
                sink,
                &stop,
            )
            .await;
        server.conclude(ended, sink).await
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
