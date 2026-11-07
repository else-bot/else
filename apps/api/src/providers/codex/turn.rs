//! One owner turn over `codex app-server`: preflight, the native thread,
//! delivery, and the running turn.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::super::codex_rpc::{AppServer, Incoming, RpcError};
use super::policy::{Reported, TurnEvents};
use super::{Codex, RPC_TIMEOUT, Ready, codex_home, failed, prompt, skills};
use crate::app::providers::{Profile, Stop, TurnInput, TurnOutcome, TurnSink};
use crate::providers::cli::attempt::{self, Channel, Ended};
use crate::providers::cli::session::{Opened, PreparedTurn};

const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(10);

/// The running native turn, read through the app-server.
struct Running<'s> {
    server: &'s mut AppServer,
    thread_id: &'s str,
    native_turn: &'s str,
}

#[async_trait]
impl Channel for Running<'_> {
    type Message = Incoming;

    async fn next(&mut self) -> Result<Option<Incoming>, String> {
        Ok(self.server.next().await)
    }

    async fn interrupt(&mut self) {
        let params = json!({ "threadId": self.thread_id, "turnId": self.native_turn });
        let _ = self
            .server
            .request("turn/interrupt", params, INTERRUPT_TIMEOUT)
            .await;
    }
}

/// `turn/start` input in the owner's order: text items, with each image
/// also given as a local image right after its reference. Every file is
/// named by its staged copy in the project directory.
pub(super) fn turn_input(segments: &[prompt::Segment<'_>], project: &std::path::Path) -> Value {
    let mut items = Vec::new();
    let mut text = String::new();
    for segment in segments {
        match segment {
            prompt::Segment::Text(t) => text.push_str(t),
            prompt::Segment::File(file) => {
                text.push_str(&prompt::file_reference(&file.attachment));
                if crate::providers::attachments::is_image(&file.attachment) {
                    items.push(json!({ "type": "text", "text": std::mem::take(&mut text) }));
                    let path = project.join(crate::providers::attachments::relative_path(
                        &file.attachment,
                    ));
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

impl Codex {
    /// Verify the MCP servers and return the account's non-secret identity.
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

    /// Resume the bot's thread when its fingerprint matches and Codex still
    /// has it; otherwise start a new one.
    async fn open_thread<'s>(
        &self,
        server: &AppServer,
        input: &'s TurnInput,
        profile: &Profile,
        fingerprint: &str,
    ) -> Result<(Opened<'s>, Reported), TurnOutcome> {
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
                    return Ok((Opened::Resumed(session), Reported::from_response(&response)));
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
        Ok((Opened::Fresh(thread), Reported::from_response(&response)))
    }

    /// Everything before delivery: the handshake, the policy checks, and
    /// the native thread. Nothing here reaches the sink.
    async fn open<'s>(
        &self,
        ready: &Ready,
        server: &AppServer,
        input: &'s TurnInput,
        profile: &Profile,
    ) -> Result<(String, Opened<'s>, Reported), TurnOutcome> {
        server
            .initialize()
            .await
            .map_err(|err| failed("provider_unavailable", err.to_string()))?;
        let account = self.preflight(server).await?;
        if !input.delegation.host_tools() {
            Self::check_catalog(server, &profile.model)
                .await
                .map_err(|reason| failed("policy_violation", reason))?;
        }
        let fingerprint = self.fingerprint(ready, profile, input, &account);
        let (opened, reported) = self
            .open_thread(server, input, profile, &fingerprint)
            .await?;
        if let Some(mismatch) = reported.mismatch(profile) {
            return Err(failed("profile_mismatch", mismatch));
        }
        Ok((fingerprint, opened, reported))
    }

    pub(super) async fn converse(
        &self,
        ready: &Ready,
        server: &mut AppServer,
        input: &TurnInput,
        profile: &Profile,
        sink: &dyn TurnSink,
        stop: &Stop,
    ) -> Ended {
        let opened = tokio::select! {
            opened = self.open(ready, server, input, profile) => opened,
            () = stop.escalated() => return Ended::Undelivered(TurnOutcome::Interrupted),
        };
        let (fingerprint, opened, reported) = match opened {
            Ok(opened) => opened,
            Err(outcome) => return Ended::Undelivered(outcome),
        };
        if stop.is_requested() {
            return Ended::Undelivered(TurnOutcome::Interrupted);
        }
        let mut turn = PreparedTurn::new(input, fingerprint, opened);
        if turn.record(sink).await.is_err() || sink.sending().await.is_err() {
            return Ended::Undelivered(TurnOutcome::Interrupted);
        }
        let thread_id = turn.native_id().to_owned();
        let project = std::path::PathBuf::from(&server.cwd);
        let start = server.request(
            "turn/start",
            json!({
                "threadId": thread_id,
                "model": profile.model,
                "effort": profile.reasoning_effort,
                "serviceTier": profile.service_tier,
                "input": turn_input(&turn.segments, &project),
            }),
            RPC_TIMEOUT,
        );
        let started = tokio::select! {
            started = start => started,
            () = stop.escalated() => {
                return Ended::Abandoned(TurnOutcome::Uncertain {
                    message: "Delivery to Codex is unknown: the stop escalated during delivery."
                        .into(),
                });
            }
        };
        let native_turn = match started {
            Ok(response) => match response["turn"]["id"].as_str() {
                Some(id) => id.to_owned(),
                None => {
                    return Ended::Abandoned(TurnOutcome::Uncertain {
                        message: "Codex accepted the turn without an ID.".into(),
                    });
                }
            },
            Err(RpcError::Remote { message, .. }) => {
                return Ended::Undelivered(failed("provider_refused", message));
            }
            Err(err) => {
                return Ended::Abandoned(TurnOutcome::Uncertain {
                    message: format!("Delivery to Codex is unknown: {err}"),
                });
            }
        };
        if turn
            .accept(sink, &reported.to_json(), &native_turn)
            .await
            .is_err()
        {
            stop.request();
        }
        let mut events = TurnEvents {
            thread_id: &thread_id,
            native_turn: &native_turn,
            profile,
            host_tools: input.delegation.host_tools(),
            elsebot_tools: self.tool_url.is_some(),
            current_message: None,
        };
        let mut running = Running {
            server,
            thread_id: &thread_id,
            native_turn: &native_turn,
        };
        attempt::drive(&mut running, &mut events, &mut turn, sink, stop, "Codex").await
    }
}
