//! Where a trigger's message is now, after bots and the server moved it.

use super::*;

/// A mail trigger's message: its event, the Message-ID the scan saw, and
/// where it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Whereabouts {
    pub event_id: Uuid,
    pub message_id: Option<String>,
    pub place: Place,
}

#[derive(Clone, Copy, sqlx::Type, strum::IntoStaticStr)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
enum Location {
    Arrived,
    Archived,
    Trashed,
    Moved,
    Deleted,
}

impl From<&Place> for Location {
    fn from(place: &Place) -> Self {
        match place {
            Place::Arrived => Self::Arrived,
            Place::Moved { kind, .. } => match kind {
                MoveKind::Archived => Self::Archived,
                MoveKind::Trashed => Self::Trashed,
                MoveKind::Moved => Self::Moved,
            },
            Place::Deleted => Self::Deleted,
        }
    }
}

type PlaceRow = (Location, Option<String>, Option<i64>, Option<i64>);

fn place_of((location, folder, uid_validity, uid): PlaceRow) -> Result<Place, StoreError> {
    let kind = match location {
        Location::Arrived => return Ok(Place::Arrived),
        Location::Deleted => return Ok(Place::Deleted),
        Location::Archived => MoveKind::Archived,
        Location::Trashed => MoveKind::Trashed,
        Location::Moved => MoveKind::Moved,
    };
    let folder = folder.ok_or(StoreError::Invalid("a moved message has a folder"))?;
    let message = match (uid_validity, uid) {
        (Some(uid_validity), Some(uid)) => Some(MessageRef {
            folder: folder.clone(),
            uid_validity: u32::try_from(uid_validity)
                .map_err(|_| StoreError::Invalid("uid_validity out of range"))?,
            uid: u32::try_from(uid).map_err(|_| StoreError::Invalid("uid out of range"))?,
        }),
        _ => None,
    };
    Ok(Place::Moved {
        kind,
        folder,
        message,
    })
}

/// The stored columns of `place`: location, folder, UIDVALIDITY and UID.
fn place_row(place: &Place) -> PlaceRow {
    match place {
        Place::Moved {
            folder, message, ..
        } => (
            place.into(),
            Some(folder.clone()),
            message.as_ref().map(|m| i64::from(m.uid_validity)),
            message.as_ref().map(|m| i64::from(m.uid)),
        ),
        Place::Arrived | Place::Deleted => (place.into(), None, None, None),
    }
}

/// Where the mail trigger that arrived as `message` in this account is
/// now; `None` when no trigger arrived there.
pub async fn whereabouts(
    pool: &SqlitePool,
    account_id: Uuid,
    message: &MessageRef,
) -> Result<Option<Whereabouts>, StoreError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        message_id: Option<String>,
        location: Location,
        moved_folder: Option<String>,
        moved_uid_validity: Option<i64>,
        moved_uid: Option<i64>,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT id, message_id, location, moved_folder, moved_uid_validity, moved_uid
         FROM connector_events
         WHERE account_id = ?1 AND folder = ?2 AND uid_validity = ?3 AND uid = ?4",
    )
    .bind(account_id)
    .bind(&message.folder)
    .bind(i64::from(message.uid_validity))
    .bind(i64::from(message.uid))
    .fetch_optional(pool)
    .await?;
    row.map(|row| {
        Ok(Whereabouts {
            event_id: row.id,
            message_id: row.message_id,
            place: place_of((
                row.location,
                row.moved_folder,
                row.moved_uid_validity,
                row.moved_uid,
            ))?,
        })
    })
    .transpose()
}

/// Records that a bot's `turn` moved or deleted the message at `from`,
/// on the mail trigger whose message sits there now. False when no
/// trigger's message sits there. A deleted message moves no further.
pub async fn relocate(
    pool: &SqlitePool,
    account_id: Uuid,
    turn_id: Uuid,
    from: &MessageRef,
    to: &Place,
) -> Result<bool, StoreError> {
    if *to == Place::Arrived {
        return Err(StoreError::Invalid("a message never moves back to arrived"));
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    // A message that left and came back to the Inbox arrived twice, and
    // both triggers follow it.
    let events: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM connector_events WHERE account_id = ?1 AND (
             (location = 'arrived' AND folder = ?2 AND uid_validity = ?3 AND uid = ?4)
          OR (location IN ('archived', 'trashed', 'moved')
              AND moved_folder = ?2 AND moved_uid_validity = ?3 AND moved_uid = ?4))",
    )
    .bind(account_id)
    .bind(&from.folder)
    .bind(i64::from(from.uid_validity))
    .bind(i64::from(from.uid))
    .fetch_all(&mut *tx)
    .await?;
    let mut moved = false;
    for event in events {
        moved |= place_in(&mut tx, event, None, to, Some(turn_id)).await?;
    }
    tx.commit().await?;
    Ok(moved)
}

/// Records where the owner's reader found the mail trigger `event`, or
/// that it found it nowhere (`Place::Deleted`), unless it moved since the
/// reader saw it at `seen`. False when it moved since.
pub async fn found(
    pool: &SqlitePool,
    event_id: Uuid,
    seen: &Place,
    now: &Place,
) -> Result<bool, StoreError> {
    if *seen == Place::Deleted || *now == Place::Arrived {
        return Err(StoreError::Invalid(
            "a deleted message stays deleted and a found one has moved",
        ));
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let moved = match (seen, now) {
        // Only the UID was unknown: who moved it, and when, still hold.
        (
            Place::Moved {
                folder,
                message: None,
                ..
            },
            Place::Moved {
                folder: found,
                message: Some(message),
                ..
            },
        ) if folder == found => {
            sqlx::query(
                "UPDATE connector_events SET moved_uid_validity = ?3, moved_uid = ?4
                 WHERE id = ?1 AND location = ?2 AND moved_folder = ?5 AND moved_uid IS NULL",
            )
            .bind(event_id)
            .bind(Location::from(seen))
            .bind(i64::from(message.uid_validity))
            .bind(i64::from(message.uid))
            .bind(folder)
            .execute(&mut *tx)
            .await?
            .rows_affected()
                == 1
        }
        _ => place_in(&mut tx, event_id, Some(seen), now, None).await?,
    };
    tx.commit().await?;
    Ok(moved)
}

/// Keeps what an opened message says of itself on a mail trigger that
/// arrived before scans kept it, so a later move can be followed.
pub async fn remember(
    pool: &SqlitePool,
    event_id: Uuid,
    envelope: &Envelope,
) -> Result<bool, StoreError> {
    let Some(message_id) = envelope.message_id.as_deref() else {
        return Ok(false);
    };
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let done = sqlx::query(
        "UPDATE connector_events SET message_id = ?2, subject = coalesce(subject, ?3),
             sender = coalesce(sender, ?4)
         WHERE id = ?1 AND message_id IS NULL",
    )
    .bind(event_id)
    .bind(message_id)
    .bind(envelope.subject.as_deref())
    .bind(envelope.sender.as_deref())
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 1 {
        turns_changed(&mut tx, event_id).await?;
    }
    tx.commit().await?;
    Ok(done.rows_affected() == 1)
}

/// Moves `event` to `to`, when it is at `from` (any place but deleted when
/// `None`), and shows its turns the change.
async fn place_in(
    tx: &mut SqliteConnection,
    event: Uuid,
    from: Option<&Place>,
    to: &Place,
    turn_id: Option<Uuid>,
) -> Result<bool, StoreError> {
    let (location, folder, uid_validity, uid) = place_row(to);
    let guard = from.map(place_row);
    let done = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE connector_events SET location = ?2, moved_folder = ?3, moved_uid_validity = ?4,
             moved_uid = ?5, located_turn_id = ?6, located_at = {NOW}
         WHERE id = ?1 AND location <> 'deleted'
           AND (?7 IS NULL OR (location = ?7 AND moved_folder IS ?8
                AND moved_uid_validity IS ?9 AND moved_uid IS ?10))"
    )))
    .bind(event)
    .bind(location)
    .bind(folder)
    .bind(uid_validity)
    .bind(uid)
    .bind(turn_id)
    .bind(guard.as_ref().map(|g| g.0))
    .bind(guard.as_ref().and_then(|g| g.1.clone()))
    .bind(guard.as_ref().and_then(|g| g.2))
    .bind(guard.as_ref().and_then(|g| g.3))
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(false);
    }
    turns_changed(tx, event).await?;
    Ok(true)
}

/// Shows each turn the mail trigger `event` started its new row.
async fn turns_changed(tx: &mut SqliteConnection, event: Uuid) -> Result<(), StoreError> {
    let turns: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT bot_id, turn_id FROM connector_deliveries WHERE event_id = ?1 AND turn_id IS NOT NULL",
    )
    .bind(event)
    .fetch_all(&mut *tx)
    .await?;
    for (bot_id, turn_id) in turns {
        crate::db::events::turn_changed(tx, bot_id, turn_id).await?;
    }
    Ok(())
}
