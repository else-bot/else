mod protocol;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::host::{Host, LaunchSpec, Supervised};
use super::{attachments, grok_home, prompt};
use crate::app::providers::{
    Availability, Catalog, Profile, Provider, SessionState, TurnInput, TurnOutcome, TurnSink,
};
use protocol::{Events, Step, Tracker};

pub const ID: &str = "grok";
pub const MODELS: &[&str] = &["grok-4.7", "grok-4.7-build-fast", "grok-4.6"];
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];
const TIERS: &[&str] = &["default"];
const EXIT_GRACE: Duration = Duration::from_secs(5);
const INTERRUPT_GRACE: Duration = Duration::from_secs(15);
const TURN_DEADLINE: Duration = Duration::from_secs(30 * 60);

pub struct Grok {
    host: Option<Host>,
    program: Option<PathBuf>,
    login: Option<PathBuf>,
    version: String,
    unavailable: Option<String>,
    tool_url: Option<String>,
}

impl Grok {
    pub fn detect(host: Option<Host>, program: Option<PathBuf>, login: Option<PathBuf>) -> Self {
        let version = match (&host, &program) {
            (Some(host), Some(program)) => host.probe(program, &["--version"], &[]),
            _ => None,
        };
        let unavailable = if host.is_none() {
            Some("Grok CLI is not configured on this host.".into())
        } else if program.is_none() {
            Some("Grok CLI (`grok`) was not found on PATH.".into())
        } else if version.is_none() {
            Some("Grok CLI does not run (`grok --version` failed).".into())
        } else if !login
            .as_ref()
            .is_some_and(|dir| dir.join("auth.json").is_file())
        {
            Some("Grok CLI is not logged in on the host (run `grok login`).".into())
        } else {
            None
        };
        Self {
            host,
            program,
            login,
            version: version.unwrap_or_default(),
            unavailable,
            tool_url: None,
        }
    }

    pub fn with_tools(mut self, url: String) -> Self {
        self.tool_url = Some(url);
        self
    }

    fn fingerprint(&self, input: &TurnInput, profile: &Profile) -> String {
        let mut hash = Sha256::new();
        for part in [
            "else-grok-v1".to_owned(),
            self.version.clone(),
            self.host
                .as_ref()
                .map_or_else(String::new, |h| h.runtime(input.bot_id)),
            self.login
                .as_ref()
                .map_or_else(String::new, |p| p.to_string_lossy().into_owned()),
            profile.model.clone(),
            profile.reasoning_effort.clone(),
            profile.service_tier.clone(),
            input.bot_name.clone(),
            prompt::registry_text(input),
            format!(
                "host:{};mcp:{:?}",
                input.delegation.host_tools(),
                self.tool_url
            ),
            crate::app::tools::TOOL_NAMES.join("\0"),
        ] {
            hash.update(part.as_bytes());
            hash.update([0]);
        }
        hex::encode(hash.finalize())
    }
}

pub fn validate(profile: &Profile) -> Result<(), String> {
    if !MODELS.contains(&profile.model.as_str()) {
        return Err(format!(
            "Unsupported Grok model {}. Supported: {}.",
            profile.model,
            MODELS.join(", ")
        ));
    }
    if !EFFORTS.contains(&profile.reasoning_effort.as_str()) {
        return Err(format!(
            "Unsupported Grok reasoning effort {} for {}.",
            profile.reasoning_effort, profile.model
        ));
    }
    if !TIERS.contains(&profile.service_tier.as_str()) {
        return Err("Grok supports only the default service tier. Choose grok-4.7-build-fast for its fast model.".into());
    }
    Ok(())
}

#[async_trait]
impl Provider for Grok {
    fn id(&self) -> &'static str {
        ID
    }
    fn label(&self) -> &'static str {
        "Grok"
    }
    fn project_dir(&self, bot_id: Uuid) -> Option<PathBuf> {
        self.host.as_ref().map(|h| h.project_path(bot_id))
    }
    fn availability(&self) -> Availability {
        self.unavailable
            .clone()
            .map_or(Availability::Available, Availability::Unavailable)
    }
    fn uses_profile(&self) -> bool {
        true
    }
    fn validate_profile(&self, profile: &Profile) -> Result<(), String> {
        validate(profile)
    }
    fn catalog(&self) -> Catalog {
        Catalog {
            models: MODELS,
            efforts: EFFORTS,
            tiers: TIERS,
            ..Default::default()
        }
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
        let Some(profile) = &input.profile else {
            return failed(
                "provider_misconfigured",
                "No execution profile was snapshotted.",
            );
        };
        if let Err(reason) = validate(profile) {
            return failed("invalid_profile", reason);
        }
        let (Some(host), Some(program), Some(login)) = (&self.host, &self.program, &self.login)
        else {
            return failed("provider_unavailable", "Grok CLI is not configured.");
        };
        if cancel.is_cancelled() {
            return TurnOutcome::Interrupted;
        }
        let dirs = match host.bot_dirs(input.bot_id, ID) {
            Ok(dirs) => dirs,
            Err(e) => return failed("provider_unavailable", e.to_string()),
        };
        let fingerprint = self.fingerprint(&input, profile);
        let resumed = input.session.as_ref().filter(|s| {
            s.fingerprint == fingerprint
                && grok_home::session_exists(&dirs.provider_home, &dirs.project, &s.native_id)
        });
        let mut state = resumed.cloned().unwrap_or_else(|| SessionState {
            native_id: Uuid::new_v4().to_string(),
            fingerprint,
            considered_through_turn: 0,
            memory_revision: None,
            default_skills: Default::default(),
        });
        let inject = prompt::memory_for(input.memory.as_ref(), resumed);
        let skills = super::default_skills::for_session(resumed);
        let (segments, selection) =
            prompt::turn_content(&input, state.considered_through_turn, inject, &skills);
        let packet = prompt::segments_text(&segments);
        if sink.session(&state).await.is_err() || sink.packet(&selection, &packet).await.is_err() {
            return TurnOutcome::Interrupted;
        }
        if let Err(e) = attachments::stage(&dirs.project, &input.files).await {
            return failed("attachment_unavailable", e.to_string());
        }
        if let Err(e) = grok_home::prepare(
            &dirs,
            login,
            self.tool_url.as_deref(),
            &packet,
            input.delegation.host_tools(),
        ) {
            return failed("provider_unavailable", e.to_string());
        }
        let prompt_file = dirs.provider_home.join("prompt.txt");
        let system = format!(
            "{}\nUse inline use_tool calls with tool_name and tool_input. else.bot MCP tool names start with elsebot__. Do not use invocation files that hide tool_name. End this attempt when an else.bot tool returns parked or pending; the app will continue it after the outcome.",
            prompt::instructions(&input, &dirs.project.to_string_lossy())
        );
        let host_tools = input.delegation.host_tools();
        let mut tools = grok_home::MCP_TOOLS.to_vec();
        if host_tools {
            tools.extend(grok_home::HOST_TOOLS);
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
            prompt_file.into_os_string(),
            if resumed.is_some() {
                "--resume".into()
            } else {
                "--session-id".into()
            },
            state.native_id.clone().into(),
        ]);
        if host_tools {
            args.push("--always-approve".into());
        } else {
            args.extend(["--permission-mode".into(), "dontAsk".into()]);
        }
        let mut env = grok_home::env(&dirs.provider_home);
        if let Some(grant) = &input.tool_grant {
            env.push((grok_home::GRANT_ENV.into(), grant.clone()));
        }
        let spec = LaunchSpec {
            turn_id: input.turn_id,
            dirs,
            program: program.clone(),
            args,
            env,
        };
        let mut command = host.command(&spec);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if sink.sending().await.is_err() {
            return TurnOutcome::Interrupted;
        }
        let mut child = match Supervised::spawn(command) {
            Ok(child) => child,
            Err(e) => return failed("provider_unavailable", e.to_string()),
        };
        let stderr = child.child.stderr.take().expect("piped");
        let stderr = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut tail = Vec::new();
            let mut buf = [0; 4096];
            while let Ok(n) = reader.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > 8192 {
                    tail.drain(..tail.len() - 4096);
                }
            }
            String::from_utf8_lossy(&tail).into_owned()
        });
        let mut events = Events::new(BufReader::new(child.child.stdout.take().expect("piped")));
        let mut tracker = Tracker::new(
            &profile.model,
            &state.native_id,
            host_tools,
            self.tool_url.is_some(),
        );
        let deadline = tokio::time::Instant::now() + TURN_DEADLINE;
        let mut stop_by = None;
        let mut park_at = None;
        let mut deadline_hit = false;
        let mut outcome = 'events: loop {
            let wake = stop_by.unwrap_or_else(|| park_at.unwrap_or(deadline).min(deadline));
            let event = tokio::select! {
                _ = cancel.cancelled(), if stop_by.is_none() => {
                    interrupt(&child);
                    stop_by = Some(tokio::time::Instant::now() + INTERRUPT_GRACE);
                    continue;
                }
                _ = tokio::time::sleep_until(wake) => {
                    if stop_by.is_some() {
                        break TurnOutcome::Uncertain { message: "Grok did not confirm interruption.".into() };
                    }
                    deadline_hit = park_at.is_none();
                    interrupt(&child);
                    stop_by = Some(tokio::time::Instant::now() + INTERRUPT_GRACE);
                    continue;
                }
                event = events.next() => event,
            };
            let event = match event {
                Ok(Some(event)) => event,
                Ok(None) => {
                    break TurnOutcome::Uncertain {
                        message: "Grok exited before its terminal result.".into(),
                    };
                }
                Err(e) => break TurnOutcome::Uncertain { message: e },
            };
            let steps = match tracker.on(&event) {
                Ok(steps) => steps,
                Err(e) => break failed("policy_violation", e),
            };
            for step in steps {
                let fenced = match step {
                    Step::Accepted => {
                        if sink.running().await.is_err() {
                            true
                        } else {
                            state.considered_through_turn = input.turn_no;
                            state.default_skills = skills.applied.clone();
                            if let Some(memory) = inject {
                                state.memory_revision = Some(memory.revision);
                            }
                            sink.session(&state).await.is_err() || sink.reported(&json!({"model": profile.model, "reasoning_effort": null, "service_tier": null}), &state.native_id).await.is_err()
                        }
                    }
                    Step::Text(text) => sink.text(&text).await.is_err(),
                    Step::EndMessage => sink.end_message().await.is_err(),
                    Step::Parked => {
                        park_at
                            .get_or_insert(tokio::time::Instant::now() + Duration::from_secs(20));
                        false
                    }
                    Step::Result(result) => {
                        break 'events if deadline_hit {
                            failed("turn_deadline", "Grok exceeded the turn deadline.")
                        } else if result == TurnOutcome::Interrupted
                            && stop_by.is_none()
                            && !cancel.is_cancelled()
                        {
                            failed(
                                "provider_refused",
                                "Grok refused to execute a tool without additional permission.",
                            )
                        } else {
                            result
                        };
                    }
                };
                if fenced {
                    cancel.cancel();
                }
            }
        };
        if outcome == TurnOutcome::Completed {
            if tokio::time::timeout(EXIT_GRACE, child.child.wait())
                .await
                .is_err()
                || !host.live_processes(input.turn_id).is_empty()
            {
                child.terminate(Duration::ZERO).await;
            }
        } else {
            child.terminate(Duration::ZERO).await;
        }
        let cleanup_host = host.clone();
        let cleanup =
            tokio::task::spawn_blocking(move || cleanup_host.sweep_turn(input.turn_id)).await;
        if !matches!(cleanup, Ok(Ok(_))) {
            outcome = TurnOutcome::Uncertain {
                message: "Could not reap Grok's remaining processes.".into(),
            };
        }
        let tail = tokio::time::timeout(Duration::from_secs(2), stderr)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        if outcome != TurnOutcome::Completed {
            sink.retire_session().await;
        }
        match outcome {
            TurnOutcome::Uncertain { message } if !tail.trim().is_empty() => {
                TurnOutcome::Uncertain {
                    message: format!("{message} {}", tail.trim()),
                }
            }
            outcome => outcome,
        }
    }
}

fn interrupt(child: &Supervised) {
    // SAFETY: the supervised child owns this process group until it is reaped.
    if child.pid() > 0 {
        unsafe {
            libc::kill(-child.pid(), libc::SIGINT);
        }
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
