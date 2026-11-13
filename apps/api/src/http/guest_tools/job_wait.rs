//! `computer_job` with `wait`: park the attempt until a background job
//! ends, so the bot does not poll it.

use serde_json::json;
use uuid::Uuid;

use super::{failure, fit, job_json};
use crate::grants::Grant;
use crate::http::AppState;
use crate::http::mcp::{Outcome, tool_error, tool_result};

/// The attempt is read before the guest is probed, so a slow probe cannot
/// park a later attempt.
pub(super) async fn wait_job(
    state: &AppState,
    grant: &Grant,
    computers: &crate::computers::ComputerManager,
    id: Uuid,
) -> Outcome {
    use crate::db::computer_job_waits::{self as waits, CreateError, Created};
    let refused = |error: CreateError| -> Outcome {
        let code = match error {
            CreateError::AttemptParked => "ATTEMPT_PARKED",
            CreateError::NotRunning => "NOT_RUNNING",
            CreateError::NoSuchJob => "NO_SUCH_JOB",
            CreateError::Db => "UNAVAILABLE",
        };
        (
            tool_error(&format!("{code}: {error}")),
            Some(format!("computer_job wait {id} refused")),
        )
    };
    let caller = match crate::db::turns::running_caller(&state.db, grant.turn_id).await {
        Ok(Some(caller)) => caller,
        Ok(None) => return refused(CreateError::NotRunning),
        Err(err) => return refused(err.into()),
    };
    let ended = |job: crate::computers::jobs::JobState| -> Outcome {
        let mut value = job_json(&job);
        value["next"] = json!("The job already ended; nothing waits. Continue with this result.");
        let log = format!("computer_job wait {id}: already {}", job.status);
        (tool_result(fit(value)), Some(log))
    };
    match computers.job(grant.bot_id, id).await {
        Ok(Some(job)) if job.status.is_ended() => return ended(job),
        Ok(Some(_)) => {}
        Ok(None) => return refused(CreateError::NoSuchJob),
        Err(err) => return failure(err),
    }
    match waits::create(&state.db, grant.turn_id, caller.attempt, id).await {
        Ok(Created::Waiting(wait_id)) => {
            state.scheduler.grants().park(grant.turn_id).await;
            let next = if caller.owner_conversation {
                "Tell the owner briefly what the job does and end this attempt now. Do not poll \
                 or call more tools; else.bot continues this request when the job ends."
            } else {
                "End this attempt now without a reply. Do not poll or call more tools; else.bot \
                 continues this task when the job ends."
            };
            (
                tool_result(json!({
                    "status": "waiting",
                    "parked": true,
                    "job_id": id,
                    "wait_id": wait_id,
                    "next": next,
                })),
                Some(format!("computer_job wait {id}: waiting {wait_id}")),
            )
        }
        Ok(Created::Ended) => match computers.job(grant.bot_id, id).await {
            Ok(Some(job)) => ended(job),
            Ok(None) => refused(CreateError::NoSuchJob),
            Err(err) => failure(err),
        },
        Err(error) => refused(error),
    }
}
