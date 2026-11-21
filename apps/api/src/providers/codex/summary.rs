use super::super::codex_rpc::AppServer;
use super::policy::{Allowed, Reported, TurnEvents};
use super::turn::Running;
use super::{Codex, ID, LaunchSpec, Profile, RPC_TIMEOUT, codex_home, prompt};
use crate::providers::cli::attempt::{self, Violation};
use async_trait::async_trait;
use serde_json::json;
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
        let ready = self.ready.as_ref().map_err(Clone::clone)?;
        let host = &ready.host;
        self.models.validate(profile)?;
        let dirs = host
            .bot_dirs(bot_id, "codex-summary")
            .map_err(|e| e.to_string())?;
        codex_home::prepare(&dirs, &ready.login, None, false).map_err(|e| e.to_string())?;
        let mut spec = LaunchSpec {
            turn_id: uuid::Uuid::new_v4(),
            program: ready.program.current().await?,
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
        let closed = server.close().await;
        let text = result?;
        closed.map(|()| text)
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
            .map_err(|reason| Violation::policy(reason).to_string())?;
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
        let project = std::path::PathBuf::from(&server.cwd);
        let mut events = TurnEvents {
            thread_id: &thread_id,
            native_turn: &native_turn,
            profile,
            allowed: Allowed::Messages,
            current_message: None,
            project: &project,
            steers: Default::default(),
        };
        let mut running = Running {
            server,
            thread_id: &thread_id,
            native_turn: &native_turn,
        };
        let (text, completed) = attempt::collect(&mut running, &mut events, "Codex").await?;
        match completed.status.as_deref() {
            Some("completed") if !text.trim().is_empty() => Ok(text.trim().to_owned()),
            Some("completed") => Err("Codex returned an empty summary".into()),
            other => Err(format!("the summary turn ended {}", other.unwrap_or("?"))),
        }
    }
}

#[async_trait]
impl crate::memory_worker::Summarizer for Codex {
    fn id(&self) -> &'static str {
        ID
    }

    fn available(&self) -> bool {
        self.ready.is_ok()
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
