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

use std::io::{Read, Write};

use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::types::{
    Account, Credential, Cursor, MailCommand, MailConfig, MailTrigger, Permissions, Provider, Scan,
};
use crate::credentials::SecretKey;

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
    #[error("the mail account is paused or was changed; reload it and retry")]
    Inactive,
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

fn identity(
    key: &SecretKey,
    salt: &[u8; 32],
    b: Binding,
) -> Result<age::x25519::Identity, StoreError> {
    let purpose = format!(
        "{PURPOSE}\0{}\0{}\0{}",
        b.account,
        b.version,
        provider_name(b.provider)
    );
    crate::backup::identity_for_purpose(key, salt, &purpose).map_err(|_| StoreError::Crypto)
}

struct Sealed {
    salt: [u8; 32],
    cipher: Vec<u8>,
}

fn seal(key: &SecretKey, b: Binding, credential: &Credential) -> Result<Sealed, StoreError> {
    let mut salt = [0u8; 32];
    getrandom::fill(&mut salt).map_err(|_| StoreError::Crypto)?;
    let recipient = identity(key, &salt, b)?.to_public();
    let plain = Zeroizing::new(serde_json::to_vec(credential).map_err(|_| StoreError::Crypto)?);
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .map_err(|_| StoreError::Crypto)?;
    let mut cipher = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut cipher)
        .map_err(|_| StoreError::Crypto)?;
    writer
        .write_all(&plain)
        .and_then(|()| writer.finish().map(drop))
        .map_err(|_| StoreError::Crypto)?;
    Ok(Sealed { salt, cipher })
}

fn open(key: &SecretKey, b: Binding, salt: &[u8], cipher: &[u8]) -> Result<Credential, StoreError> {
    let salt: [u8; 32] = salt.try_into().map_err(|_| StoreError::Crypto)?;
    let identity = identity(key, &salt, b)?;
    let decryptor = age::Decryptor::new(cipher).map_err(|_| StoreError::Crypto)?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|_| StoreError::Crypto)?;
    let mut plain = Zeroizing::new(Vec::new());
    reader
        .read_to_end(&mut plain)
        .map_err(|_| StoreError::Crypto)?;
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
            health: row.health,
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
        health: "unknown".into(),
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
    let changed = sqlx::query("UPDATE connector_accounts SET config=?3,generation=generation+1,health='unknown',updated_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE id=?1 AND generation=?2 AND state <> 'removed'")
        .bind(id).bind(current.generation).bind(Json(config)).execute(pool).await?.rows_affected();
    if changed != 1 {
        return Err(StoreError::Inactive);
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
                 generation = generation + 1, health = 'unknown', updated_at = {NOW}
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

/// Record the runtime's view of an account's health for this generation.
pub async fn set_health(
    pool: &SqlitePool,
    id: Uuid,
    generation: i64,
    health: &str,
) -> Result<bool, StoreError> {
    if !field(health) || health.len() > 64 {
        return Err(StoreError::Invalid("a health code needs 1 to 64 bytes"));
    }
    let done = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE connector_accounts SET health = ?3, updated_at = {NOW}
         WHERE id = ?1 AND generation = ?2 AND state <> 'removed'"
    )))
    .bind(id)
    .bind(generation)
    .bind(health)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
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

/// [`set_wake`] inside the caller's immediate transaction.
pub async fn wake_in(
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
        "INSERT INTO connector_grants (account_id, bot_id, binding_id, can_read, can_write, can_send, can_wake)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (account_id, bot_id) DO UPDATE SET binding_id = ?3, can_read = ?4,
             can_write = ?5, can_send = ?6, can_wake = ?7, updated_at = {NOW}"
    )))
    .bind(account_id)
    .bind(bot_id)
    .bind(binding)
    .bind(p.read)
    .bind(p.write)
    .bind(p.send)
    .bind(p.wake)
    .execute(&mut *tx)
    .await?;
    crate::db::events::bot_updated(tx, bot_id).await?;
    Ok(true)
}

/// Every inbox one bot can read, and whether its new mail wakes the bot.
pub async fn mail_triggers(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
) -> Result<Vec<MailTrigger>, sqlx::Error> {
    sqlx::query_as(
        "SELECT a.id AS account_id, a.address, a.state, a.health,
                coalesce(g.can_wake, 0) AS wake
         FROM connector_accounts a
         LEFT JOIN connector_grants g ON g.account_id = a.id AND g.bot_id = ?1
         WHERE a.state <> 'removed' AND coalesce(g.can_read, 1)
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
            .map(|a| &a.message)
            .filter(|m| {
                m.uid_validity == cursor.uid_validity
                    && m.uid > old.last_uid
                    && m.uid <= cursor.last_uid
                    && field(&m.folder)
            })
            .collect();
        arrivals.sort_by(|a, b| (a.uid, &a.folder).cmp(&(b.uid, &b.folder)));
        arrivals.dedup();
        if arrivals.len() > MAX_ARRIVALS {
            arrivals.truncate(MAX_ARRIVALS);
            cursor.last_uid = arrivals[MAX_ARRIVALS - 1].uid;
        }
    }
    for message in arrivals {
        let event: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO connector_events (id, account_id, folder, uid_validity, uid, generation)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (account_id, folder, uid_validity, uid) DO NOTHING RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .bind(&message.folder)
        .bind(i64::from(message.uid_validity))
        .bind(i64::from(message.uid))
        .bind(generation)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionStatus {
    Dispatching,
    Succeeded,
    Failed,
    /// It may or may not have reached the server; never replayed.
    Uncertain,
}

impl ActionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Dispatching => "dispatching",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
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
    let turn: Option<(Uuid, String, String)> = sqlx::query_as(
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
    if !matches!(status.as_str(), "sending" | "running") || bot_status != "active" {
        return Err(StoreError::NotRunning);
    }
    let account: Option<(String, i64)> = sqlx::query_as(
        "SELECT state, generation FROM connector_accounts WHERE id = ?1 AND state <> 'removed'",
    )
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (state, current) = account.ok_or(StoreError::NotFound)?;
    if state != "active" || current != generation {
        return Err(StoreError::Inactive);
    }
    match grant_in(conn, account_id, bot_id).await? {
        Some(p) if command.allowed(&p) => Ok(()),
        _ => Err(StoreError::Forbidden),
    }
}

/// [`authorize_in`] in its own transaction, for commands that change
/// nothing and so are not recorded.
pub async fn authorize(
    pool: &SqlitePool,
    account_id: Uuid,
    generation: i64,
    bot_id: Uuid,
    turn_id: Uuid,
    command: &MailCommand,
) -> Result<(), StoreError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
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

/// Settle a dispatching action with the receipt later retries get. Returns
/// false when it was not dispatching (already settled, or made uncertain by
/// [`recover`]).
pub async fn finish_action(
    pool: &SqlitePool,
    account_id: Uuid,
    action_id: Uuid,
    status: ActionStatus,
    receipt: &Value,
) -> Result<bool, StoreError> {
    if status == ActionStatus::Dispatching {
        return Err(StoreError::Invalid(
            "an action finishes as succeeded, failed or uncertain",
        ));
    }
    let mut text = receipt.to_string();
    if text.len() > MAX_RESULT {
        text = serde_json::json!({
            "action_id": action_id,
            "status": status.as_str(),
            "omitted": "the result was too large to record",
        })
        .to_string();
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let done = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE connector_actions SET status = ?3, result = ?4, finished_at = {NOW}
         WHERE account_id = ?1 AND action_id = ?2 AND status = 'dispatching'"
    )))
    .bind(account_id)
    .bind(action_id)
    .bind(status.as_str())
    .bind(text)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 1 {
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
    }
    tx.commit().await?;
    Ok(done.rows_affected() == 1)
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
