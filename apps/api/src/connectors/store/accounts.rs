//! Mail accounts: their encrypted credentials, state, health and generation.

use super::*;

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

fn validate(config: &MailConfig, credential: &Credential) -> Result<(), StoreError> {
    config
        .validate()
        .map_err(|_| StoreError::Invalid("invalid mail configuration"))?;
    credential
        .validate(config.provider)
        .map_err(|_| StoreError::Invalid("invalid mail credential"))
}

#[derive(sqlx::FromRow)]
pub(super) struct AccountRow {
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
    if code.trim().is_empty() || code.len() > 64 || code.contains('\0') {
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
    ACTIONS.recover(&mut tx).await?;
    tx.commit().await
}
