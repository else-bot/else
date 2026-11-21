//! Authorizing mail commands and recording mail actions in the shared ledger.

use super::*;

/// The mail action table's own columns.
pub struct MailAction {
    generation: i64,
    operation: String,
}

impl Columns for MailAction {
    const NAMES: &'static [&'static str] = &["generation", "operation"];
    fn values(&self) -> Vec<Column<'_>> {
        vec![
            Column::Int(self.generation),
            Column::Text(Some(&self.operation)),
        ]
    }
}

pub const ACTIONS: Ledger<MailAction> = Ledger::new(
    "connector_actions",
    "account_id",
    "mail",
    Some(b"mail-action\0"),
);

/// The bot, turn and account checks every mail command passes, reads
/// included, in the caller's transaction: the turn is this bot's and still
/// running, the bot is active, the account is active at `generation`, and
/// the bot's grant allows the command.
pub async fn authorize_in(
    conn: &mut SqliteConnection,
    account_id: Uuid,
    generation: i64,
    bot_id: Uuid,
    turn_id: Uuid,
    command: &MailCommand,
) -> Result<(), StoreError> {
    let turn = crate::db::turns::caller(conn, turn_id)
        .await?
        .ok_or(StoreError::NotRunning)?;
    if turn.bot_id != bot_id {
        return Err(StoreError::Forbidden);
    }
    if !turn.running {
        return Err(StoreError::NotRunning);
    }
    // The grant comes first, so a bot without access learns nothing about
    // the account's state; a removed account has no grant.
    if !grant_in(conn, account_id, bot_id)
        .await?
        .is_some_and(|p| command.allowed(&p))
    {
        return Err(StoreError::Forbidden);
    }
    let account: Option<(AccountState, i64)> = sqlx::query_as(
        "SELECT state, generation FROM connector_accounts WHERE id = ?1 AND state <> 'removed'",
    )
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (state, current) = account.ok_or(StoreError::NotFound)?;
    if state != AccountState::Active {
        return Err(StoreError::Inactive);
    }
    if current != generation {
        return Err(StoreError::Changed);
    }
    Ok(())
}

/// [`authorize_in`] in a deferred read transaction, for commands that
/// change nothing and so are not recorded: one snapshot, no write lock.
pub async fn authorize(
    pool: &SqlitePool,
    account_id: Uuid,
    generation: i64,
    bot_id: Uuid,
    turn_id: Uuid,
    command: &MailCommand,
) -> Result<(), StoreError> {
    let mut tx = pool.begin().await?;
    authorize_in(&mut tx, account_id, generation, bot_id, turn_id, command).await
}

/// Authorize a command and record it as dispatching before it is sent, in
/// one immediate transaction.
pub async fn begin_action(
    pool: &SqlitePool,
    account_id: Uuid,
    generation: i64,
    bot_id: Uuid,
    turn_id: Uuid,
    action_id: Uuid,
    command: &MailCommand,
) -> Result<Start, StoreError> {
    let operation = serde_json::to_value(command)
        .ok()
        .and_then(|v| v["operation"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize_in(&mut tx, account_id, generation, bot_id, turn_id, command).await?;
    // A draft settled the send it claimed under this action without it, so
    // a copy still queued must never send.
    let released: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM draft_claims c JOIN outgoing_drafts d ON d.id = c.draft_id
             WHERE c.account_id = ?1 AND c.action_id = ?2
               AND NOT (d.status = 'sending' AND d.claim_action_id = c.action_id)
               AND NOT EXISTS (SELECT 1 FROM connector_actions a
                               WHERE a.account_id = ?1 AND a.action_id = ?2))",
    )
    .bind(account_id)
    .bind(action_id)
    .fetch_one(&mut *tx)
    .await?;
    if released {
        return Err(StoreError::Conflict);
    }
    let start = ACTIONS
        .begin(
            &mut tx,
            account_id,
            action_id,
            bot_id,
            turn_id,
            command,
            &MailAction {
                generation,
                operation,
            },
        )
        .await
        .map_err(|error| match error {
            BeginError::Conflict => StoreError::Conflict,
            BeginError::Db(error) => error.into(),
        })?;
    tx.commit().await?;
    Ok(start)
}

/// The account whose mail a connector turn was queued for; `None` for any
/// other turn.
pub async fn event_account(pool: &SqlitePool, turn_id: Uuid) -> Result<Option<Uuid>, StoreError> {
    Ok(
        sqlx::query_scalar(
            "SELECT account_id FROM connector_deliveries WHERE turn_id = ?1 LIMIT 1",
        )
        .bind(turn_id)
        .fetch_optional(pool)
        .await?,
    )
}

/// A send whose message was found in the account's Sent folder went out.
/// An uncertain action becomes succeeded. Returns whether the action now
/// stands as sent: also true when it already succeeded or is still
/// dispatching, false when it failed.
pub async fn found_in_sent(
    conn: &mut SqliteConnection,
    account_id: Uuid,
    action_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let status = ACTIONS
        .confirm(
            conn,
            account_id,
            action_id,
            serde_json::json!({"result": {"sent": true, "found_in_sent": true}}),
        )
        .await?;
    Ok(matches!(
        status,
        Some(Status::Succeeded | Status::Dispatching)
    ))
}

/// At startup, before any mail command runs: actions a previous process
/// left dispatching become uncertain. Returns how many.
pub async fn recover(pool: &SqlitePool) -> Result<u64, StoreError> {
    let mut conn = pool.acquire().await?;
    Ok(ACTIONS.recover(&mut conn).await?)
}
