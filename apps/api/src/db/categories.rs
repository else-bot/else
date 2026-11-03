//! Sidebar categories (POC-030): flat, owner-managed groups of bots.
//!
//! A category organizes the sidebar only. Assigning a bot changes nothing
//! else about it: not its identity, role, permissions, memory, provider
//! profile, or computer. Every change wakes the `bots` channel: assignment
//! and deletion journal a `bot.updated` event for each bot they touch, and
//! changes to a category's own row notify without a bot.

use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use super::events;

pub const MAX_NAME: usize = 80;

#[derive(Debug, Clone, Serialize, sqlx::FromRow, PartialEq, Eq)]
pub struct Category {
    pub id: Uuid,
    pub name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum CategoryError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("a category name is 1 to 80 characters after trimming, without control characters")]
    InvalidName,
    #[error("a category with that name already exists")]
    Duplicate,
    #[error("the request ID was used for a different request")]
    RequestConflict,
    #[error("the category made by this request was deleted since")]
    Gone,
    #[error("no such category")]
    NotFound,
    #[error("no such bot")]
    BotNotFound,
    #[error("the bot is deleted")]
    BotDeleted,
}

pub enum Created {
    New(Category),
    Existing(Category),
}

/// The trimmed name, or `InvalidName`.
pub fn normalize(name: &str) -> Result<String, CategoryError> {
    let name = name.trim();
    let chars = name.chars().count();
    if chars == 0 || chars > MAX_NAME || name.chars().any(char::is_control) {
        return Err(CategoryError::InvalidName);
    }
    Ok(name.to_owned())
}

fn duplicate(err: sqlx::Error) -> CategoryError {
    match &err {
        sqlx::Error::Database(db) if db.constraint() == Some("bot_categories_name") => {
            CategoryError::Duplicate
        }
        _ => CategoryError::Db(err),
    }
}

/// Wake `bots` subscribers once the transaction commits, for a change no
/// single bot carries.
async fn wake(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    events::notify(conn, Uuid::nil()).await
}

pub async fn list(pool: &PgPool) -> Result<Vec<Category>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, name, created_at, updated_at FROM bot_categories ORDER BY lower(name), id",
    )
    .fetch_all(pool)
    .await
}

/// Create a category once per request ID: a matching retry returns it, a
/// different payload under the same ID is a conflict.
pub async fn create(pool: &PgPool, request_id: Uuid, name: &str) -> Result<Created, CategoryError> {
    let name = normalize(name)?;
    let hash = Sha256::digest(format!("create_category\0{name}").as_bytes()).to_vec();
    let mut tx = pool.begin().await?;
    super::bots::lock_request(&mut tx, request_id).await?;
    let other: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM request_receipts WHERE request_id = $1)")
            .bind(request_id)
            .fetch_one(&mut *tx)
            .await?;
    if other {
        return Err(CategoryError::RequestConflict);
    }
    let receipt: Option<(Uuid, Vec<u8>)> = sqlx::query_as(
        "SELECT category_id, payload_hash FROM category_requests WHERE request_id = $1",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((id, saved)) = receipt {
        if saved != hash {
            return Err(CategoryError::RequestConflict);
        }
        let found: Option<Category> = sqlx::query_as(
            "SELECT id, name, created_at, updated_at FROM bot_categories WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        return found.map(Created::Existing).ok_or(CategoryError::Gone);
    }
    let id = Uuid::new_v4();
    let category: Category = sqlx::query_as(
        "INSERT INTO bot_categories (id, name) VALUES ($1, $2)
         RETURNING id, name, created_at, updated_at",
    )
    .bind(id)
    .bind(&name)
    .fetch_one(&mut *tx)
    .await
    .map_err(duplicate)?;
    sqlx::query(
        "INSERT INTO category_requests (request_id, category_id, payload_hash) VALUES ($1, $2, $3)",
    )
    .bind(request_id)
    .bind(id)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;
    wake(&mut tx).await?;
    tx.commit().await?;
    Ok(Created::New(category))
}

pub async fn rename(pool: &PgPool, id: Uuid, name: &str) -> Result<Category, CategoryError> {
    let name = normalize(name)?;
    let mut tx = pool.begin().await?;
    let category: Option<Category> = sqlx::query_as(
        "UPDATE bot_categories SET name = $2, updated_at = now() WHERE id = $1
         RETURNING id, name, created_at, updated_at",
    )
    .bind(id)
    .bind(&name)
    .fetch_optional(&mut *tx)
    .await
    .map_err(duplicate)?;
    let category = category.ok_or(CategoryError::NotFound)?;
    wake(&mut tx).await?;
    tx.commit().await?;
    Ok(category)
}

/// Delete a category in one transaction: its bots become unassigned (each
/// journals `bot.updated`); no bot, conversation, job, or other data is
/// touched. Returns the bots it unassigned.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<Vec<Uuid>, CategoryError> {
    let mut tx = pool.begin().await?;
    let found: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM bot_categories WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if found.is_none() {
        return Err(CategoryError::NotFound);
    }
    let bots: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE bots SET category_id = NULL WHERE category_id = $1 RETURNING id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    for bot in &bots {
        events::bot_updated(&mut tx, *bot).await?;
    }
    sqlx::query("DELETE FROM bot_categories WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    wake(&mut tx).await?;
    tx.commit().await?;
    Ok(bots)
}

/// Assign a bot to a category, or unassign it with `None`. The bot must be
/// active (a deleted bot is never revived by an assignment) and the
/// category must exist (it is locked against a concurrent delete).
pub async fn assign(
    pool: &PgPool,
    bot_id: Uuid,
    category: Option<Uuid>,
) -> Result<(), CategoryError> {
    let mut tx = pool.begin().await?;
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM bots WHERE id = $1 FOR UPDATE")
            .bind(bot_id)
            .fetch_optional(&mut *tx)
            .await?;
    match status.as_deref() {
        None => return Err(CategoryError::BotNotFound),
        Some("active") => {}
        Some(_) => return Err(CategoryError::BotDeleted),
    }
    if let Some(id) = category {
        let exists: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM bot_categories WHERE id = $1 FOR SHARE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if exists.is_none() {
            return Err(CategoryError::NotFound);
        }
    }
    let changed = sqlx::query(
        "UPDATE bots SET category_id = $2 WHERE id = $1 AND category_id IS DISTINCT FROM $2",
    )
    .bind(bot_id)
    .bind(category)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed > 0 {
        events::bot_updated(&mut tx, bot_id).await?;
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_are_trimmed_and_bounded_by_characters() {
        assert_eq!(super::normalize("  Family  ").unwrap(), "Family");
        assert!(super::normalize("   ").is_err());
        assert!(
            super::normalize(&"é".repeat(80)).is_ok(),
            "80 characters, 160 bytes"
        );
        assert!(super::normalize(&"é".repeat(81)).is_err());
        assert!(super::normalize("tab\there").is_err());
    }
}
