//! Per-bot grants on mail accounts and the triggers that wake bots.

use super::*;

/// Set one bot's permission override; all false persists a denial of access.
/// Withdrawing read or wake replaces the grant's identity and revokes its
/// undelivered events, so they never reach a later grant. A grant only
/// covers mail that arrives after it. Returns whether anything changed.
pub async fn set_permissions(
    pool: &SqlitePool,
    account_id: Uuid,
    bot_id: Uuid,
    p: &Permissions,
) -> Result<bool, StoreError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let changed = write_grant(&mut tx, account_id, bot_id, p).await?;
    tx.commit().await?;
    Ok(changed)
}

/// Turn new-mail wake-ups on or off for one bot, keeping its other access.
/// Waking needs read, so turning it on also grants read.
pub async fn set_wake(
    pool: &SqlitePool,
    account_id: Uuid,
    bot_id: Uuid,
    wake: bool,
) -> Result<bool, StoreError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let changed = wake_in(&mut tx, account_id, bot_id, wake).await?;
    tx.commit().await?;
    Ok(changed)
}

async fn wake_in(
    conn: &mut SqliteConnection,
    account_id: Uuid,
    bot_id: Uuid,
    wake: bool,
) -> Result<bool, StoreError> {
    let current = grant_in(conn, account_id, bot_id)
        .await?
        .ok_or(StoreError::NotFound)?;
    let p = Permissions {
        read: current.read || wake,
        wake,
        ..current
    };
    write_grant(conn, account_id, bot_id, &p).await
}

/// Wake a bot on an inbox it can already read. Never restores read the
/// owner withdrew. Returns whether anything changed.
pub async fn wake_if_readable_in(
    conn: &mut SqliteConnection,
    account_id: Uuid,
    bot_id: Uuid,
) -> Result<bool, StoreError> {
    match grant_in(conn, account_id, bot_id).await? {
        Some(current) if current.read => {
            let p = Permissions {
                wake: true,
                ..current
            };
            write_grant(conn, account_id, bot_id, &p).await
        }
        _ => Ok(false),
    }
}

/// What a trigger does when wake was turned on without instructions.
pub const DEFAULT_WAKE_INSTRUCTIONS: &str = "Handle new mail as the owner asked.";

/// Wake the calling bot on new mail in an inbox it can read, with what to
/// do then. Setting it again replaces the instructions and resumes it. Only
/// a turn the owner asked for may: mail it was woken by must not add one.
pub async fn set_trigger(
    pool: &SqlitePool,
    turn_id: Uuid,
    account_id: Uuid,
    instructions: &str,
) -> Result<MailTrigger, StoreError> {
    let instructions = wake_instructions(instructions)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let bot_id = owner_request_bot(&mut tx, turn_id).await?;
    let current = grant_in(&mut tx, account_id, bot_id)
        .await?
        .ok_or(StoreError::NotFound)?;
    if !current.read {
        return Err(StoreError::Forbidden);
    }
    let p = Permissions {
        wake: true,
        ..current
    };
    write_grant(&mut tx, account_id, bot_id, &p).await?;
    sqlx::query(
        "UPDATE connector_grants SET wake_instructions = ?3 WHERE account_id = ?1 AND bot_id = ?2",
    )
    .bind(account_id)
    .bind(bot_id)
    .bind(instructions)
    .execute(&mut *tx)
    .await?;
    crate::db::events::bot_updated(&mut tx, bot_id).await?;
    let trigger = mail_triggers(&mut tx, bot_id)
        .await?
        .into_iter()
        .find(|trigger| trigger.account_id == account_id)
        .ok_or(StoreError::NotFound)?;
    tx.commit().await?;
    Ok(trigger)
}

/// Delete the calling bot's trigger on an inbox, for an owner request.
pub async fn delete_trigger(
    pool: &SqlitePool,
    turn_id: Uuid,
    account_id: Uuid,
) -> Result<bool, StoreError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let bot_id = owner_request_bot(&mut tx, turn_id).await?;
    let deleted = clear_trigger(&mut tx, account_id, bot_id).await?;
    tx.commit().await?;
    Ok(deleted)
}

/// Delete a bot's trigger on an inbox from the owner's sidebar. The bot
/// keeps reading the inbox. Returns whether there was one.
pub async fn delete_trigger_by_owner(
    pool: &SqlitePool,
    account_id: Uuid,
    bot_id: Uuid,
) -> Result<bool, StoreError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let deleted = clear_trigger(&mut tx, account_id, bot_id).await?;
    tx.commit().await?;
    Ok(deleted)
}

async fn clear_trigger(
    conn: &mut SqliteConnection,
    account_id: Uuid,
    bot_id: Uuid,
) -> Result<bool, StoreError> {
    let current = grant_in(conn, account_id, bot_id)
        .await?
        .ok_or(StoreError::NotFound)?;
    let p = Permissions {
        wake: false,
        ..current
    };
    let woke = write_grant(conn, account_id, bot_id, &p).await?;
    let cleared = sqlx::query(
        "UPDATE connector_grants SET wake_instructions = NULL
         WHERE account_id = ?1 AND bot_id = ?2 AND wake_instructions IS NOT NULL",
    )
    .bind(account_id)
    .bind(bot_id)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        > 0;
    if cleared {
        crate::db::events::bot_updated(conn, bot_id).await?;
    }
    Ok(woke || cleared)
}

/// Trimmed trigger instructions, or why they can't be saved. They fit the
/// owner's approval card with the inbox address.
pub fn wake_instructions(instructions: &str) -> Result<&str, StoreError> {
    let instructions = instructions.trim();
    if instructions.is_empty() || instructions.len() > 3500 || instructions.contains('\0') {
        return Err(StoreError::Invalid(
            "Describe what to do on new mail in 1 to 3500 bytes.",
        ));
    }
    Ok(instructions)
}

/// The bot of a turn that may change its own mail triggers: one the owner
/// asked for in the bot's own chat, never a task another bot delegated.
pub async fn trigger_manager(pool: &SqlitePool, turn_id: Uuid) -> Result<Uuid, StoreError> {
    owner_request_bot(&mut *pool.acquire().await?, turn_id).await
}

async fn owner_request_bot(conn: &mut SqliteConnection, turn_id: Uuid) -> Result<Uuid, StoreError> {
    let caller = crate::db::turns::asking_caller(conn, turn_id)
        .await?
        .ok_or(StoreError::NotRunning)?;
    if !caller.owner_conversation
        || crate::db::delegations::origin(conn, turn_id).await?
            != crate::app::providers::Origin::OwnerRequest
    {
        return Err(StoreError::Forbidden);
    }
    Ok(caller.bot_id)
}

async fn write_grant(
    tx: &mut SqliteConnection,
    account_id: Uuid,
    bot_id: Uuid,
    p: &Permissions,
) -> Result<bool, StoreError> {
    if !p.read && (p.write || p.send || p.wake) {
        return Err(StoreError::Invalid("write, send and wake need read"));
    }
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM connector_accounts WHERE id = ?1 AND state <> 'removed')
            AND EXISTS (SELECT 1 FROM bots WHERE id = ?2 AND status = 'active')",
    )
    .bind(account_id)
    .bind(bot_id)
    .fetch_one(&mut *tx)
    .await?;
    if !live {
        return Err(StoreError::NotFound);
    }
    let old: Option<(Uuid, bool, bool, bool, bool)> = sqlx::query_as(
        "SELECT binding_id, can_read, can_write, can_send, can_wake FROM connector_grants
         WHERE account_id = ?1 AND bot_id = ?2",
    )
    .bind(account_id)
    .bind(bot_id)
    .fetch_optional(&mut *tx)
    .await?;
    let binding = match old {
        None if p.read && p.write && p.send && !p.wake => return Ok(false),
        Some((_, read, write, send, wake))
            if (read, write, send, wake) == (p.read, p.write, p.send, p.wake) =>
        {
            return Ok(false);
        }
        None => {
            let total: i64 = sqlx::query_scalar("SELECT count(*) FROM connector_grants")
                .fetch_one(&mut *tx)
                .await?;
            if total >= MAX_GRANTS {
                return Err(StoreError::Limit("at most 128 mail grants can exist"));
            }
            Uuid::new_v4()
        }
        Some((id, read, _, _, wake)) => {
            if (read && !p.read) || (wake && !p.wake) {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "UPDATE connector_deliveries SET status = ?3, settled_at = {NOW}
                     WHERE account_id = ?1 AND bot_id = ?2 AND status = 'pending'"
                )))
                .bind(account_id)
                .bind(bot_id)
                .bind(DeliveryStatus::Revoked)
                .execute(&mut *tx)
                .await?;
                Uuid::new_v4()
            } else {
                id
            }
        }
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO connector_grants (account_id, bot_id, binding_id, can_read, can_write, can_send,
             can_wake, wake_instructions)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, CASE WHEN ?7 THEN ?8 END)
         ON CONFLICT (account_id, bot_id) DO UPDATE SET binding_id = ?3, can_read = ?4,
             can_write = ?5, can_send = ?6, can_wake = ?7,
             wake_instructions = CASE WHEN ?7 THEN coalesce(wake_instructions, ?8)
                                      ELSE wake_instructions END,
             updated_at = {NOW}"
    )))
    .bind(account_id)
    .bind(bot_id)
    .bind(binding)
    .bind(p.read)
    .bind(p.write)
    .bind(p.send)
    .bind(p.wake)
    .bind(DEFAULT_WAKE_INSTRUCTIONS)
    .execute(&mut *tx)
    .await?;
    crate::db::events::bot_updated(tx, bot_id).await?;
    Ok(true)
}

/// One bot's mail triggers: each readable inbox with instructions, whether
/// its new mail wakes the bot or the trigger is paused.
pub async fn mail_triggers(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
) -> Result<Vec<MailTrigger>, sqlx::Error> {
    sqlx::query_as(
        "SELECT a.id AS account_id, a.address, a.state, a.health, g.can_wake AS wake,
                g.wake_instructions AS instructions,
                (SELECT max(d.settled_at) FROM connector_deliveries d
                 WHERE d.account_id = a.id AND d.bot_id = ?1 AND d.status = 'delivered')
                    AS last_woke_at
         FROM connector_accounts a
         JOIN connector_grants g ON g.account_id = a.id AND g.bot_id = ?1
         WHERE a.state <> 'removed' AND g.can_read AND g.wake_instructions IS NOT NULL
         ORDER BY a.created_at, a.id",
    )
    .bind(bot_id)
    .fetch_all(conn)
    .await
}

/// Republish every active bot's state after an inbox was added, removed,
/// paused, or resumed, since each bot lists the inboxes it can read.
pub(super) async fn inboxes_changed(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    let bots: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM bots WHERE status = 'active'")
        .fetch_all(&mut *conn)
        .await?;
    for bot in bots {
        crate::db::events::bot_updated(conn, bot).await?;
    }
    Ok(())
}

fn permissions_of((read, write, send, wake): (bool, bool, bool, bool)) -> Permissions {
    Permissions {
        read,
        write,
        send,
        wake,
    }
}

/// Effective access for an active bot, including workspace defaults.
pub async fn permissions(
    pool: &SqlitePool,
    account_id: Uuid,
    bot_id: Uuid,
) -> Result<Option<Permissions>, StoreError> {
    let mut conn = pool.acquire().await?;
    Ok(grant_in(&mut conn, account_id, bot_id).await?)
}

pub(super) async fn grant_in(
    conn: &mut SqliteConnection,
    account_id: Uuid,
    bot_id: Uuid,
) -> Result<Option<Permissions>, sqlx::Error> {
    let row: Option<(bool, bool, bool, bool)> = sqlx::query_as(
        "SELECT coalesce(g.can_read, 1), coalesce(g.can_write, 1),
                coalesce(g.can_send, 1), coalesce(g.can_wake, 0)
         FROM connector_accounts a CROSS JOIN bots b
         LEFT JOIN connector_grants g ON g.account_id = a.id AND g.bot_id = b.id
         WHERE a.id = ?1 AND b.id = ?2 AND a.state <> 'removed' AND b.status = 'active'",
    )
    .bind(account_id)
    .bind(bot_id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(permissions_of))
}

/// Every grant on one account, by bot.
pub async fn grants(
    pool: &SqlitePool,
    account_id: Uuid,
) -> Result<Vec<(Uuid, Permissions)>, StoreError> {
    let rows: Vec<(Uuid, bool, bool, bool, bool)> = sqlx::query_as(
        "SELECT bot_id, can_read, can_write, can_send, can_wake FROM connector_grants
         WHERE account_id = ?1 ORDER BY bot_id",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(bot, r, w, s, k)| (bot, permissions_of((r, w, s, k))))
        .collect())
}

/// The active bots that can read a connected account, by name, and whether
/// its new mail wakes each one.
pub async fn readers(
    pool: &SqlitePool,
    account_id: Uuid,
) -> Result<Vec<(String, bool)>, StoreError> {
    Ok(sqlx::query_as(
        "SELECT b.name, coalesce(g.can_wake, 0) FROM connector_accounts a CROSS JOIN bots b
         LEFT JOIN connector_grants g ON g.account_id = a.id AND g.bot_id = b.id
         WHERE a.id = ?1 AND a.state <> 'removed' AND b.status = 'active'
            AND coalesce(g.can_read, 1)
         ORDER BY b.name COLLATE NOCASE, b.id",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?)
}

/// Connected accounts and the active bot's effective access.
pub async fn bot_accounts(
    pool: &SqlitePool,
    bot_id: Uuid,
) -> Result<Vec<(Account, Permissions)>, StoreError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        #[sqlx(flatten)]
        account: AccountRow,
        can_read: bool,
        can_write: bool,
        can_send: bool,
        can_wake: bool,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT a.id, a.config, a.generation, a.state, a.health,
                coalesce(g.can_read, 1) AS can_read, coalesce(g.can_write, 1) AS can_write,
                coalesce(g.can_send, 1) AS can_send, coalesce(g.can_wake, 0) AS can_wake
         FROM connector_accounts a CROSS JOIN bots b
         LEFT JOIN connector_grants g ON g.account_id = a.id AND g.bot_id = b.id
         WHERE b.id = ?1 AND b.status = 'active' AND a.state <> 'removed'
         ORDER BY a.created_at, a.id",
    )
    .bind(bot_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.account.into(),
                permissions_of((r.can_read, r.can_write, r.can_send, r.can_wake)),
            )
        })
        .collect())
}
