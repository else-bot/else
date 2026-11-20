//! `computer_resize`: the bot asks the owner, on an approval card, for more
//! memory after a command ran out of it. Once approved, a repeated call
//! restarts the computer at the new size and, when the card showed a failed
//! job, runs that job again and parks on it. A saved "always allow up to
//! N GiB" grant applies the size without a card, but never runs a job the
//! owner did not see.

use crate::http::owner_tools::{Card, Declined, ask};
use crate::http::tool_result::{self, Done, Refusal, ToolResult};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{AppState, Grant, secrets_for};
use crate::app::questions::{Kind, Spec, TaskScope};
use crate::computers::ComputerManager;
use crate::computers::jobs::{JobStatus, Rerun, Started};
use crate::computers::memory::{MEMORY_SIZES_MIB, MemorySizes, gib};
use crate::db::task_secrets;

pub(super) const NAME: &str = "computer_resize";
const MAX_REASON: usize = 300;
const GRANT_TASK: &str = "Raise this bot's computer memory";

fn grant_scope(memory_mib: u32) -> TaskScope {
    TaskScope::new(GRANT_TASK, &format!("up to {}", gib(memory_mib))).expect("a valid task scope")
}

/// What a failed command's result adds so the bot can ask for memory.
pub(super) async fn out_of_memory(computers: &ComputerManager, bot_id: Uuid) -> Value {
    let sizes = match computers.memory_sizes(bot_id).await {
        Ok(sizes) => sizes,
        Err(err) => {
            tracing::warn!(bot = %bot_id, error = %err, "reading computer memory failed");
            return json!({ "next": "This likely ran out of memory." });
        }
    };
    let at = gib(sizes.current_mib);
    let next = if sizes.offered_mib.is_empty() {
        format!(
            "This likely ran out of memory at {at}, and no larger size fits this host now. \
             Tell the owner; do not call computer_resize."
        )
    } else {
        format!(
            "This likely ran out of memory at {at}. Call computer_resize once with the smallest \
             offered size likely to work and, for a background job, retry_job set to its job_id."
        )
    };
    json!({ "memory_mib": sizes.current_mib, "sizes_mib": sizes.offered_mib, "next": next })
}

fn card(sizes: &MemorySizes, memory_mib: u32, reason: &str, rerun: Option<&Rerun>) -> String {
    let mut context = format!(
        "Memory: {} → {}\nWhy: {reason}\nThen: the computer restarts. Running jobs and \
         commands stop; files stay.",
        gib(sizes.current_mib),
        gib(memory_mib),
    );
    if let Some(rerun) = rerun {
        let room = crate::app::questions::MAX_CONTEXT.saturating_sub(context.len() + 64);
        let command = crate::http::mcp::slice(&rerun.command, 0, room).1;
        context.push_str(&format!("\nRe-runs this job after the restart:\n{command}"));
        if rerun.command.len() > command.len() {
            context.push('…');
        }
        if !rerun.secrets.is_empty() {
            context.push_str(&format!(
                "\n(uses the secrets: {})",
                rerun.secrets.join(", ")
            ));
        }
    }
    context
}

pub(super) async fn resize(
    state: &AppState,
    grant: &Grant,
    computers: &ComputerManager,
    args: &Value,
) -> ToolResult {
    let memory_mib = match args["memory_mib"].as_u64() {
        Some(size) if MEMORY_SIZES_MIB.iter().any(|s| u64::from(*s) == size) => size as u32,
        _ => return Err(Refusal::text("memory_mib is 2048, 4096, or 8192.")),
    };
    let reason = match args["reason"].as_str().map(str::trim) {
        Some(reason)
            if !reason.is_empty() && reason.len() <= MAX_REASON && !reason.contains('\0') =>
        {
            reason
        }
        _ => {
            return Err(Refusal::text(
                "reason is required: what ran out of memory, in at most 300 bytes.",
            ));
        }
    };
    let retry_job = match &args["retry_job"] {
        Value::Null => None,
        id => match id.as_str().and_then(|s| s.parse::<Uuid>().ok()) {
            Some(id) => Some(id),
            None => return Err(Refusal::text("retry_job must be a job ID.")),
        },
    };
    let sizes = match computers.memory_sizes(grant.bot_id).await {
        Ok(sizes) => sizes,
        Err(err) => return Err(err.into()),
    };
    if memory_mib <= sizes.current_mib {
        return Ok(Done::json(json!({
            "status": "unchanged",
            "memory_mib": sizes.current_mib,
            "next": format!(
                "Your computer already has {}. Nothing was changed or re-run.",
                gib(sizes.current_mib)
            ),
        }))
        .logged(format!("{NAME}: already {}", gib(sizes.current_mib))));
    }
    if !sizes.offered_mib.contains(&memory_mib) {
        let offered: Vec<String> = sizes.offered_mib.iter().map(|m| gib(*m)).collect();
        let message = if offered.is_empty() {
            format!(
                "SIZE_UNAVAILABLE: no size above {} fits this host now. Nothing was done. \
                 Tell the owner the work ran out of memory.",
                gib(sizes.current_mib)
            )
        } else {
            format!(
                "SIZE_UNAVAILABLE: {} does not fit this host now; offered: {}. Nothing was done.",
                gib(memory_mib),
                offered.join(", ")
            )
        };
        return Err(Refusal::text(message));
    }
    let rerun = match retry_job {
        None => None,
        Some(id) => match computers.rerun(grant.bot_id, id).await {
            Ok(Some(rerun)) if matches!(rerun.status, JobStatus::Failed | JobStatus::Lost) => {
                Some(rerun)
            }
            Ok(Some(_)) => {
                return Err(Refusal::text(
                    "retry_job must name a job that failed or was lost. Nothing was done.",
                ));
            }
            Ok(None) => {
                return Err(Refusal::text(
                    "NO_SUCH_JOB: retry_job names no job of yours.",
                ));
            }
            Err(err) => return Err(err.into()),
        },
    };
    let scope = covering_scope(state, grant, memory_mib).await;
    let spec = Spec::tool_approval(
        Kind::Destructive,
        NAME,
        args,
        Some(scope),
        format!("Raise this bot's computer memory to {}", gib(memory_mib)),
        card(&sizes, memory_mib, reason, rerun.as_ref()),
    );
    let card = Card::approval(format!(
        "Nothing was changed. The owner sees a card asking for {}. Tell them briefly what ran \
         out of memory, then end this attempt now; do not wait, poll, or call more tools. If \
         they approve, you continue: call {NAME} again with exactly the same arguments, once. \
         If they decline, report the failure.",
        gib(memory_mib)
    ))
    .not_allowed(
        Refusal::text(
            "NOT_ALLOWED: this task cannot ask the owner for memory. Nothing was changed. Report \
             that the work ran out of memory in your result.",
        )
        .logged("rejected NOT_ALLOWED"),
    );
    let declined = Declined::Text(format!(
        "DENIED: the owner kept the computer at {}. Nothing was changed. Do not ask again in \
         this request; report that the work ran out of memory.",
        gib(sizes.current_mib)
    ));
    let standing = ask(state, grant, &spec, card, declined).await?;
    let shown = standing.is_none() || card_shown(state, grant, &spec).await;
    let resized = match computers
        .resize_memory(grant.bot_id, Some(memory_mib))
        .await
    {
        Ok(resized) => resized,
        Err(err) => {
            tracing::warn!(bot = %grant.bot_id, error = %err, "resizing a computer failed");
            return Err(Refusal::text(format!(
                "RESIZE_FAILED: {err}. Tell the owner; do not retry the job."
            ))
            .logged(format!("{NAME} {} failed", gib(memory_mib))));
        }
    };
    let log = format!(
        "{NAME}: {} → {}",
        gib(sizes.current_mib),
        gib(resized.memory_mib)
    );
    let done = json!({
        "status": "resized",
        "memory_mib": resized.memory_mib,
        "restarted": resized.restarted,
    });
    let Some(rerun) = rerun else {
        let mut value = done;
        value["next"] = json!(format!(
            "Your computer now has {}. Run the work again.",
            gib(resized.memory_mib)
        ));
        return Ok(Done::json(value).logged(log));
    };
    if !shown && standing.is_some() {
        // The owner saw no card, so no command runs that they did not see.
        let mut value = done;
        value["next"] = json!(format!(
            "Your computer now has {} under the owner's saved grant, which does not re-run \
             jobs. Start the job again yourself with computer_job_start, with its usual effect.",
            gib(resized.memory_mib)
        ));
        return Ok(Done::json(value).logged(log));
    }
    retry(
        state,
        grant,
        computers,
        rerun,
        resized.memory_mib,
        done,
        log,
    )
    .await
}

/// Whether this attempt ran this exact call on a card the owner approved,
/// which "Always allow" also does while saving its grant.
async fn card_shown(state: &AppState, grant: &Grant, spec: &Spec) -> bool {
    let Some(approval) = &spec.approval else {
        return false;
    };
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM owner_questions q JOIN turns t ON t.id = q.turn_id
                        WHERE q.turn_id = ?1 AND q.action_hash = ?2
                          AND q.consumed_attempt = t.attempt)",
    )
    .bind(grant.turn_id)
    .bind(approval.hash.as_slice())
    .fetch_one(&state.db)
    .await
    .unwrap_or(false)
}

/// The saved grant this size would use: the smallest "up to" size at or above
/// it that the bot holds, else its own (which the card's Always allow saves).
async fn covering_scope(state: &AppState, grant: &Grant, memory_mib: u32) -> TaskScope {
    use crate::app::questions::{Covers, PermissionTarget};
    let Ok(mut conn) = state.db.acquire().await else {
        return grant_scope(memory_mib);
    };
    for size in MEMORY_SIZES_MIB.into_iter().filter(|s| *s >= memory_mib) {
        let scope = grant_scope(size);
        let target = PermissionTarget::Task {
            task: scope.task.clone(),
            target: scope.target.clone(),
            kind: Kind::Destructive,
            covers: Covers::Computer,
        };
        for key in target.covering_scopes() {
            if crate::db::owner_questions::has_permission(&mut conn, grant.bot_id, &key)
                .await
                .unwrap_or(false)
            {
                return scope;
            }
        }
    }
    grant_scope(memory_mib)
}

async fn retry(
    state: &AppState,
    grant: &Grant,
    computers: &ComputerManager,
    rerun: Rerun,
    memory_mib: u32,
    done: Value,
    log: String,
) -> ToolResult {
    let not_rerun = |why: &str| {
        let mut value = done.clone();
        value["next"] = json!(format!(
            "Your computer now has {}, but the job was not re-run: {why} Tell the owner.",
            gib(memory_mib)
        ));
        Ok(Done::json(value).logged(log.clone()))
    };
    let secrets = json!({ "secrets": rerun.secrets });
    let (env, names) = match secrets_for(state, grant, &secrets, task_secrets::Use::Job).await {
        Ok(found) => found,
        Err(error) => return not_rerun(&tool_result::first_text(error)),
    };
    let started = computers
        .start_job_with(
            grant.bot_id,
            Some(grant.turn_id),
            &rerun.command,
            rerun.purpose.as_deref(),
            rerun.max_duration,
            &env,
            &names,
            rerun.webhook,
        )
        .await;
    let job_id = match started {
        Ok(Started::Job(id)) => id,
        Ok(Started::TooMany) => return not_rerun("4 background jobs already run."),
        Ok(Started::NotReady(acquired)) => {
            return not_rerun(&tool_result::first_text(super::not_ready(&acquired)));
        }
        Ok(Started::Stopped) => return not_rerun("this turn was stopped."),
        Err(err) => return not_rerun(&format!("{err}.")),
    };
    let waited = super::job_wait::wait_job(state, grant, computers, job_id).await;
    let summary = format!(
        "Your computer now has {}; the job re-runs as job {job_id}.",
        gib(memory_mib)
    );
    tool_result::amend(
        waited,
        |payload| {
            let next = payload["next"].as_str().unwrap_or_default();
            payload["next"] = json!(format!("{summary} {next}"));
            payload["resized"] = done;
        },
        |wait_log| {
            Some(match wait_log {
                Some(wait) => format!("{log}; retry {job_id}; {wait}"),
                None => format!("{log}; retry {job_id}"),
            })
        },
    )
}
