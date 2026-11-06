use std::sync::Arc;

use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::app::messages::Attachment;
use crate::app::providers::InputFile;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("bot not found")]
    NotFound,
    #[error("bot deleted")]
    Deleted,
    #[error("attachment ID reused with different content")]
    Conflict,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

type Row = (Uuid, String, String, i64);

fn attachment((id, name, media_type, size): Row) -> Attachment {
    Attachment {
        id,
        name,
        media_type,
        size: size as u64,
    }
}

/// Store an upload under its client-chosen ID. Repeating the same upload
/// returns the saved file; the ID with other content, or another bot, is a
/// conflict. True when this call stored it.
pub async fn store(
    pool: &SqlitePool,
    bot_id: Uuid,
    id: Uuid,
    name: &str,
    media_type: &str,
    data: &[u8],
) -> Result<(Attachment, bool), StoreError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let result = store_in(&mut tx, bot_id, id, name, media_type, data).await?;
    tx.commit().await?;
    Ok(result)
}

pub(super) async fn store_in(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    id: Uuid,
    name: &str,
    media_type: &str,
    data: &[u8],
) -> Result<(Attachment, bool), StoreError> {
    let digest = Sha256::digest(data).to_vec();
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
        .bind(bot_id)
        .fetch_optional(&mut *conn)
        .await?;
    match status.as_deref() {
        None => return Err(StoreError::NotFound),
        Some("active") => {}
        Some(_) => return Err(StoreError::Deleted),
    }
    let saved: Option<(Uuid, String, String, Vec<u8>)> =
        sqlx::query_as("SELECT bot_id, name, media_type, sha256 FROM attachments WHERE id = ?1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    let created = match saved {
        Some((owner, saved_name, saved_type, saved_digest))
            if owner == bot_id
                && saved_name == name
                && saved_type == media_type
                && saved_digest == digest =>
        {
            false
        }
        Some(_) => return Err(StoreError::Conflict),
        None => {
            sqlx::query(
                "INSERT INTO attachments (id, bot_id, name, media_type, size, sha256, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .bind(id)
            .bind(bot_id)
            .bind(name)
            .bind(media_type)
            .bind(data.len() as i64)
            .bind(&digest)
            .bind(data)
            .execute(&mut *conn)
            .await?;
            true
        }
    };
    let file = Attachment {
        id,
        name: name.to_owned(),
        media_type: media_type.to_owned(),
        size: data.len() as u64,
    };
    Ok((file, created))
}

/// A bot's file with its bytes.
pub async fn read(
    pool: &SqlitePool,
    bot_id: Uuid,
    id: Uuid,
) -> Result<Option<(Attachment, Vec<u8>)>, sqlx::Error> {
    let row: Option<(Uuid, String, String, i64, Vec<u8>)> = sqlx::query_as(
        "SELECT id, name, media_type, size, data FROM attachments WHERE id = ?1 AND bot_id = ?2",
    )
    .bind(id)
    .bind(bot_id)
    .fetch_optional(pool)
    .await?;
    Ok(row
        .map(|(id, name, media_type, size, data)| (attachment((id, name, media_type, size)), data)))
}

/// Whether every file is this bot's, exactly as stored.
pub async fn all_match(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    files: &[&Attachment],
) -> Result<bool, sqlx::Error> {
    for file in files {
        let row: Option<Row> = sqlx::query_as(
            "SELECT id, name, media_type, size FROM attachments WHERE id = ?1 AND bot_id = ?2",
        )
        .bind(file.id)
        .bind(bot_id)
        .fetch_optional(&mut *conn)
        .await?;
        if row.map(attachment).as_ref() != Some(*file) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The stored files with these IDs, in the given order; missing ones are
/// left out.
pub async fn load(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    ids: &[Uuid],
) -> Result<Vec<InputFile>, sqlx::Error> {
    let mut files = Vec::with_capacity(ids.len());
    for id in ids {
        let row: Option<(Uuid, String, String, i64, Vec<u8>)> = sqlx::query_as(
            "SELECT id, name, media_type, size, data FROM attachments
             WHERE id = ?1 AND bot_id = ?2",
        )
        .bind(id)
        .bind(bot_id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some((id, name, media_type, size, data)) = row {
            files.push(InputFile {
                attachment: attachment((id, name, media_type, size)),
                data: Arc::from(data),
            });
        }
    }
    Ok(files)
}
