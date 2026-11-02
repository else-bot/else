//! Saved provider execution profiles.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::app::providers::Profile;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProfileRecord {
    pub provider_id: String,
    pub model: String,
    pub reasoning_effort: String,
    pub service_tier: String,
    pub revision: i64,
    pub updated_at: DateTime<Utc>,
}

impl ProfileRecord {
    pub fn profile(&self) -> Profile {
        Profile {
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            service_tier: self.service_tier.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("no saved profile")]
    NotFound,
    #[error("the profile changed since revision {0}")]
    RevisionMismatch(i64),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

pub async fn get(pool: &PgPool, provider_id: &str) -> Result<Option<ProfileRecord>, sqlx::Error> {
    sqlx::query_as(
        "SELECT provider_id, model, reasoning_effort, service_tier, revision, updated_at
         FROM provider_profiles WHERE provider_id = $1",
    )
    .bind(provider_id)
    .fetch_optional(pool)
    .await
}

/// Replace the profile and bump its revision. Already accepted turns keep
/// their snapshots; only later acceptances see the new revision. An unchanged
/// profile keeps its revision.
pub async fn update(
    pool: &PgPool,
    provider_id: &str,
    profile: &Profile,
    expected_revision: Option<i64>,
) -> Result<ProfileRecord, ProfileError> {
    let mut tx = pool.begin().await?;
    let current: Option<ProfileRecord> = sqlx::query_as(
        "SELECT provider_id, model, reasoning_effort, service_tier, revision, updated_at
         FROM provider_profiles WHERE provider_id = $1 FOR UPDATE",
    )
    .bind(provider_id)
    .fetch_optional(&mut *tx)
    .await?;
    let current = current.ok_or(ProfileError::NotFound)?;
    if let Some(expected) = expected_revision.filter(|r| *r != current.revision) {
        return Err(ProfileError::RevisionMismatch(expected));
    }
    if current.profile() == *profile {
        return Ok(current);
    }
    let updated = sqlx::query_as(
        "UPDATE provider_profiles
         SET model = $2, reasoning_effort = $3, service_tier = $4,
             revision = revision + 1, updated_at = now()
         WHERE provider_id = $1
         RETURNING provider_id, model, reasoning_effort, service_tier, revision, updated_at",
    )
    .bind(provider_id)
    .bind(&profile.model)
    .bind(&profile.reasoning_effort)
    .bind(&profile.service_tier)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(updated)
}
