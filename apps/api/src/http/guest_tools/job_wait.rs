//! `computer_job` with `wait`: park the attempt until a background job
//! ends, so the bot does not poll it.

use crate::http::tool_result::{Done, Refusal, ToolResult};
use serde_json::json;
use uuid::Uuid;

use super::{fit, job_json};
use crate::grants::Grant;
use crate::http::AppState;
use crate::http::waits::{Next, waited};

/// The attempt is read before the guest is probed, so a slow probe cannot
/// park a later attempt.
pub(super) async fn wait_job(
    state: &AppState,
    grant: &Grant,
    computers: &crate::computers::ComputerManager,
    id: Uuid,
) -> ToolResult {
    use crate::db::computer_job_waits::{self as waits, CreateError};
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
    let created = match waits::create(&state.db, grant.turn_id, caller.attempt, id).await {
        Ok(Some(created)) => created,
        Ok(None) => {
            return match computers.job(grant.bot_id, id).await {
                Ok(Some(job)) => ended(job).await,
                Ok(None) => refused(CreateError::NoSuchJob),
                Err(err) => Err(err.into()),
            };
        }
        Err(error) => return refused(error),
    };
    let mut value = waited(
        state,
        grant.turn_id,
        created,
        caller.owner_conversation,
        &NEXT,
    )
    .await;
    value["job_id"] = json!(id);
    let log = format!(
        "computer_job wait {id}: {} {}",
        created.status(),
        created.id()
    );
    Ok(Done::json(value).parked(created.parked()).logged(log))
}

const NEXT: Next = Next {
    waiting_owner: "Tell the owner briefly what the job does and end this attempt now. Do not poll \
                    or call more tools; else.bot continues this request when the job ends.",
    waiting_task: "End this attempt now without a reply. Do not poll or call more tools; else.bot \
                   continues this task when the job ends.",
    carried_owner: "This request already waits on this job; no new wait was added. Answer the \
                    owner briefly and end this attempt now. Do not poll or call more tools; \
                    else.bot continues this request when the job ends.",
    carried_task: "This task already waits on this job; no new wait was added. End this attempt \
                   now without a reply. Do not poll or call more tools; else.bot continues this \
                   task when the job ends.",
};
