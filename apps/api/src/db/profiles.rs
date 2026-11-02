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
    #[error("{0}")]
    Invalid(String),
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

/// Fields to change; `None` keeps the saved value.
#[derive(Debug, Default)]
pub struct ProfilePatch {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
}

/// Merge `patch` into the saved profile under its row lock, validate the
/// result, and bump the revision. Concurrent partial updates therefore never
/// overwrite each other's fields. Already accepted turns keep their
/// snapshots; only later acceptances see the new revision. An unchanged
/// profile keeps its revision.
pub async fn update(
    pool: &PgPool,
    provider_id: &str,
    patch: ProfilePatch,
    expected_revision: Option<i64>,
    validate: impl FnOnce(&Profile) -> Result<(), String>,
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
    let mut profile = current.profile();
    if let Some(model) = patch.model {
        profile.model = model;
    }
    if let Some(effort) = patch.reasoning_effort {
        profile.reasoning_effort = effort;
    }
    if let Some(tier) = patch.service_tier {
        profile.service_tier = tier;
    }
    validate(&profile).map_err(ProfileError::Invalid)?;
    if current.profile() == profile {
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
