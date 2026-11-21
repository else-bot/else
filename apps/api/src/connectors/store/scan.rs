//! The arrival cursor and the mail events a scan records.

use super::*;

fn cursor_of(row: (Option<i64>, Option<i64>)) -> Option<Cursor> {
    match row {
        (Some(uid_validity), Some(last_uid)) => Some(Cursor {
            uid_validity: u32::try_from(uid_validity).ok()?,
            last_uid: u32::try_from(last_uid).ok()?,
        }),
        _ => None,
    }
}

/// Where the next scan starts; `None` before the first scan.
pub async fn cursor(pool: &SqlitePool, id: Uuid) -> Result<Option<Cursor>, StoreError> {
    let row: Option<(Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT cursor_uid_validity, cursor_last_uid FROM connector_accounts WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(cursor_of))
}

/// Record a scan that started from `old`. Refused (false) unless the account
/// is active at `generation` and its cursor is still `old`, and when it
/// would move the cursor back. The first scan, and a scan after the folder's
/// UIDVALIDITY changed, only set the cursor: mail already there wakes no
/// one. Otherwise each new message is recorded once under its IMAP identity
/// and owed to every active bot whose grant has read and wake now, in the
/// same transaction as the cursor. At most [`MAX_ARRIVALS`] are recorded;
/// the cursor then stops at the last one so the next scan continues.
pub async fn record_scan(
    pool: &SqlitePool,
    id: Uuid,
    generation: i64,
    old: Option<&Cursor>,
    scan: &Scan,
) -> Result<bool, StoreError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(AccountState, i64, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT state, generation, cursor_uid_validity, cursor_last_uid FROM connector_accounts WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((state, current, validity, last)) = row else {
        return Ok(false);
    };
    if state != AccountState::Active
        || current != generation
        || cursor_of((validity, last)).as_ref() != old
    {
        return Ok(false);
    }
    let mut cursor = scan.cursor.clone();
    let mut arrivals = Vec::new();
    if let Some(old) = old.filter(|old| old.uid_validity == cursor.uid_validity) {
        if cursor.last_uid < old.last_uid {
            return Ok(false);
        }
        arrivals = scan
            .arrivals
            .iter()
            .filter(|a| {
                let m = &a.message;
                m.uid_validity == cursor.uid_validity
                    && m.uid > old.last_uid
                    && m.uid <= cursor.last_uid
                    && !m.folder.is_empty()
                    && m.folder.len() <= MAX_EVENT_FOLDER
            })
            .collect();
        arrivals.sort_by(|a, b| {
            (a.message.uid, &a.message.folder).cmp(&(b.message.uid, &b.message.folder))
        });
        arrivals.dedup_by(|a, b| a.message == b.message);
        if arrivals.len() > MAX_ARRIVALS {
            arrivals.truncate(MAX_ARRIVALS);
            cursor.last_uid = arrivals[MAX_ARRIVALS - 1].message.uid;
        }
    }
    for Arrival { message, envelope } in arrivals {
        let bounded = |text: &Option<String>| {
            text.as_deref()
                .map(|t| t.chars().take(256).collect::<String>())
        };
        let event: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO connector_events
                 (id, account_id, folder, uid_validity, uid, generation, message_id, subject, sender)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (account_id, folder, uid_validity, uid) DO NOTHING RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .bind(&message.folder)
        .bind(i64::from(message.uid_validity))
        .bind(i64::from(message.uid))
        .bind(generation)
        .bind(
            envelope
                .message_id
                .as_deref()
                .filter(|id| crate::connectors::config::message_id(id)),
        )
        .bind(bounded(&envelope.subject))
        .bind(bounded(&envelope.sender))
        .fetch_optional(&mut *tx)
        .await?;
        let Some(event) = event else { continue };
        sqlx::query(
            "INSERT INTO connector_deliveries (event_id, bot_id, account_id, binding_id, status)
             SELECT ?1, g.bot_id, g.account_id, g.binding_id, ?3 FROM connector_grants g
             JOIN bots b ON b.id = g.bot_id
             WHERE g.account_id = ?2 AND g.can_read AND g.can_wake
               AND b.status = 'active' AND NOT b.cancel_pending",
        )
        .bind(event)
        .bind(id)
        .bind(DeliveryStatus::Pending)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE connector_accounts SET cursor_uid_validity = ?2, cursor_last_uid = ?3, updated_at = {NOW}
         WHERE id = ?1"
    )))
    .bind(id)
    .bind(i64::from(cursor.uid_validity))
    .bind(i64::from(cursor.last_uid))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}
