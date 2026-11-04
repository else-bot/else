//! Explicit installation setup and the startup check.
//!
//! `setup` creates the main bot and its conversation exactly once and seeds
//! missing provider profiles. Ordinary startup only reads this state: it never
//! seeds bots, recreates a deleted default, or overwrites saved settings.

use sqlx::{Sqlite, SqlitePool, Transaction};
use uuid::Uuid;

use super::DbError;
use crate::app::bots::{BotName, BotRole, MAIN_BOT_NAME, MAIN_BOT_ROLE};
use crate::app::providers::default_profiles;

#[derive(Debug, PartialEq, Eq)]
pub enum MainBot {
    Created(Uuid),
    /// Setup already completed; the saved default (possibly cleared) is kept.
    AlreadyInstalled(Option<Uuid>),
}

#[derive(Debug)]
pub struct SetupReport {
    pub main_bot: MainBot,
    pub seeded_profiles: Vec<&'static str>,
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("installation setup has not completed; run `else-api setup`")]
    NotInstalled,
    #[error(
        "the database contains bots but no installation record; it may be a partial \
         restore. Refusing to modify it; recover the installation record first"
    )]
    MissingInstallationRecord,
    #[error(
        "the saved default bot {0} is not active; refusing to start on an inconsistent installation"
    )]
    InactiveDefault(Uuid),
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<sqlx::Error> for InstallError {
    fn from(err: sqlx::Error) -> Self {
        Self::Db(err.into())
    }
}

/// Insert a bot and its canonical conversation in the caller's transaction.
/// Returns the conversation ID.
pub async fn insert_bot(
    tx: &mut Transaction<'_, Sqlite>,
    bot_id: Uuid,
    name: &BotName,
    role: &BotRole,
) -> Result<Uuid, sqlx::Error> {
    let conversation_id = Uuid::new_v4();
    sqlx::query("INSERT INTO bots (id, name, role) VALUES (?1, ?2, ?3)")
        .bind(bot_id)
        .bind(name.as_str())
        .bind(role.as_str())
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO conversations (id, bot_id) VALUES (?1, ?2)")
        .bind(conversation_id)
        .bind(bot_id)
        .execute(&mut **tx)
        .await?;
    Ok(conversation_id)
}

pub async fn setup(pool: &SqlitePool) -> Result<SetupReport, InstallError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    // Serialize concurrent setups so the main bot is created once.

    let mut seeded_profiles = Vec::new();
    for (provider_id, profile) in default_profiles() {
        let inserted = sqlx::query(
            "INSERT INTO provider_profiles (provider_id, model, reasoning_effort, service_tier)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (provider_id) DO NOTHING",
        )
        .bind(provider_id)
        .bind(&profile.model)
        .bind(&profile.reasoning_effort)
        .bind(&profile.service_tier)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if inserted == 1 {
            seeded_profiles.push(provider_id);
        }
    }

    let existing: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT default_bot_id FROM installation")
            .fetch_optional(&mut *tx)
            .await?;
    let main_bot = match existing {
        Some(default) => MainBot::AlreadyInstalled(default),
        None => {
            // Never treat saved bots as a fresh database.
            let bots: i64 = sqlx::query_scalar("SELECT count(*) FROM bots")
                .fetch_one(&mut *tx)
                .await?;
            if bots > 0 {
                return Err(InstallError::MissingInstallationRecord);
            }
            let bot_id = Uuid::new_v4();
            let name = BotName::parse(MAIN_BOT_NAME).expect("valid constant");
            let role = BotRole::parse(MAIN_BOT_ROLE).expect("valid constant");
            insert_bot(&mut tx, bot_id, &name, &role).await?;
            sqlx::query("INSERT INTO installation (default_bot_id) VALUES (?1)")
                .bind(bot_id)
                .execute(&mut *tx)
                .await?;
            MainBot::Created(bot_id)
        }
    };
    tx.commit().await?;
    Ok(SetupReport {
        main_bot,
        seeded_profiles,
    })
}

/// Startup check. Reads only; an empty installation is an error that points
/// at explicit setup rather than something startup repairs.
pub async fn check_installed(pool: &SqlitePool) -> Result<Option<Uuid>, InstallError> {
    let row: Option<(Option<Uuid>, Option<String>)> = sqlx::query_as(
        "SELECT i.default_bot_id, b.status
         FROM installation i LEFT JOIN bots b ON b.id = i.default_bot_id",
    )
    .fetch_optional(pool)
    .await?;
    match row {
        None => {
            let bots: i64 = sqlx::query_scalar("SELECT count(*) FROM bots")
                .fetch_one(pool)
                .await?;
            Err(if bots > 0 {
                InstallError::MissingInstallationRecord
            } else {
                InstallError::NotInstalled
            })
        }
        Some((Some(id), status)) if status.as_deref() != Some("active") => {
            Err(InstallError::InactiveDefault(id))
        }
        Some((default, _)) => Ok(default),
    }
}
