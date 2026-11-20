//! Persistence for mail connectors: accounts, their encrypted credentials,
//! per-bot permission overrides, the arrival cursor, recorded mail events, and the
//! durable log of mail actions.
//!
//! A credential is age-encrypted with an identity derived from `SECRET_KEY`,
//! a per-write salt, and a purpose naming this account, credential version
//! and provider, so a ciphertext copied to another row or kept across a
//! provider change does not decrypt. It is never returned with an
//! [`Account`] and is erased when the account is removed.
//!
//! `generation` fences runtime work. Pausing, resuming, removing and
//! restoring increment it, and scans, credential refreshes and actions that
//! carry an older generation are refused.
//!
//! An action is recorded as `dispatching`, with a fingerprint of who asked
//! for what, before anything reaches the mail server. A retry with the same
//! action ID returns the recorded outcome; a different request under that ID
//! is a conflict. An action left dispatching by a crash becomes `uncertain`
//! and is never replayed.

use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::types::{
    Account, Arrival, Credential, Cursor, Envelope, Health, MailCommand, MailConfig, MailError,
    MailTrigger, MessageRef, MoveKind, Permissions, Place, Provider, Scan,
};
use crate::credentials::SecretKey;
use crate::db::turns::TurnStatus;

pub const PURPOSE: &str = "else.bot/mail-connectors/v1";
/// Accounts that are not removed.
pub const MAX_ACCOUNTS: i64 = 32;
/// Grants across all accounts.
pub const MAX_GRANTS: i64 = 128;
/// Events one scan records; the cursor stops at the last one recorded.
pub const MAX_ARRIVALS: usize = 100;
/// Bytes of a recorded action result.
pub const MAX_RESULT: usize = 256 * 1024;
const MAX_CONFIG: usize = 8192;
const MAX_FIELD: usize = 255;
const MAX_SECRET: usize = 8192;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no such mail account")]
    NotFound,
    #[error("the bot is not allowed to do this with this mail account")]
    Forbidden,
    #[error("the request is no longer running")]
    NotRunning,
    #[error("the mail account is paused")]
    Inactive,
    #[error("the mail account was changed; reload it and retry")]
    Changed,
    #[error("this action ID was already used for a different request")]
    Conflict,
    #[error("another mail account already uses this address")]
    AddressTaken,
    #[error("{0}")]
    Limit(&'static str),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("a mail credential could not be encrypted or decrypted")]
    Crypto,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl From<StoreError> for MailError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::NotFound => MailError::new("mail_account_not_found"),
            StoreError::Forbidden => MailError::new("mail_forbidden"),
            StoreError::NotRunning => MailError::new("mail_not_running"),
            StoreError::Inactive => MailError::new("mail_account_paused"),
            StoreError::Changed => MailError::new("mail_account_changed"),
            StoreError::Conflict => MailError::new("mail_action_conflict"),
            StoreError::AddressTaken => MailError::new("mail_account_exists"),
            StoreError::Limit(limit) => MailError::new("connector_limit").detail(limit),
            StoreError::Invalid(why) => MailError::new("invalid_mail_request").detail(why),
            StoreError::Crypto => MailError::new("mail_credential_unavailable"),
            StoreError::Db(error) => {
                // sqlx text can quote stored values, so it is logged, never
                // shown to the owner or the bot.
                tracing::warn!(%error, "mail store failed");
                MailError::new("mail_unavailable")
            }
        }
    }
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Gmail => "gmail",
        Provider::Microsoft => "microsoft",
        Provider::Yahoo => "yahoo",
        Provider::Icloud => "icloud",
        Provider::Imap => "imap",
    }
}

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')";

#[derive(Clone, Copy)]
struct Binding {
    account: Uuid,
    version: i64,
    provider: Provider,
}

fn purpose(b: Binding) -> String {
    format!(
        "{PURPOSE}\0{}\0{}\0{}",
        b.account,
        b.version,
        provider_name(b.provider)
    )
}

fn seal(
    key: &SecretKey,
    b: Binding,
    credential: &Credential,
) -> Result<crate::sealed::Sealed, StoreError> {
    let plain = Zeroizing::new(serde_json::to_vec(credential).map_err(|_| StoreError::Crypto)?);
    crate::sealed::seal(key, &purpose(b), &plain).map_err(|_| StoreError::Crypto)
}

fn open(key: &SecretKey, b: Binding, salt: &[u8], cipher: &[u8]) -> Result<Credential, StoreError> {
    let plain =
        crate::sealed::open(key, &purpose(b), salt, cipher).map_err(|_| StoreError::Crypto)?;
    serde_json::from_slice(&plain).map_err(|_| StoreError::Crypto)
}

fn field(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= MAX_FIELD && !value.contains('\0')
}

fn secret(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_SECRET
}

fn validate(config: &MailConfig, credential: &Credential) -> Result<(), StoreError> {
    let folders = [
        &config.archive_folder,
        &config.trash_folder,
        &config.drafts_folder,
        &config.sent_folder,
    ];
    let texts = [&config.username, &config.imap.host, &config.smtp.host];
    if !config.address.contains('@') || config.address.len() > 320 || !field(&config.address) {
        return Err(StoreError::Invalid("the mail address is not valid"));
    }
    if !texts.into_iter().chain(folders).all(|v| field(v)) {
        return Err(StoreError::Invalid(
            "the username, hosts and folder names need 1 to 255 bytes",
        ));
    }
    if config.imap.port == 0 || config.smtp.port == 0 {
        return Err(StoreError::Invalid("a mail server port cannot be 0"));
    }
    let credential_ok = match credential {
        Credential::Password { password } => secret(password),
        Credential::OAuth2 {
            client_id,
            client_secret,
            refresh_token,
            access_token,
            ..
        } => {
            field(client_id)
                && secret(refresh_token)
                && client_secret.as_deref().is_none_or(secret)
                && access_token.as_deref().is_none_or(secret)
        }
    };
    if !credential_ok {
        return Err(StoreError::Invalid(
            "the mail credential is empty or too long",
        ));
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct AccountRow {
    id: Uuid,
    config: Json<MailConfig>,
    generation: i64,
    state: String,
    health: String,
}

impl From<AccountRow> for Account {
    fn from(row: AccountRow) -> Self {
        Self {
            id: row.id,
            config: row.config.0,
            generation: row.generation,
            state: row.state,
            health: row.health.into(),
        }
    }
}

const ACCOUNT: &str = "SELECT id, config, generation, state, health FROM connector_accounts";

/// Encrypt and store a new account, active at generation 1 with no cursor.
pub async fn create(
    pool: &SqlitePool,
    key: &SecretKey,
    config: &MailConfig,
    credential: &Credential,
) -> Result<Account, StoreError> {
    validate(config, credential)?;
    let config_json =
        serde_json::to_string(config).map_err(|_| StoreError::Invalid("bad config"))?;
    if config_json.len() > MAX_CONFIG {
        return Err(StoreError::Invalid("the mail configuration is too large"));
    }
    let id = Uuid::new_v4();
    let sealed = seal(
        key,
        Binding {
            account: id,
            version: 1,
            provider: config.provider,
        },
        credential,
    )?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM connector_accounts WHERE state <> 'removed'")
            .fetch_one(&mut *tx)
            .await?;
    if count >= MAX_ACCOUNTS {
        return Err(StoreError::Limit(
            "at most 32 mail accounts can be connected",
        ));
    }
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM connector_accounts WHERE lower(address) = lower(?1) AND state <> 'removed')",
    )
    .bind(&config.address)
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return Err(StoreError::AddressTaken);
    }
    sqlx::query(
        "INSERT INTO connector_accounts (id, address, config, credential_salt, credential)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(id)
    .bind(&config.address)
    .bind(&config_json)
    .bind(&sealed.salt[..])
    .bind(&sealed.cipher)
    .execute(&mut *tx)
    .await?;
    inboxes_changed(&mut tx).await?;
    tx.commit().await?;
    Ok(Account {
        id,
        config: config.clone(),
        generation: 1,
        state: "active".into(),
        health: Health::Unknown,
    })
}

/// Connected accounts (active or paused), oldest first.
pub async fn accounts(pool: &SqlitePool) -> Result<Vec<Account>, StoreError> {
    let rows: Vec<AccountRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{ACCOUNT} WHERE state <> 'removed' ORDER BY created_at, id"
    )))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Account::from).collect())
}

/// A connected account; `None` once removed.
pub async fn account(pool: &SqlitePool, id: Uuid) -> Result<Option<Account>, StoreError> {
    let row: Option<AccountRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{ACCOUNT} WHERE id = ?1 AND state <> 'removed'"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Account::from))
}

pub async fn configure_folders(
    pool: &SqlitePool,
    id: Uuid,
    config: &MailConfig,
) -> Result<Account, StoreError> {
    config
        .validate()
        .map_err(|_| StoreError::Invalid("invalid mail configuration"))?;
    let current = account(pool, id).await?.ok_or(StoreError::NotFound)?;
    if config.provider != current.config.provider
        || config.address != current.config.address
        || config.username != current.config.username
        || config.imap != current.config.imap
        || config.smtp != current.config.smtp
    {
        return Err(StoreError::Invalid(
            "Changing account identity or server endpoints requires a new connection.",
        ));
    }
    if config == &current.config {
        return Ok(current);
    }
    if [
        &config.archive_folder,
        &config.trash_folder,
        &config.drafts_folder,
        &config.sent_folder,
    ]
    .iter()
    .any(|value| !field(value))
    {
        return Err(StoreError::Invalid("Folder names need 1 to 255 bytes."));
    }
    let changed = sqlx::query("UPDATE connector_accounts SET config=?3,generation=generation+1,health='unknown',health_version=health_version+1,updated_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE id=?1 AND generation=?2 AND state <> 'removed'")
        .bind(id).bind(current.generation).bind(Json(config)).execute(pool).await?.rows_affected();
    if changed != 1 {
        return Err(StoreError::Changed);
    }
    account(pool, id).await?.ok_or(StoreError::NotFound)
}

/// Decrypt an account's credential.
pub async fn credential(
    pool: &SqlitePool,
    key: &SecretKey,
    id: Uuid,
) -> Result<Credential, StoreError> {
    #[derive(sqlx::FromRow)]
    struct Stored {
        config: Json<MailConfig>,
        credential_version: i64,
        credential_salt: Vec<u8>,
        credential: Vec<u8>,
    }
    let row: Option<Stored> = sqlx::query_as(
        "SELECT config, credential_version, credential_salt, credential FROM connector_accounts
         WHERE id = ?1 AND state <> 'removed'",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let row = row.ok_or(StoreError::NotFound)?;
    let binding = Binding {
        account: id,
        version: row.credential_version,
        provider: row.config.0.provider,
    };
    open(key, binding, &row.credential_salt, &row.credential)
}

/// Replace the credential, for example after an OAuth refresh. Returns
/// false when the account was removed, its generation moved on, or another
/// write replaced the credential first.
pub async fn save_credential(
    pool: &SqlitePool,
    key: &SecretKey,
    id: Uuid,
    generation: i64,
    credential: &Credential,
) -> Result<bool, StoreError> {
    let row: Option<(Json<MailConfig>, i64)> = sqlx::query_as(
        "SELECT config, credential_version FROM connector_accounts
         WHERE id = ?1 AND generation = ?2 AND state <> 'removed'",
    )
    .bind(id)
    .bind(generation)
    .fetch_optional(pool)
    .await?;
    let Some((config, version)) = row else {
        return Ok(false);
    };
    validate(&config.0, credential)?;
    let next = version + 1;
    let sealed = seal(
        key,
        Binding {
            account: id,
            version: next,
            provider: config.0.provider,
        },
        credential,
    )?;
    let done = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE connector_accounts SET credential_version = ?4, credential_salt = ?5, credential = ?6,
             updated_at = {NOW}
         WHERE id = ?1 AND generation = ?2 AND credential_version = ?3 AND state <> 'removed'"
    )))
    .bind(id)
    .bind(generation)
    .bind(version)
    .bind(next)
    .bind(&sealed.salt[..])
    .bind(&sealed.cipher)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// The owner's replacement credential. Increments the generation, so
/// runtime work using the old credential is fenced, and resets health.
/// Returns false once the account is removed.
pub async fn replace_credential(
    pool: &SqlitePool,
    key: &SecretKey,
    id: Uuid,
    credential: &Credential,
) -> Result<bool, StoreError> {
    loop {
        let row: Option<(Json<MailConfig>, i64)> = sqlx::query_as(
            "SELECT config, credential_version FROM connector_accounts WHERE id = ?1 AND state <> 'removed'",
        )
        .bind(id)
        .fetch_optional(pool)
        .await?;
        let Some((config, version)) = row else {
            return Ok(false);
        };
        validate(&config.0, credential)?;
        let next = version + 1;
        let sealed = seal(
            key,
            Binding {
                account: id,
                version: next,
                provider: config.0.provider,
            },
            credential,
        )?;
        let done = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE connector_accounts SET credential_version = ?3, credential_salt = ?4, credential = ?5,
                 generation = generation + 1, health = 'unknown',
                 health_version = health_version + 1, updated_at = {NOW}
             WHERE id = ?1 AND credential_version = ?2 AND state <> 'removed'"
        )))
        .bind(id)
        .bind(version)
        .bind(next)
        .bind(&sealed.salt[..])
        .bind(&sealed.cipher)
        .execute(pool)
        .await?;
        if done.rows_affected() == 1 {
            return Ok(true);
        }
    }
}

/// Pause or resume an account. A change increments the generation; asking
/// for the current state changes nothing. `None` once removed.
pub async fn set_active(
    pool: &SqlitePool,
    id: Uuid,
    active: bool,
) -> Result<Option<Account>, StoreError> {
    let (to, from) = if active {
        ("active", "paused")
    } else {
        ("paused", "active")
    };
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let done = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE connector_accounts SET state = ?2, generation = generation + 1, updated_at = {NOW}
         WHERE id = ?1 AND state = ?3"
    )))
    .bind(id)
    .bind(to)
    .bind(from)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 1 {
        inboxes_changed(&mut tx).await?;
    }
    tx.commit().await?;
    account(pool, id).await
}

/// Counts an account's health writes. A watch reads it before it scans and
/// hands it to [`mark_connected`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthVersion(i64);

pub async fn health_version(pool: &SqlitePool, id: Uuid) -> Result<HealthVersion, StoreError> {
    sqlx::query_scalar(
        "SELECT health_version FROM connector_accounts WHERE id = ?1 AND state <> 'removed'",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .map(HealthVersion)
    .ok_or(StoreError::NotFound)
}

/// Record that a scan which began at `since` signed in. Refused when any
/// health was written after `since`, such as a rejection reported while the
/// scan ran, because that write is newer than this sign-in.
pub async fn mark_connected(
    pool: &SqlitePool,
    id: Uuid,
    generation: i64,
    since: HealthVersion,
) -> Result<bool, StoreError> {
    let done = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE connector_accounts
         SET health = 'connected', health_version = health_version + 1, updated_at = {NOW}
         WHERE id = ?1 AND generation = ?2 AND state <> 'removed' AND health_version = ?3"
    )))
    .bind(id)
    .bind(generation)
    .bind(since.0)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Record the runtime's view of an account's health for this generation.
/// A rejected credential or an expired OAuth grant stays recorded until the
/// owner replaces it or a sign-in succeeds, so a network error cannot hide
/// it. Only a sign-in that began afterwards reports `connected`, through
/// [`mark_connected`], so a repeated rejection still counts as a write.
/// Returns whether `health` is now recorded.
pub async fn set_health(
    pool: &SqlitePool,
    id: Uuid,
    generation: i64,
    health: &Health,
) -> Result<bool, StoreError> {
    let code = health.code();
    if !field(code) || code.len() > 64 {
        return Err(StoreError::Invalid("a health code needs 1 to 64 bytes"));
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let current: Option<String> = sqlx::query_scalar(
        "SELECT health FROM connector_accounts WHERE id = ?1 AND generation = ?2 AND state <> 'removed'",
    )
    .bind(id)
    .bind(generation)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(current) = current else {
        return Ok(false);
    };
    let applied = !Health::from(current).needs_new_credential() || *health == Health::Connected;
    if applied || health.needs_new_credential() {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE connector_accounts
             SET health = CASE WHEN ?4 THEN ?3 ELSE health END,
                 health_version = health_version + 1, updated_at = {NOW}
             WHERE id = ?1 AND generation = ?2"
        )))
        .bind(id)
        .bind(generation)
        .bind(code)
        .bind(applied)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(applied)
}

/// Remove an account: erase its credential, drop its grants and cancel
/// undelivered events. The row stays so its events keep their identity. A
/// later account for the same address starts with a quiet first scan.
pub async fn remove(pool: &SqlitePool, id: Uuid) -> Result<bool, StoreError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let done = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE connector_accounts SET state = 'removed', removed_at = {NOW}, updated_at = {NOW},
             generation = generation + 1, credential = NULL, credential_salt = NULL
         WHERE id = ?1 AND state <> 'removed'"
    )))
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query("DELETE FROM connector_grants WHERE account_id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE connector_deliveries SET status = 'cancelled', settled_at = {NOW}
         WHERE account_id = ?1 AND status = 'pending'"
    )))
    .bind(id)
    .execute(&mut *tx)
    .await?;
    inboxes_changed(&mut tx).await?;
    tx.commit().await?;
    Ok(true)
}

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
                    "UPDATE connector_deliveries SET status = 'revoked', settled_at = {NOW}
                     WHERE account_id = ?1 AND bot_id = ?2 AND status = 'pending'"
                )))
                .bind(account_id)
                .bind(bot_id)
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
async fn inboxes_changed(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
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

async fn grant_in(
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
    let row: Option<(String, i64, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT state, generation, cursor_uid_validity, cursor_last_uid FROM connector_accounts WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((state, current, validity, last)) = row else {
        return Ok(false);
    };
    if state != "active" || current != generation || cursor_of((validity, last)).as_ref() != old {
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
                    && field(&m.folder)
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
                .filter(|id| super::config::message_id(id)),
        )
        .bind(bounded(&envelope.subject))
        .bind(bounded(&envelope.sender))
        .fetch_optional(&mut *tx)
        .await?;
        let Some(event) = event else { continue };
        sqlx::query(
            "INSERT INTO connector_deliveries (event_id, bot_id, account_id, binding_id)
             SELECT ?1, g.bot_id, g.account_id, g.binding_id FROM connector_grants g
             JOIN bots b ON b.id = g.bot_id
             WHERE g.account_id = ?2 AND g.can_read AND g.can_wake
               AND b.status = 'active' AND NOT b.cancel_pending",
        )
        .bind(event)
        .bind(id)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum ActionStatus {
    Dispatching,
    Succeeded,
    Failed,
    /// It may or may not have reached the server; never replayed.
    Uncertain,
}

impl ActionStatus {
    fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionStart {
    /// Recorded as dispatching: send it, then [`finish_action`].
    Dispatch,
    /// This action was already requested: its recorded receipt. Never send
    /// it again.
    Existing(Value),
}

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
    let turn: Option<(Uuid, TurnStatus, String)> = sqlx::query_as(
        "SELECT t.bot_id, t.status, b.status FROM turns t JOIN bots b ON b.id = t.bot_id WHERE t.id = ?1",
    )
    .bind(turn_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((owner, status, bot_status)) = turn else {
        return Err(StoreError::NotRunning);
    };
    if owner != bot_id {
        return Err(StoreError::Forbidden);
    }
    if !status.serves_tools() || bot_status != "active" {
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
    let account: Option<(String, i64)> = sqlx::query_as(
        "SELECT state, generation FROM connector_accounts WHERE id = ?1 AND state <> 'removed'",
    )
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (state, current) = account.ok_or(StoreError::NotFound)?;
    if state != "active" {
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

fn fingerprint(account_id: Uuid, bot_id: Uuid, turn_id: Uuid, command: &MailCommand) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"mail-action\0");
    hasher.update(account_id.as_bytes());
    hasher.update(bot_id.as_bytes());
    hasher.update(turn_id.as_bytes());
    hasher.update(serde_json::to_vec(command).expect("a mail command serializes"));
    hasher.finalize().to_vec()
}

fn receipt(action_id: Uuid, status: &str, result: Option<Json<Value>>) -> Value {
    match result {
        Some(result) => result.0,
        None => serde_json::json!({ "action_id": action_id, "status": status }),
    }
}

/// Authorize a command and record it as dispatching before it is sent, in
/// one immediate transaction. A repeat of the same request returns its
/// receipt; a different request under the same action ID is a conflict.
pub async fn begin_action(
    pool: &SqlitePool,
    account_id: Uuid,
    generation: i64,
    bot_id: Uuid,
    turn_id: Uuid,
    action_id: Uuid,
    command: &MailCommand,
) -> Result<ActionStart, StoreError> {
    let print = fingerprint(account_id, bot_id, turn_id, command);
    let operation = serde_json::to_value(command)
        .ok()
        .and_then(|v| v["operation"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize_in(&mut tx, account_id, generation, bot_id, turn_id, command).await?;
    let existing: Option<(Vec<u8>, String, Option<Json<Value>>)> = sqlx::query_as(
        "SELECT fingerprint, status, result FROM connector_actions WHERE account_id = ?1 AND action_id = ?2",
    )
    .bind(account_id)
    .bind(action_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((recorded, status, result)) = existing {
        if recorded != print {
            return Err(StoreError::Conflict);
        }
        return Ok(ActionStart::Existing(receipt(action_id, &status, result)));
    }
    sqlx::query(
        "INSERT INTO connector_actions (account_id, action_id, bot_id, turn_id, generation, operation, fingerprint)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(account_id)
    .bind(action_id)
    .bind(bot_id)
    .bind(turn_id)
    .bind(generation)
    .bind(&operation)
    .bind(&print)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(ActionStart::Dispatch)
}

/// Settle a dispatching action with the receipt later retries get, and
/// return the receipt the ledger holds. That keeps an earlier settlement,
/// such as [`recover`] marking it uncertain.
pub async fn finish_action(
    pool: &SqlitePool,
    account_id: Uuid,
    action_id: Uuid,
    status: ActionStatus,
    settlement: &Value,
) -> Result<Value, StoreError> {
    if status == ActionStatus::Dispatching {
        return Err(StoreError::Invalid(
            "an action finishes as succeeded, failed or uncertain",
        ));
    }
    let mut text = settlement.to_string();
    if text.len() > MAX_RESULT {
        text = serde_json::json!({
            "action_id": action_id,
            "status": status.as_str(),
            "omitted": "the result was too large to record",
        })
        .to_string();
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let settled: Option<(String, Option<Json<Value>>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "UPDATE connector_actions SET status = ?3, result = ?4, finished_at = {NOW}
         WHERE account_id = ?1 AND action_id = ?2 AND status = 'dispatching'
         RETURNING status, result"
        )))
        .bind(account_id)
        .bind(action_id)
        .bind(status.as_str())
        .bind(text)
        .fetch_optional(&mut *tx)
        .await?;
    let stored = if let Some(stored) = settled {
        let turns: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT DISTINCT bot_id, turn_id FROM outgoing_drafts
             WHERE json_extract(delivery, '$.via') = 'mail'
               AND replace(json_extract(delivery, '$.account_id'), '-', '') = lower(hex(?1))
               AND replace(json_extract(delivery, '$.action_id'), '-', '') = lower(hex(?2))",
        )
        .bind(account_id)
        .bind(action_id)
        .fetch_all(&mut *tx)
        .await?;
        for (bot_id, turn_id) in turns {
            crate::db::events::turn_changed(&mut tx, bot_id, turn_id).await?;
        }
        stored
    } else {
        sqlx::query_as(
            "SELECT status, result FROM connector_actions WHERE account_id = ?1 AND action_id = ?2",
        )
        .bind(account_id)
        .bind(action_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::NotFound)?
    };
    tx.commit().await?;
    let (status, result) = stored;
    Ok(receipt(action_id, &status, result))
}

/// The receipt of an action this bot requested on this account.
pub async fn action_status(
    pool: &SqlitePool,
    account_id: Uuid,
    bot_id: Uuid,
    action_id: Uuid,
) -> Result<Option<Value>, StoreError> {
    let row: Option<(String, Option<Json<Value>>)> = sqlx::query_as(
        "SELECT status, result FROM connector_actions
         WHERE account_id = ?1 AND action_id = ?2 AND bot_id = ?3",
    )
    .bind(account_id)
    .bind(action_id)
    .bind(bot_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(status, result)| receipt(action_id, &status, result)))
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
    let receipt = serde_json::json!({
        "action_id": action_id,
        "status": "succeeded",
        "result": {"sent": true, "found_in_sent": true},
    });
    sqlx::query(
        "UPDATE connector_actions SET status = 'succeeded',
             result = json_set(?3, '$.earlier', json(coalesce(result, 'null')))
         WHERE account_id = ?1 AND action_id = ?2 AND status = 'uncertain'",
    )
    .bind(account_id)
    .bind(action_id)
    .bind(receipt.to_string())
    .execute(&mut *conn)
    .await?;
    let status: Option<String> = sqlx::query_scalar(
        "SELECT status FROM connector_actions WHERE account_id = ?1 AND action_id = ?2",
    )
    .bind(account_id)
    .bind(action_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(matches!(
        status.as_deref(),
        Some("succeeded" | "dispatching")
    ))
}

async fn uncertain_in(conn: &mut SqliteConnection) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE connector_actions SET status = 'uncertain', finished_at = {NOW} WHERE status = 'dispatching'"
    )))
    .execute(conn)
    .await?
    .rows_affected())
}

/// At startup, before any mail command runs: actions a previous process
/// left dispatching become uncertain. Returns how many.
pub async fn recover(pool: &SqlitePool) -> Result<u64, StoreError> {
    let mut conn = pool.acquire().await?;
    Ok(uncertain_in(&mut conn).await?)
}

/// On a restored database: pause every connected account, fence its
/// generation and forget its cursor, so nothing runs until the owner
/// resumes it and the first scan after that is quiet. Undelivered events
/// are cancelled and dispatching actions become uncertain. One transaction
/// (a savepoint inside the caller's).
pub async fn restore_passive(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    use sqlx::Connection;
    let mut tx = conn.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE connector_accounts SET state = 'paused', generation = generation + 1,
             cursor_uid_validity = NULL, cursor_last_uid = NULL, updated_at = {NOW}
         WHERE state <> 'removed'"
    )))
    .execute(&mut *tx)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE connector_deliveries SET status = 'cancelled', settled_at = {NOW} WHERE status = 'pending'"
    )))
    .execute(&mut *tx)
    .await?;
    uncertain_in(&mut tx).await?;
    tx.commit().await
}

/// A mail trigger's message: its event, the Message-ID the scan saw, and
/// where it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Whereabouts {
    pub event_id: Uuid,
    pub message_id: Option<String>,
    pub place: Place,
}

type PlaceRow = (String, Option<String>, Option<i64>, Option<i64>);

fn place_of((location, folder, uid_validity, uid): PlaceRow) -> Result<Place, StoreError> {
    let kind = match location.as_str() {
        "arrived" => return Ok(Place::Arrived),
        "deleted" => return Ok(Place::Deleted),
        "archived" => MoveKind::Archived,
        "trashed" => MoveKind::Trashed,
        "moved" => MoveKind::Moved,
        _ => return Err(StoreError::Invalid("unknown mail location")),
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
            place.as_str().to_owned(),
            Some(folder.clone()),
            message.as_ref().map(|m| i64::from(m.uid_validity)),
            message.as_ref().map(|m| i64::from(m.uid)),
        ),
        Place::Arrived | Place::Deleted => (place.as_str().to_owned(), None, None, None),
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
        location: String,
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
            .bind(seen.as_str())
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
    .bind(guard.as_ref().map(|g| g.0.clone()))
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
