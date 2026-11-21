//! `computer_job` with `wait`: park the attempt until a background job
//! ends, so the bot does not poll it.

use crate::http::tool_result::{Done, Refusal, ToolResult};
use serde_json::json;
use uuid::Uuid;

use super::{fit, job_json};
use crate::grants::Grant;
use crate::http::AppState;

/// The attempt is read before the guest is probed, so a slow probe cannot
/// park a later attempt.
pub(super) async fn wait_job(
    state: &AppState,
    grant: &Grant,
    computers: &crate::computers::ComputerManager,
    id: Uuid,
) -> ToolResult {
    use crate::db::computer_job_waits::{self as waits, CreateError, Created};
    let refused = |error: CreateError| -> ToolResult {
        let code = match error {
            CreateError::AttemptParked => "ATTEMPT_PARKED",
            CreateError::NotRunning => "NOT_RUNNING",
            CreateError::NoSuchJob => "NO_SUCH_JOB",
            CreateError::Db => "UNAVAILABLE",
        };
        Err(Refusal::text(format!("{code}: {error}"))
            .logged(format!("computer_job wait {id} refused")))
    };
    let caller = match crate::db::turns::running_caller(&state.db, grant.turn_id).await {
        Ok(Some(caller)) => caller,
        Ok(None) => return refused(CreateError::NotRunning),
        Err(err) => return refused(err.into()),
    };
    let ended = async |job: crate::computers::jobs::JobState| -> ToolResult {
        let mut value = job_json(&job);
        value["next"] = json!("The job already ended; nothing waits. Continue with this result.");
        if job.out_of_memory {
            value["out_of_memory"] = super::resize::out_of_memory(computers, grant.bot_id).await;
        }
        let log = format!("computer_job wait {id}: already {}", job.status);
        Ok(Done::json(fit(value)).logged(log))
    };
    match computers.job(grant.bot_id, id).await {
        Ok(Some(job)) if job.status.is_ended() => return ended(job).await,
        Ok(Some(_)) => {}
        Ok(None) => return refused(CreateError::NoSuchJob),
        Err(err) => return Err(err.into()),
    }
    let owner = caller.owner_conversation;
    let (status, wait_id, next) =
        match waits::create(&state.db, grant.turn_id, caller.attempt, id).await {
            Ok(Created::Waiting(wait_id)) if owner => (
                "waiting",
                wait_id,
                "Tell the owner briefly what the job does and end this attempt now. Do not poll or \
             call more tools; else.bot continues this request when the job ends.",
            ),
            Ok(Created::Waiting(wait_id)) => (
                "waiting",
                wait_id,
                "End this attempt now without a reply. Do not poll or call more tools; else.bot \
             continues this task when the job ends.",
            ),
            Ok(Created::Carried(wait_id)) if owner => (
                "already_waiting",
                wait_id,
                "This request already waits on this job; no new wait was added. Answer the owner \
             briefly and end this attempt now. Do not poll or call more tools; else.bot \
             continues this request when the job ends.",
            ),
            Ok(Created::Carried(wait_id)) => (
                "already_waiting",
                wait_id,
                "This task already waits on this job; no new wait was added. End this attempt now \
             without a reply. Do not poll or call more tools; else.bot continues this task \
             when the job ends.",
            ),
            Ok(Created::Ended) => {
                return match computers.job(grant.bot_id, id).await {
                    Ok(Some(job)) => ended(job).await,
                    Ok(None) => refused(CreateError::NoSuchJob),
                    Err(err) => Err(err.into()),
                };
            }
            Err(error) => return refused(error),
        };
    let parked = status == "waiting";
    if parked {
        state.grants.park(grant.turn_id).await;
    }
    Ok(Done::json(json!({
        "status": status,
        "parked": parked,
        "job_id": id,
        "wait_id": wait_id,
        "next": next,
    }))
    .logged(format!("computer_job wait {id}: {status} {wait_id}")))
}
