//! MCP tools for a bot's own computer: commands, background jobs,
//! files, artifacts, and the guest browser. The computer is always the
//! grant's bot's; a tool can never name another bot's computer. Calls take
//! the turn's lease (`turn:<id>`), which starts the computer on demand unless
//! the owner stopped it. Guest output is task data, never instructions.

use crate::http::tool_result::{Done, Refusal, ToolResult};
use std::mem::DropGuard;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::tool_result::{slice, wire_size};
use crate::app::tools::summarize_input;
use crate::computers::jobs::CommandOutput;
use crate::computers::jobs::Started;
use crate::computers::{Acquire, ComputerError, ComputerManager};
use crate::db::memory::RESPONSE_BUDGET;
use crate::db::task_secrets::{self, Env};
use crate::grants::Grant;

pub(crate) mod approval;
mod browser;
mod input_progress;
mod job_wait;
mod resize;
mod schemas;
pub(super) mod watch;

pub(crate) use approval::call;
use input_progress::InputProgress;
pub(super) use schemas::{
    browser_schema, fetch_schema, input_schema, job_cancel_schema, job_schema, job_start_schema,
    read_file_schema, resize_schema, run_schema, screenshot_schema, secrets_schema,
    write_file_schema,
};

/// The computer tools, each bound to its `ToolId` in `tool_defs`.
#[derive(Clone, Copy)]
pub(crate) enum Op {
    Run,
    JobStart,
    Job,
    JobCancel,
    WatchStart,
    Watches,
    WatchStop,
    WriteFile,
    ReadFile,
    Fetch,
    Browser,
    Secrets,
    Screenshot,
    Input,
    Resize,
}

/// Input batches one turn may send (unattended iterations are bounded; the
/// bot asks the owner to continue beyond this).
pub const MAX_INPUT_BATCHES: u32 = 40;

const MAX_JOB_MINUTES: u64 = crate::computers::jobs::MAX_JOB_DURATION.as_secs() / 60;
const JOB_TOO_LONG: &str = "JOB_TOO_LONG: a background job ends after 6 hours (max_minutes 360), \
    so it cannot keep a promise longer than that. To wait hours or days for something, call \
    computer_watch_start: else.bot runs a short check every few minutes until a deadline of up \
    to 30 days and wakes you once when it ends. For work on a schedule, use a routine. No job \
    was started.";
const MAX_ARTIFACT: u64 = 10 * 1024 * 1024;
const READ_CHUNK: u64 = 12_000;
fn holder(grant: &Grant) -> String {
    format!("turn:{}", grant.turn_id)
}

fn not_ready(acquired: &Acquire) -> Refusal {
    if let Acquire::DesktopFailed { reason } = acquired {
        return Refusal::flagged(json!({
            "error": format!("DESKTOP_FAILED: {reason}. Nothing was done."),
            "next": "Screen, input and browser tools need the desktop. computer_run still \
                works: inspect the guest (its processes or logs), then retry, which starts \
                the desktop again.",
        }));
    }
    Refusal::text(match acquired {
        Acquire::OwnerStopped => {
            "COMPUTER_STOPPED: the owner stopped this computer; it does not start until resumed."
                .to_owned()
        }
        Acquire::Queued { reason } => format!("COMPUTER_BUSY: {reason}. Try again shortly."),
        Acquire::Unavailable { reason } => format!("COMPUTER_UNAVAILABLE: {reason}"),
        Acquire::BotInactive => "COMPUTER_UNAVAILABLE: this bot is not active.".to_owned(),
        Acquire::DesktopFailed { .. } | Acquire::Ready { .. } => "unexpected".to_owned(),
    })
}

impl From<ComputerError> for Refusal {
    fn from(err: ComputerError) -> Self {
        tracing::warn!(error = %err, "computer tool failed");
        Refusal::text(format!("COMPUTER_ERROR: {err}"))
    }
}

fn output_json(out: &CommandOutput) -> Value {
    json!({
        "exit_code": out.exit_code,
        "stdout": out.stdout,
        "stderr": out.stderr,
        "stdout_bytes": out.stdout_bytes,
        "stderr_bytes": out.stderr_bytes,
        "stdout_truncated": (out.stdout.len() as u64) < out.stdout_bytes,
        "stderr_truncated": (out.stderr.len() as u64) < out.stderr_bytes,
    })
}

fn fit(mut value: Value) -> Value {
    const KEEP_END: &[&str] = &["stdout", "stderr", "output_tail"];
    const FIELDS: &[&str] = &[
        "stdout",
        "stderr",
        "output_tail",
        "text",
        "content",
        "preview",
        "title",
        "url",
    ];
    for _ in 0..16 {
        let size = wire_size(&value);
        if size <= RESPONSE_BUDGET {
            break;
        }
        let Some(field) = FIELDS
            .iter()
            .filter(|f| value[**f].is_string())
            .max_by_key(|f| value[**f].as_str().map_or(0, str::len))
        else {
            break;
        };
        let text = value[*field].as_str().unwrap_or_default().to_owned();
        let keep = text.len().saturating_sub(size - RESPONSE_BUDGET + 64);
        let cut = if KEEP_END.contains(field) {
            slice(&text, text.len() - keep, keep).1.to_owned()
        } else {
            slice(&text, 0, keep).1.to_owned()
        };
        value[*field] = json!(cut);
        value[format!("{field}_truncated")] = json!(true);
    }
    value
}

/// The turn's lease on its computer; with `desktop`, its screen and browser
/// running too.
async fn lease(state: &AppState, grant: &Grant, desktop: bool) -> Result<Uuid, Refusal> {
    let Some(computers) = state.computers.manager() else {
        return Err(Refusal::text(
            "COMPUTER_UNAVAILABLE: no computer runtime on this host.",
        ));
    };
    let acquired = if desktop {
        computers
            .acquire_desktop(grant.bot_id, &holder(grant))
            .await
    } else {
        computers.acquire(grant.bot_id, &holder(grant)).await
    };
    match acquired {
        Ok(Acquire::Ready { lease_id, .. }) => Ok(lease_id),
        Ok(other) => Err(not_ready(&other)),
        Err(err) => Err(err.into()),
    }
}

/// Start the desktop for a shell command that names its helpers, as a best
/// effort: a failed startup lets the command run, so it can diagnose it.
async fn prepare_desktop(
    computers: &crate::computers::ComputerManager,
    grant: &Grant,
) -> Result<(), Refusal> {
    match computers
        .acquire_desktop(grant.bot_id, &holder(grant))
        .await
    {
        Ok(Acquire::Ready { .. } | Acquire::DesktopFailed { .. }) => Ok(()),
        Ok(other) => Err(not_ready(&other)),
        Err(err) => Err(err.into()),
    }
}

/// Whether a command names the guest's browser or screen helpers, so its
/// computer grows for the desktop before the command starts it.
fn wants_desktop(command: &str) -> bool {
    const HELPERS: [&str; 9] = [
        "else-desktop",
        "elsecdp",
        "cdp.mjs",
        "/usr/local/lib/elsebot/cdp",
        "else-browser",
        "else-screenshot",
        "else-input",
        "chromium",
        ":9222",
    ];
    HELPERS.iter().any(|h| command.contains(h))
}

fn job_json(job: &crate::computers::jobs::JobState) -> Value {
    json!({
        "job_id": job.id,
        "status": job.status,
        "exit_code": job.exit_code,
        "output_tail": job.output_tail,
        "command": job.command,
    })
}

async fn screenshot(
    state: &AppState,
    grant: &Grant,
    args: &Value,
    computers: &std::sync::Arc<crate::computers::ComputerManager>,
) -> ToolResult {
    use crate::computers::screens::ScreenRefusal;
    let refusal = |r: ScreenRefusal| -> ToolResult {
        Err(Refusal::text(match r {
            ScreenRefusal::DisplayUnavailable(m) | ScreenRefusal::TooLarge(m) => m,
            ScreenRefusal::Invalid(m) => format!("SCREENSHOT_INVALID: {m}"),
            ScreenRefusal::Stopped => {
                "STOPPED: this turn was stopped; nothing was captured.".into()
            }
            ScreenRefusal::NoSuchArtifact => {
                "No such screenshot: artifact_id must be one you saved.".into()
            }
        }))
    };
    let share = args["share"].as_bool().unwrap_or(false);
    if let Some(id) = args["artifact_id"].as_str() {
        let Ok(id) = id.parse::<Uuid>() else {
            return Err(Refusal::text("artifact_id must be an artifact ID."));
        };
        return match computers.saved_screen(grant.bot_id, id).await {
            Ok(Ok((png, meta))) => {
                let mut meta = if meta.is_object() { meta } else { json!({}) };
                meta["artifact_id"] = json!(id);
                meta["saved"] = json!(true);
                let log = format!("showed saved screenshot {id}");
                shown(state, grant, share, id, meta, &png, log).await
            }
            Ok(Err(r)) => refusal(r),
            Err(err) => Err(err.into()),
        };
    }
    let save = args["save"].as_bool().unwrap_or(false);
    let lease = lease(state, grant, true).await?;
    match computers
        .capture_screen(lease, grant.bot_id, Some(grant.turn_id), save)
        .await
    {
        Ok(Ok(shot)) => {
            let meta = &shot.meta;
            let log = format!(
                "screenshot {} {}x{} (image {}x{}, scale {}), {} bytes{}",
                meta.capture_id,
                meta.screen_width,
                meta.screen_height,
                meta.image_width,
                meta.image_height,
                meta.scale,
                meta.bytes,
                meta.artifact_id
                    .map(|a| format!(", saved as artifact {a}"))
                    .unwrap_or_default()
            );
            shown(
                state,
                grant,
                share,
                meta.capture_id,
                json!(meta),
                &shot.png,
                log,
            )
            .await
        }
        Ok(Err(r)) => refusal(r),
        Err(err) => Err(err.into()),
    }
}

/// The screenshot result, after copying the image into the bot's project
/// when `share` asks, where a reply link attaches it to the conversation.
async fn shown(
    state: &AppState,
    grant: &Grant,
    share: bool,
    id: Uuid,
    mut meta: Value,
    png: &[u8],
    mut log: String,
) -> ToolResult {
    if share {
        let rel = match shared(state, grant, id, png).await {
            Ok(rel) => rel,
            Err(message) => return Err(Refusal::text(message)),
        };
        meta["shared"] = json!(rel);
        meta["next"] = json!(format!(
            "Show it to the owner by writing ![screenshot]({rel}) in your reply."
        ));
        log.push_str(&format!(", shared as {rel}"));
    }
    Ok(Done::image(meta, png.to_vec()).logged(log))
}

/// Copy a screenshot to `screens/` in the bot's project and return the
/// path a reply links to show it.
pub(super) async fn shared(
    state: &AppState,
    grant: &Grant,
    id: Uuid,
    png: &[u8],
) -> Result<String, &'static str> {
    let project = state
        .registry
        .all()
        .iter()
        .find_map(|provider| provider.project_dir(grant.bot_id))
        .ok_or("SHARE_UNAVAILABLE: your project directory is unavailable.")?;
    let name = format!("{id}.png");
    let rel = format!("screens/{name}");
    let data = png.to_vec();
    let written = tokio::task::spawn_blocking(move || {
        crate::computers::screens::share(&project, &name, &data)
    })
    .await
    .map_err(std::io::Error::other)
    .and_then(|r| r);
    if let Err(err) = written {
        tracing::warn!(error = %err, "sharing a screenshot failed");
        return Err("SHARE_UNAVAILABLE: the screenshot could not be copied to your project.");
    }
    Ok(rel)
}

/// The bot's screen as an ID and PNG, for a handoff that shows the owner
/// what it asks about. `None` unless the desktop already runs: a handoff
/// never wakes a computer.
pub(super) async fn current_screen(state: &AppState, grant: &Grant) -> Option<(Uuid, Vec<u8>)> {
    let computers = state.computers.manager()?;
    let lease = computers
        .lease_running_desktop(grant.bot_id, &holder(grant))
        .await
        .ok()??;
    match computers
        .capture_screen(lease, grant.bot_id, Some(grant.turn_id), false)
        .await
    {
        Ok(Ok(shot)) => Some((shot.meta.capture_id, shot.png)),
        Ok(Err(_)) => None,
        Err(err) => {
            tracing::warn!(error = %err, "capturing the screen for a handoff failed");
            None
        }
    }
}

fn end_turn_on_drop(
    computers: &Arc<ComputerManager>,
    turn: Uuid,
) -> DropGuard<Arc<ComputerManager>, impl FnOnce(Arc<ComputerManager>)> {
    DropGuard::new(computers.clone(), move |computers| {
        tokio::spawn(async move { computers.end_turn_commands(turn).await });
    })
}

async fn input(
    _state: &AppState,
    grant: &Grant,
    args: &Value,
    computers: &std::sync::Arc<crate::computers::ComputerManager>,
) -> ToolResult {
    let (Some(frame), Some(actions)) = (args["frame"].as_array(), args["actions"].as_array())
    else {
        return Err(Refusal::text("frame and actions are required."));
    };
    if frame.len() != 2 || actions.is_empty() || actions.len() > 20 {
        return Err(Refusal::text(
            "frame is [width, height]; actions has 1 to 20 entries.",
        ));
    }
    if !computers.config().desktop {
        return Err(Refusal::text(
            "DISPLAY_UNAVAILABLE: this installation's computers have no desktop.",
        ));
    }
    let used = match computers.count_input(grant.turn_id).await {
        Ok(used) => used,
        Err(err) => return Err(err.into()),
    };
    if used > MAX_INPUT_BATCHES {
        return Err(Refusal::text(format!(
            "INPUT_LIMIT: this turn already sent {MAX_INPUT_BATCHES} input batches; stop and \
                 report to the owner, who can ask you to continue."
        )));
    }
    let batch = json!({ "frame": frame, "actions": actions }).to_string();
    if batch.len() > 16_000 {
        return Err(Refusal::text("the batch is over 16 KB."));
    }
    let env = Env {
        vars: vec![(
            "ELSE_INPUT_BATCH".to_owned(),
            zeroize::Zeroizing::new(batch),
        )],
    };
    let command = "DISPLAY=:99 exec /usr/local/bin/else-input".to_owned();
    let guard = end_turn_on_drop(computers, grant.turn_id);
    let run = {
        let computers = computers.clone();
        let (bot, holder) = (grant.bot_id, holder(grant));
        tokio::spawn(async move {
            computers
                .run_input(bot, &holder, &command, Duration::from_secs(150), &env)
                .await
        })
    };
    let finished = run.await;
    DropGuard::dismiss(guard);
    let out = match finished {
        Ok(Ok(Ok(out))) => out,
        Ok(Ok(Err(acquired))) => return Err(not_ready(&acquired)),
        Ok(Err(crate::computers::ComputerError::Refused(message))) => {
            return Err(Refusal::text(message));
        }
        Ok(Err(err)) => return Err(err.into()),
        Err(_) => {
            return Err(Refusal::text("COMPUTER_ERROR: the input did not report."));
        }
    };
    let progress = InputProgress::parse(&out.stdout, actions.len());
    let done = progress.done(actions.len());
    let summary = summarize_input(actions);
    let killed = out.exit_code.is_none_or(|code| code > 128);
    if matches!(progress, InputProgress::Refused { .. })
        || (matches!(progress, InputProgress::NotStarted) && !killed)
    {
        return Err(Refusal::text(format!(
            "{} Nothing was done.",
            progress
                .error()
                .unwrap_or("INPUT_FAILED: the input helper did not start.")
        )));
    }
    if out.exit_code == Some(0) && matches!(progress, InputProgress::Completed) {
        return Ok(Done::json(json!({ "done": done, "of": actions.len(),
                "next": "take a screenshot to see the result" }))
        .logged(format!("input {done}/{} actions: {summary}", actions.len())));
    }
    Err(Refusal::text(format!(
        "UNCERTAIN: {done} of {} actions completed; the next may or may not have happened \
             ({}). Take a screenshot before repeating anything.",
        actions.len(),
        progress.error().unwrap_or("the input was cut off")
    ))
    .logged(format!(
        "input cut off after {done}/{} actions: {summary}",
        actions.len()
    )))
}

/// The secrets a command or job names, decrypted, with the use recorded.
async fn secrets_for(
    state: &AppState,
    grant: &Grant,
    args: &Value,
    kind: task_secrets::Use,
) -> Result<(Env, Vec<String>), Refusal> {
    let names = secret_names(args)?;
    let env = resolve_secrets(state, &names).await?;
    task_secrets::record_use(&state.db, &names, grant.bot_id, Some(grant.turn_id), kind).await;
    Ok((env, names))
}

/// The Vault entry names in `args["secrets"]`, unchecked against the Vault.
fn secret_names(args: &Value) -> Result<Vec<String>, Refusal> {
    let names: Vec<String> = match &args["secrets"] {
        Value::Null => Vec::new(),
        Value::Array(items) => {
            let names: Option<Vec<String>> = items
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect();
            match names {
                Some(names) => names,
                None => return Err(Refusal::text("secrets must be a list of names.")),
            }
        }
        _ => return Err(Refusal::text("secrets must be a list of names.")),
    };
    let mut unique = names.clone();
    unique.sort();
    unique.dedup();
    if unique.len() != names.len() {
        return Err(Refusal::text("secrets names a secret twice."));
    }
    Ok(names)
}

/// Decrypt the Vault entries `names`, without recording a use.
async fn resolve_secrets(state: &AppState, names: &[String]) -> Result<Env, Refusal> {
    match task_secrets::resolve(&state.db, Some(&*state.secret_key), names).await {
        Ok(env) => Ok(env),
        Err(task_secrets::SecretError::Unknown(name)) => Err(Refusal::text(format!(
            "SECRET_UNKNOWN: there is no Vault entry named {name}; search computer_secrets. \
                 Ask the owner with request_secret if the task needs it."
        ))),
        Err(task_secrets::SecretError::Invalid(reason)) => {
            Err(Refusal::text(format!("SECRET_INVALID: {reason}")))
        }
        Err(err) => {
            tracing::warn!(error = %err, "task secrets unavailable");
            Err(Refusal::text(
                "SECRET_UNAVAILABLE: task secrets could not be read.",
            ))
        }
    }
}

/// Git reads these through `GIT_CONFIG_COUNT`: the first value clears any
/// helper configured for github.com, the second answers from the token
/// variable, so the token is in neither a command line nor a file.
const GIT_HELPER: [(&str, &str); 2] = [
    ("credential.https://github.com.helper", ""),
    (
        "credential.https://github.com.helper",
        "!f() { test \"$1\" = get && echo username=x-access-token && echo \"password=$ELSE_GITHUB_TOKEN\"; }; f",
    ),
];

fn github_env(token: zeroize::Zeroizing<String>) -> Vec<(String, zeroize::Zeroizing<String>)> {
    let mut vars = vec![
        ("ELSE_GITHUB_TOKEN".to_owned(), token),
        ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned().into()),
        (
            "GIT_CONFIG_COUNT".to_owned(),
            GIT_HELPER.len().to_string().into(),
        ),
    ];
    for (i, (key, value)) in GIT_HELPER.iter().enumerate() {
        vars.push((format!("GIT_CONFIG_KEY_{i}"), (*key).to_owned().into()));
        vars.push((format!("GIT_CONFIG_VALUE_{i}"), (*value).to_owned().into()));
    }
    vars
}

/// Adds the GitHub MCP's sign-in to a command's environment when the call
/// names one. It reaches the guest like a task secret. Its output is filtered
/// in the guest, but only a foreground command's output is redacted again on
/// the host. Sign-in uses the owner's connection card, as `mcp_request` does.
async fn add_github(
    state: &AppState,
    grant: &Grant,
    args: &Value,
    env: &mut Env,
) -> Result<(), Refusal> {
    let id = match &args["github"] {
        Value::Null => return Ok(()),
        Value::String(id) => match crate::mcp::PackageId::try_from(id.clone()) {
            Ok(id) => id,
            Err(_) => return Err(Refusal::text("github must be an installed MCP id.")),
        },
        _ => return Err(Refusal::text("github must be an installed MCP id.")),
    };
    if !grant.allows(crate::grants::Tool::Mcp) {
        return Err(Refusal::text(
            "GITHUB_UNAVAILABLE: this request cannot use shared sign-ins. Nothing was run.",
        ));
    }
    match state.mcps.git_token(&id).await {
        Ok(token) => {
            env.vars.extend(github_env(token));
            Ok(())
        }
        Err(crate::mcp::Error::AuthRequired(connection)) => Err(super::shared_mcp::auth_required(
            state,
            grant,
            &id,
            Some("Authenticate git with GitHub in this bot's computer."),
            *connection,
            "auth_required",
        )
        .await),
        Err(error) => Err(Refusal::text(format!(
            "GITHUB_UNAVAILABLE: {error} Nothing was run."
        ))),
    }
}

async fn execute(state: &AppState, grant: &Grant, op: Op, name: &str, args: &Value) -> ToolResult {
    let Some(computers) = state.computers.manager().cloned() else {
        return Err(Refusal::text(
            "COMPUTER_UNAVAILABLE: no computer runtime on this host.",
        ));
    };
    match op {
        Op::Run => run(state, grant, &computers, args).await,
        Op::JobStart => job_start(state, grant, &computers, args).await,
        Op::Resize => resize::resize(state, grant, &computers, args).await,
        Op::WatchStart => watch::call(state, grant, watch::Op::Start, args).await,
        Op::Watches => watch::call(state, grant, watch::Op::List, args).await,
        Op::WatchStop => watch::call(state, grant, watch::Op::Stop, args).await,
        Op::Job if args["wait"] == true => {
            let Some(id) = text(args, "job_id").and_then(|s| s.parse::<Uuid>().ok()) else {
                return Err(Refusal::text("job_id must be a job ID."));
            };
            job_wait::wait_job(state, grant, &computers, id).await
        }
        Op::Job | Op::JobCancel => job(grant, &computers, op, name, args).await,
        Op::WriteFile => write_file(state, grant, &computers, args).await,
        Op::ReadFile => read_file(state, grant, &computers, args).await,
        Op::Fetch => fetch(state, grant, &computers, args).await,
        Op::Browser => browser::browser(state, grant, &computers, args).await,
        Op::Screenshot => screenshot(state, grant, args, &computers).await,
        Op::Input => input(state, grant, args, &computers).await,
        Op::Secrets => secrets(state, args).await,
    }
}

/// A string argument.
fn text(args: &Value, key: &str) -> Option<String> {
    args[key].as_str().map(str::to_owned)
}

async fn run(
    state: &AppState,
    grant: &Grant,
    computers: &Arc<ComputerManager>,
    args: &Value,
) -> ToolResult {
    let Some(command) = text(args, "command") else {
        return Err(Refusal::text("command is required."));
    };
    let timeout = args["timeout_secs"].as_u64().unwrap_or(60).clamp(1, 300);
    if wants_desktop(&command) {
        prepare_desktop(computers, grant).await?;
    }
    let (mut env, _) = secrets_for(state, grant, args, task_secrets::Use::Command).await?;
    add_github(state, grant, args, &mut env).await?;
    let run = {
        let computers = computers.clone();
        let (bot, holder) = (grant.bot_id, holder(grant));
        let command = command.clone();
        tokio::spawn(async move {
            computers
                .run_command_with(bot, &holder, &command, Duration::from_secs(timeout), &env)
                .await
        })
    };
    let guard = end_turn_on_drop(computers, grant.turn_id);
    let finished = run.await;
    DropGuard::dismiss(guard);
    let ran = match finished {
        Ok(ran) => ran,
        Err(err) => {
            tracing::warn!(error = %err, "a computer command task failed");
            return Err(Refusal::text("COMPUTER_ERROR: the command did not report."));
        }
    };
    match ran {
        Ok(Ok(out)) => {
            let log = format!(
                "run exit {:?}: {}",
                out.exit_code,
                slice(&command, 0, 200).1
            );
            let mut value = fit(output_json(&out));
            if out.out_of_memory {
                value["out_of_memory"] = resize::out_of_memory(computers, grant.bot_id).await;
            }
            Ok(Done::json(value).logged(log))
        }
        Ok(Err(acquired)) => Err(not_ready(&acquired)),
        Err(err) => Err(err.into()),
    }
}

async fn job_start(
    state: &AppState,
    grant: &Grant,
    computers: &Arc<ComputerManager>,
    args: &Value,
) -> ToolResult {
    let Some(command) = text(args, "command") else {
        return Err(Refusal::text("command is required."));
    };
    let minutes = args["max_minutes"]
        .as_u64()
        .unwrap_or(60)
        .clamp(1, MAX_JOB_MINUTES);
    let webhook = match &args["webhook_id"] {
        Value::Null => None,
        id => match id.as_str().and_then(|s| s.parse::<Uuid>().ok()) {
            None => return Err(Refusal::text("webhook_id must be a webhook ID.")),
            Some(id) => match crate::db::webhooks::is_live(&state.db, grant.bot_id, id).await {
                Ok(true) => Some(id),
                Ok(false) => {
                    return Err(Refusal::text(
                        "NOT_FOUND: no such webhook; see webhook_list. No job was started.",
                    ));
                }
                Err(err) => return Err(ComputerError::from(err).into()),
            },
        },
    };
    if wants_desktop(&command) {
        prepare_desktop(computers, grant).await?;
    }
    let (mut env, names) = secrets_for(state, grant, args, task_secrets::Use::Job).await?;
    add_github(state, grant, args, &mut env).await?;
    match computers
        .start_job_with(
            grant.bot_id,
            Some(grant.turn_id),
            &command,
            args["purpose"].as_str().map(str::trim),
            Duration::from_secs(minutes * 60),
            &env,
            &names,
            webhook,
        )
        .await
    {
        Ok(Started::Job(id)) => {
            let job = computers.job(grant.bot_id, id).await.ok().flatten();
            let value = job
                .as_ref()
                .map(job_json)
                .unwrap_or(json!({ "job_id": id }));
            Ok(
                Done::json(value)
                    .logged(format!("job {id} started: {}", slice(&command, 0, 200).1)),
            )
        }
        Ok(Started::TooMany) => Err(Refusal::text(
            "JOB_LIMIT: 4 background jobs already run; wait or cancel one.",
        )),
        Ok(Started::NotReady(acquired)) => Err(not_ready(&acquired)),
        Ok(Started::Stopped) => Err(Refusal::text(
            "STOPPED: this turn was stopped; no job was started.",
        )),
        Err(err) => Err(err.into()),
    }
}

async fn job(
    grant: &Grant,
    computers: &Arc<ComputerManager>,
    op: Op,
    name: &str,
    args: &Value,
) -> ToolResult {
    let Some(id) = text(args, "job_id").and_then(|s| s.parse::<Uuid>().ok()) else {
        return Err(Refusal::text("job_id must be a job ID."));
    };
    let result = if matches!(op, Op::Job) {
        computers.job(grant.bot_id, id).await
    } else {
        computers.cancel_job(grant.bot_id, id).await
    };
    match result {
        Ok(Some(job)) => {
            let mut value = fit(job_json(&job));
            if job.out_of_memory {
                value["out_of_memory"] = resize::out_of_memory(computers, grant.bot_id).await;
            }
            Ok(Done::json(value).logged(format!("{name} {id}: {}", job.status)))
        }
        Ok(None) => Err(Refusal::text("No such job.")),
        Err(err) => Err(err.into()),
    }
}

async fn write_file(
    state: &AppState,
    grant: &Grant,
    computers: &Arc<ComputerManager>,
    args: &Value,
) -> ToolResult {
    let (Some(path), Some(content)) = (text(args, "path"), text(args, "content")) else {
        return Err(Refusal::text("path and content are required."));
    };
    if content.len() > 65_536 {
        return Err(Refusal::text("content is over 64 KB."));
    }
    let lease = lease(state, grant, false).await?;
    match computers.write_file(lease, &path, &content).await {
        Ok(out) if out.status == Some(0) => {
            Ok(Done::json(json!({ "path": path, "bytes": content.len() }))
                .logged(format!("wrote {} bytes to {path}", content.len())))
        }
        Ok(out) => Err(Refusal::text(format!(
            "write failed: {}",
            slice(&out.stderr, 0, 500).1
        ))),
        Err(err) => Err(err.into()),
    }
}

async fn read_file(
    state: &AppState,
    grant: &Grant,
    computers: &Arc<ComputerManager>,
    args: &Value,
) -> ToolResult {
    let Some(path) = text(args, "path") else {
        return Err(Refusal::text("path is required."));
    };
    let offset = args["offset"].as_u64().unwrap_or(0);
    let lease = lease(state, grant, false).await?;
    match computers.read_file(lease, &path, offset, READ_CHUNK).await {
        Ok(Ok((size, bytes))) => {
            let (text, consumed, binary) = match std::str::from_utf8(&bytes) {
                Ok(text) => (text.to_owned(), bytes.len(), false),
                Err(err) if err.error_len().is_none() => {
                    let valid = err.valid_up_to();
                    (
                        String::from_utf8_lossy(&bytes[..valid]).into_owned(),
                        valid,
                        false,
                    )
                }
                Err(_) => {
                    use base64::Engine;
                    (
                        base64::engine::general_purpose::STANDARD.encode(&bytes),
                        bytes.len(),
                        true,
                    )
                }
            };
            let end = offset + consumed as u64;
            Ok(Done::json(fit(json!({
                "path": path, "size_bytes": size, "offset": offset,
                "encoding": if binary { "base64" } else { "utf-8" },
                "content": text,
                "next_offset": (end < size).then_some(end),
            })))
            .logged(format!("read {path} at {offset}")))
        }
        Ok(Err(reason)) => Err(Refusal::text(format!("read failed: {reason}"))),
        Err(err) => Err(err.into()),
    }
}

async fn fetch(
    state: &AppState,
    grant: &Grant,
    computers: &Arc<ComputerManager>,
    args: &Value,
) -> ToolResult {
    let Some(path) = text(args, "path") else {
        return Err(Refusal::text("path is required."));
    };
    let lease = lease(state, grant, false).await?;
    match computers
        .fetch_artifact(
            lease,
            grant.bot_id,
            Some(grant.turn_id),
            &path,
            MAX_ARTIFACT,
        )
        .await
    {
        Ok(Ok((id, size, sha256, preview))) => Ok(Done::json(fit(json!({
            "artifact_id": id, "guest_path": path, "size_bytes": size,
            "sha256": sha256, "preview": slice(&preview, 0, 2000).1,
        })))
        .logged(format!("fetched {path}: artifact {id}, {size} bytes"))),
        Ok(Err(reason)) => Err(Refusal::text(format!("fetch failed: {reason}"))),
        Err(err) => Err(err.into()),
    }
}

async fn secrets(state: &AppState, args: &Value) -> ToolResult {
    match task_secrets::list(&state.db).await {
        Ok(list) => {
            let query = text(args, "query").unwrap_or_default();
            let secrets: Vec<Value> = list
                .iter()
                .filter(|s| !task_secrets::provider_only(&s.name) && s.matches(&query))
                .map(|s| {
                    json!({
                        "name": s.name,
                        "description": s.description,
                        "website": s.website,
                        "env": s.env,
                        "last_used_at": s.last_used.as_ref().map(|u| u.at),
                    })
                })
                .collect();
            let log = format!("found {} vault entries", secrets.len());
            let next = if secrets.is_empty() {
                "Nothing matches. Call request_secret with the website to have the owner save \
             it, instead of asking in chat or handing over the computer."
            } else {
                "Sign in with computer_browser type, secret and field per input; read \
             variables from the environment in commands."
            };
            Ok(Done::json(json!({ "secrets": secrets, "next": next })).logged(log))
        }
        Err(err) => {
            tracing::warn!(error = %err, "listing task secrets failed");
            Err(Refusal::text(
                "SECRET_UNAVAILABLE: task secrets could not be read.",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_are_fitted_to_the_budget_after_escaping() {
        let value = json!({ "stdout": "\"".repeat(20_000), "stderr": "ok", "exit_code": 0 });
        let fitted = fit(value);
        assert!(wire_size(&fitted) <= RESPONSE_BUDGET);
        assert_eq!(fitted["stdout_truncated"], true);
        assert_eq!(fitted["stderr"], "ok");
        let small = json!({ "text": "hello" });
        assert_eq!(fit(small.clone()), small);
    }

    fn credential(token: &str, host: &str) -> std::process::Output {
        use std::io::Write;
        let home = tempfile::tempdir().unwrap();
        let mut git = std::process::Command::new("git");
        git.args(["credential", "fill"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (name, value) in github_env(zeroize::Zeroizing::new(token.to_owned())) {
            git.env(name, value.as_str());
        }
        let mut child = git.spawn().unwrap();
        write!(
            child.stdin.take().unwrap(),
            "protocol=https\nhost={host}\n\n"
        )
        .unwrap();
        child.wait_with_output().unwrap()
    }

    #[test]
    fn git_answers_github_with_the_token_and_prompts_for_nothing_else() {
        let github = credential("gho_example-token", "github.com");
        assert!(github.status.success(), "{github:?}");
        let answer = String::from_utf8(github.stdout).unwrap();
        assert!(answer.contains("username=x-access-token\n"), "{answer}");
        assert!(answer.contains("password=gho_example-token\n"), "{answer}");

        let other = credential("gho_example-token", "gitlab.com");
        assert!(!other.status.success(), "{other:?}");
        assert!(!String::from_utf8_lossy(&other.stdout).contains("gho_example-token"));
    }
}
