use super::{AppServer, Codex, Incoming, LaunchSpec, Profile, Provider, codex_home, json, prompt};
use super::{ID, RPC_TIMEOUT, Reported, SHUTDOWN_GRACE};
use async_trait::async_trait;
use std::ffi::OsString;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const SUMMARY_DEADLINE: Duration = Duration::from_secs(180);

impl Codex {
    /// Run one summary attempt with no tools at all: no MCP server, no
    /// code-mode host, no shell, file edits, or agents (the catalog without
    /// native tools, as for connector events), in an ephemeral thread under a
    /// separate per-bot home, with the saved profile. Any tool, command,
    /// file, or agent activity, or any server request, rejects the output.
    pub async fn summarize_once(
        &self,
        bot_id: uuid::Uuid,
        profile: &Profile,
        prompt: &str,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        self.no_tools_once(
            bot_id,
            profile,
            prompt::MEMORY_SUMMARY_SYSTEM,
            prompt,
            SUMMARY_DEADLINE,
            cancel,
        )
        .await
    }

    pub(super) async fn no_tools_once(
        &self,
        bot_id: uuid::Uuid,
        profile: &Profile,
        system: &str,
        prompt: &str,
        deadline: Duration,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        if let Some(reason) = &self.unavailable {
            return Err(reason.clone());
        }
        let (Some(host), Some(program), Some(auth)) = (&self.host, &self.program, &self.auth)
        else {
            return Err("Codex is not configured.".into());
        };
        self.validate_profile(profile)?;
        let dirs = host
            .bot_dirs(bot_id, "codex-summary")
            .map_err(|e| e.to_string())?;
        codex_home::prepare(&dirs, auth, None, false).map_err(|e| e.to_string())?;
        let mut spec = LaunchSpec {
            turn_id: uuid::Uuid::new_v4(),
            program: program.clone(),
            args: Vec::new(),
            env: vec![(
                "CODEX_HOME".into(),
                dirs.provider_home.to_string_lossy().into_owned(),
            )],
            dirs,
        };
        spec.args = tokio::select! {
            _ = cancel.cancelled() => return Err("cancelled".to_owned()),
            args = codex_home::no_native_tools(host, &spec, &profile.model) => args?,
        };
        spec.args.push(OsString::from("app-server"));
        let mut server = AppServer::start(host, &spec)
            .await
            .map_err(|e| e.to_string())?;
        let result = tokio::select! {
            _ = cancel.cancelled() => Err("cancelled".to_owned()),
            outcome = tokio::time::timeout(
                deadline,
                self.summary_exchange(&mut server, profile, system, prompt),
            ) => outcome.unwrap_or_else(|_| Err("the summary exceeded its deadline".into())),
        };
        server.shutdown(SHUTDOWN_GRACE).await;
        result
    }

    async fn summary_exchange(
        &self,
        server: &mut AppServer,
        profile: &Profile,
        system: &str,
        prompt: &str,
    ) -> Result<String, String> {
        let status = server
            .request("mcpServerStatus/list", json!({}), RPC_TIMEOUT)
            .await
            .map_err(|e| e.to_string())?;
        self.check_servers(&status["data"], false)?;
        Self::check_catalog(server, &profile.model)
            .await
            .map_err(|reason| format!("policy_violation: {reason}"))?;
        let mut config = super::skills::config(server).await?;
        config["model_reasoning_effort"] = json!(profile.reasoning_effort);
        let started = server
            .request(
                "thread/start",
                json!({
                    "model": profile.model,
                    "serviceTier": profile.service_tier,
                    "cwd": server.cwd,
                    "approvalPolicy": "never",
                    "sandbox": "read-only",
                    "ephemeral": true,
                    "developerInstructions": system,
                    "config": config,
                }),
                RPC_TIMEOUT,
            )
            .await
            .map_err(|e| e.to_string())?;
        if let Some(mismatch) = Reported::from_response(&started).mismatch(profile) {
            return Err(mismatch);
        }
        let thread_id = started["thread"]["id"]
            .as_str()
            .ok_or("thread/start returned no thread ID")?
            .to_owned();
        let turn = server
            .request(
                "turn/start",
                json!({
                    "threadId": thread_id,
                    "model": profile.model,
                    "effort": profile.reasoning_effort,
                    "serviceTier": profile.service_tier,
                    "input": [{ "type": "text", "text": prompt }],
                }),
                RPC_TIMEOUT,
            )
            .await
            .map_err(|e| e.to_string())?;
        let native_turn = turn["turn"]["id"].as_str().unwrap_or_default().to_owned();
        let mut text = String::new();
        let mut violation: Option<String> = None;
        loop {
            let Some(message) = server.next().await else {
                return Err("Codex exited before the summary finished".into());
            };
            let (method, params) = match message {
                Incoming::Refused { method } => {
                    violation.get_or_insert(format!("Codex requested {method}"));
                    continue;
                }
                Incoming::Notification { method, params } => (method, params),
            };
            match method.as_str() {
                "thread/started" if params["thread"]["id"].as_str() != Some(&thread_id) => {
                    violation.get_or_insert("Codex started another thread".into());
                }
                "item/started" | "item/completed" => {
                    let item = &params["item"];
                    let kind = item["type"].as_str().unwrap_or_default();
                    if !matches!(kind, "userMessage" | "agentMessage" | "reasoning") {
                        violation.get_or_insert(format!("Codex produced a {kind} item"));
                    } else if method == "item/completed" && kind == "agentMessage" {
                        if item["phase"].as_str() != Some("commentary") {
                            text = item["text"].as_str().unwrap_or_default().to_owned();
                        }
                    }
                }
                "model/rerouted" if params["toModel"].as_str() != Some(&profile.model) => {
                    violation.get_or_insert("Codex rerouted the model".into());
                }
                "thread/settings/updated" => {
                    if let Some(mismatch) =
                        Reported::from_response(&params["threadSettings"]).mismatch(profile)
                    {
                        violation.get_or_insert(format!("profile_mismatch: {mismatch}"));
                    }
                }
                "turn/completed"
                    if params["turn"]["id"]
                        .as_str()
                        .is_none_or(|t| t == native_turn) =>
                {
                    if let Some(violation) = violation {
                        return Err(format!("policy_violation: {violation}"));
                    }
                    return match params["turn"]["status"].as_str() {
                        Some("completed") if !text.trim().is_empty() => Ok(text.trim().to_owned()),
                        Some("completed") => Err("Codex returned an empty summary".into()),
                        other => Err(format!("the summary turn ended {}", other.unwrap_or("?"))),
                    };
                }
                _ => {}
            }
        }
    }
}

#[async_trait]
impl crate::memory_worker::Summarizer for Codex {
    fn id(&self) -> &'static str {
        ID
    }

    fn available(&self) -> bool {
        self.unavailable.is_none() && self.host.is_some()
    }

    async fn summarize(
        &self,
        bot_id: uuid::Uuid,
        profile: &Profile,
        prompt: &str,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        self.summarize_once(bot_id, profile, prompt, cancel).await
    }
}
