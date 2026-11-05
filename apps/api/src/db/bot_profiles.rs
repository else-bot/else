use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::app::providers::{Availability, Profile};
use crate::providers::Registry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotProfile {
    pub provider_id: String,
    pub profile: Profile,
    pub revision: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum BotProfileError {
    #[error("bot not found")]
    NotFound,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

pub async fn get(pool: &SqlitePool, bot_id: Uuid) -> Result<Option<BotProfile>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    own(&mut conn, bot_id).await
}

async fn own(conn: &mut SqliteConnection, bot_id: Uuid) -> Result<Option<BotProfile>, sqlx::Error> {
    let row: Option<(String, String, String, String, i64)> = sqlx::query_as(
        "SELECT provider_id, model, reasoning_effort, service_tier, revision
         FROM bot_profiles WHERE bot_id = ?1",
    )
    .bind(bot_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(
        |(provider_id, model, reasoning_effort, service_tier, revision)| BotProfile {
            provider_id,
            profile: Profile {
                model,
                reasoning_effort,
                service_tier,
            },
            revision,
        },
    ))
}

#[derive(Debug)]
pub struct TurnProvider {
    pub provider_id: &'static str,
    pub profile: Option<(i64, Json<Profile>)>,
}

/// Resolve a runnable provider and its profile snapshot for scheduled work.
/// A known but unavailable owner choice waits rather than switching providers.
pub async fn for_admission(
    conn: &mut SqliteConnection,
    registry: &Registry,
    bot_id: Uuid,
) -> Result<Option<TurnProvider>, sqlx::Error> {
    let chosen = match own(conn, bot_id).await? {
        Some(own) => Some(own.provider_id),
        None => default_provider(conn).await?,
    };
    let id = chosen
        .and_then(|id| registry.get(&id).map(|p| p.id()))
        .or(registry.default_id());
    let Some(provider) = id.and_then(|id| registry.get(id)) else {
        return Ok(None);
    };
    if provider.availability() != Availability::Available {
        return Ok(None);
    }
    let profile = if provider.uses_profile() {
        let Some(profile) = effective(conn, bot_id, provider.id()).await? else {
            return Ok(None);
        };
        Some(profile)
    } else {
        None
    };
    Ok(Some(TurnProvider {
        provider_id: provider.id(),
        profile,
    }))
}

/// Set an active bot's own profile (already validated by its provider's
/// adapter). An unchanged profile keeps its revision.
pub async fn put(
    pool: &SqlitePool,
    bot_id: Uuid,
    provider_id: &str,
    profile: &Profile,
) -> Result<BotProfile, BotProfileError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let record = put_in(&mut tx, bot_id, provider_id, profile).await?;
    tx.commit().await?;
    Ok(record)
}

pub(super) async fn put_in(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    provider_id: &str,
    profile: &Profile,
) -> Result<BotProfile, BotProfileError> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
        .bind(bot_id)
        .fetch_optional(&mut *conn)
        .await?;
    if status.as_deref() != Some("active") {
        return Err(BotProfileError::NotFound);
    }
    if let Some(current) = own(conn, bot_id).await?
        && current.provider_id == provider_id
        && current.profile == *profile
    {
        return Ok(current);
    }
    let revision: i64 = sqlx::query_scalar(
        "INSERT INTO bot_profiles (bot_id, provider_id, model, reasoning_effort, service_tier)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (bot_id) DO UPDATE SET
             provider_id = EXCLUDED.provider_id, model = EXCLUDED.model,
             reasoning_effort = EXCLUDED.reasoning_effort, service_tier = EXCLUDED.service_tier,
             revision = bot_profiles.revision + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         RETURNING revision",
    )
    .bind(bot_id)
    .bind(provider_id)
    .bind(&profile.model)
    .bind(&profile.reasoning_effort)
    .bind(&profile.service_tier)
    .fetch_one(&mut *conn)
    .await?;
    let record = BotProfile {
        provider_id: provider_id.to_owned(),
        profile: profile.clone(),
        revision,
    };
    Ok(record)
}

/// The installation's default provider, if the owner chose one.
pub async fn default_provider(conn: &mut SqliteConnection) -> Result<Option<String>, sqlx::Error> {
    Ok(
        sqlx::query_scalar("SELECT default_provider_id FROM installation")
            .fetch_optional(&mut *conn)
            .await?
            .flatten(),
    )
}

pub async fn set_default_provider(
    conn: &mut SqliteConnection,
    provider_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE installation SET default_provider_id = ?1")
        .bind(provider_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The provider a message to this bot uses when none is named: the bot's
/// own, else the installation's default (`None`: the caller's fallback).
pub async fn provider_for(pool: &SqlitePool, bot_id: Uuid) -> Result<Option<String>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    if let Some(own) = own(&mut conn, bot_id).await? {
        return Ok(Some(own.provider_id));
    }
    default_provider(&mut conn).await
}

/// The profile a new turn of this bot on `provider_id` snapshots: the
/// bot's own when it is for that provider, else the provider's saved one
/// (locked for share, as before). `None` if neither exists.
pub async fn effective(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    provider_id: &str,
) -> Result<Option<(i64, Json<Profile>)>, sqlx::Error> {
    if let Some(own) = own(conn, bot_id)
        .await?
        .filter(|p| p.provider_id == provider_id)
    {
        return Ok(Some((own.revision, Json(own.profile))));
    }
    let row: Option<(i64, String, String, String)> = sqlx::query_as(
        "SELECT revision, model, reasoning_effort, service_tier
         FROM provider_profiles WHERE provider_id = ?1",
    )
    .bind(provider_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(
        row.map(|(revision, model, reasoning_effort, service_tier)| {
            (
                revision,
                Json(Profile {
                    model,
                    reasoning_effort,
                    service_tier,
                }),
            )
        }),
    )
}
