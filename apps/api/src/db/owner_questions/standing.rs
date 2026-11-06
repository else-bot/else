use sqlx::SqlitePool;
use uuid::Uuid;

use super::{QuestionInfo, RequestError, find_same, info, insert, turns};
use crate::app::providers::Origin;
use crate::app::questions::{
    APPROVE_FOREVER, APPROVE_ROUTINE, APPROVE_TASK, APPROVE_TOOL, MAX_CONTEXT, PermissionTarget,
    Spec,
};

#[derive(Debug, thiserror::Error)]
pub enum ConfirmError {
    #[error(transparent)]
    Request(#[from] RequestError),
    #[error("quote the explicit authorization from this request's owner message")]
    NotAuthorized,
    #[error("list the routine again; it is missing, belongs to another bot, or its task changed")]
    RoutineChanged,
    #[error(
        "this message already authorized another task revision; a new owner message is required"
    )]
    AuthorizationUsed,
}

impl From<sqlx::Error> for ConfirmError {
    fn from(err: sqlx::Error) -> Self {
        Self::Request(err.into())
    }
}

pub struct Confirmed {
    pub question: QuestionInfo,
    pub active: bool,
}

pub async fn confirm(
    pool: &SqlitePool,
    turn_id: Uuid,
    target: PermissionTarget,
    authorization: &str,
) -> Result<Confirmed, ConfirmError> {
    let authorization = authorization.trim();
    let Some(target) = target.canonical() else {
        return Err(ConfirmError::NotAuthorized);
    };
    if authorization.is_empty() || authorization.len() > MAX_CONTEXT {
        return Err(ConfirmError::NotAuthorized);
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = turns::asking_caller(&mut tx, turn_id)
        .await?
        .ok_or(RequestError::NotRunning)?;
    if !caller.owner_conversation {
        return Err(RequestError::NotAllowed.into());
    }
    if !super::owner_message_quotes(&mut tx, turn_id, authorization).await? {
        return Err(ConfirmError::NotAuthorized);
    }
    if matches!(target, PermissionTarget::McpTool { .. })
        && !matches!(super::scope(&mut tx, turn_id).await?, super::Scope::Bot)
    {
        return Err(RequestError::NotAllowed.into());
    }
    if matches!(target, PermissionTarget::Task { .. })
        && (!matches!(super::scope(&mut tx, turn_id).await?, super::Scope::Bot)
            || crate::db::delegations::origin(&mut tx, turn_id).await? != Origin::OwnerRequest)
    {
        return Err(RequestError::NotAllowed.into());
    }
    if turns::attempt_parked(&mut tx, turn_id, caller.attempt).await? {
        return Err(RequestError::AttemptParked.into());
    }
    if let PermissionTarget::Routine {
        routine_id,
        prompt_revision,
    } = &target
    {
        let current: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM routines WHERE id = ?1 AND bot_id = ?2
             AND prompt_revision = ?3 AND deleted_at IS NULL)",
        )
        .bind(routine_id)
        .bind(caller.bot_id)
        .bind(prompt_revision)
        .fetch_one(&mut *tx)
        .await?;
        if !current {
            return Err(ConfirmError::RoutineChanged);
        }
    }
    let spec = Spec::standing_permission(target.clone(), authorization);
    if let Some(question) = find_same(&mut tx, turn_id, &spec).await? {
        if question.action.as_ref() != spec.approval.as_ref().map(|approval| &approval.action) {
            return Err(ConfirmError::AuthorizationUsed);
        }
        let active: bool = match &target {
            PermissionTarget::Bot
            | PermissionTarget::McpTool { .. }
            | PermissionTarget::Task { .. } => sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM bot_permissions WHERE bot_id = ?1 AND question_id = ?2)",
            )
            .bind(caller.bot_id)
            .bind(question.id)
            .fetch_one(&mut *tx)
            .await?,
            PermissionTarget::Routine { routine_id, .. } => sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM routine_grants WHERE routine_id = ?1 AND question_id = ?2)",
            )
            .bind(routine_id)
            .bind(question.id)
            .fetch_one(&mut *tx)
            .await?,
        };
        return Ok(Confirmed { question, active });
    }
    let question = insert(&mut tx, turn_id, &caller, &spec).await?;
    let answer = match &target {
        PermissionTarget::Bot => APPROVE_FOREVER,
        PermissionTarget::Routine { .. } => APPROVE_ROUTINE,
        PermissionTarget::McpTool { .. } => APPROVE_TOOL,
        PermissionTarget::Task { .. } => APPROVE_TASK,
    };
    let row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE owner_questions SET status = 'answered', answer = ?2,
             delivered_attempt = ?3, consumed_attempt = ?3,
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 RETURNING {}",
        super::COLUMNS
    )))
    .bind(question.id)
    .bind(answer)
    .bind(caller.attempt)
    .fetch_one(&mut *tx)
    .await?;
    match &target {
        PermissionTarget::Bot
        | PermissionTarget::McpTool { .. }
        | PermissionTarget::Task { .. } => {
            sqlx::query(
                "INSERT INTO bot_permissions (bot_id, scope, question_id) VALUES (?1, ?2, ?3)
                 ON CONFLICT (bot_id, scope) DO UPDATE SET question_id = excluded.question_id,
                     granted_at = excluded.granted_at",
            )
            .bind(caller.bot_id)
            .bind(target.bot_scope().unwrap())
            .bind(question.id)
            .execute(&mut *tx)
            .await?;
        }
        PermissionTarget::Routine {
            routine_id,
            prompt_revision,
        } => {
            sqlx::query(
                "INSERT INTO routine_grants (routine_id, prompt_revision, question_id) VALUES (?1, ?2, ?3)
                 ON CONFLICT (routine_id) DO UPDATE SET prompt_revision = excluded.prompt_revision,
                     question_id = excluded.question_id, granted_at = excluded.granted_at",
            )
            .bind(routine_id)
            .bind(prompt_revision)
            .bind(question.id)
            .execute(&mut *tx)
            .await?;
        }
    }
    let question = info(row)?;
    super::events::turn_changed(&mut tx, caller.bot_id, turn_id).await?;
    tx.commit().await?;
    Ok(Confirmed {
        question,
        active: true,
    })
}
