//! Installed skills and the owner cards that install them.
//!
//! `skill_install` fetches a skill and records a pending card holding its
//! exact files; the attempt then ends and its turn waits. Approving the card
//! installs those files, stores any keys as the task secret `skill-<id>`,
//! settles the card, and requeues the turn, in one transaction. Ending the
//! turn for any reason withdraws a pending card (a trigger).

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use else_wire::skills::{InstalledSkill, SkillInstall, SkillInstallStatus, SkillScope};
use serde::Serialize;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;
use super::task_secrets::{self, SecretError};
use crate::credentials::SecretKey;
use crate::db::in_ids;
use crate::skills::{Package, SkillFile, SkillSource, secret_name};

/// Install cards per owner request (turn).
pub const MAX_PER_TURN: i64 = 3;
pub const MAX_REASON: usize = 500;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    serde::Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    sqlx::Type,
)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Scope {
    All,
    Bot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Installed,
    Declined,
    Cancelled,
}

/// An install card as the conversation shows it, without file contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallInfo {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub turn_id: Uuid,
    pub skill_id: String,
    pub name: String,
    pub description: String,
    pub reason: String,
    pub source: SkillSource,
    pub files: Vec<SkillFile>,
    pub env: Vec<String>,
    pub secret: String,
    pub status: Status,
    pub scope: Option<Scope>,
    pub key_stored: bool,
    pub created_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
}

impl From<Scope> for SkillScope {
    fn from(scope: Scope) -> Self {
        match scope {
            Scope::All => Self::All,
            Scope::Bot => Self::Bot,
        }
    }
}

impl From<&InstallInfo> for SkillInstall {
    fn from(install: &InstallInfo) -> Self {
        Self {
            id: install.id,
            bot_id: Some(install.bot_id),
            turn_id: Some(install.turn_id),
            skill_id: install.skill_id.clone(),
            name: install.name.clone(),
            description: install.description.clone(),
            reason: install.reason.clone(),
            source: (&install.source).into(),
            files: install.files.iter().map(Into::into).collect(),
            env: install.env.clone(),
            secret: install.secret.clone(),
            status: match install.status {
                Status::Pending => SkillInstallStatus::Pending,
                Status::Installed => SkillInstallStatus::Installed,
                Status::Declined => SkillInstallStatus::Declined,
                Status::Cancelled => SkillInstallStatus::Cancelled,
            },
            scope: install.scope.map(Into::into),
            key_stored: install.key_stored,
            created_at: install.created_at,
            settled_at: install.settled_at,
        }
    }
}

type InstallRow = (
    Uuid,
    Uuid,
    Uuid,
    String,
    String,
    String,
    String,
    Json<SkillSource>,
    Json<Vec<SkillFile>>,
    Json<Vec<String>>,
    Status,
    Option<Scope>,
    bool,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
);

const INSTALL_COLUMNS: &str = "id, bot_id, turn_id, skill_id, name, description, reason, source, \
     files, env_names, status, scope, key_stored, created_at, settled_at";

fn install(
    (
        id,
        bot_id,
        turn_id,
        skill_id,
        name,
        description,
        reason,
        source,
        files,
        env,
        status,
        scope,
        key_stored,
        created_at,
        settled_at,
    ): InstallRow,
) -> InstallInfo {
    InstallInfo {
        id,
        bot_id,
        turn_id,
        secret: secret_name(&skill_id),
        skill_id,
        name,
        description,
        reason,
        source: source.0,
        files: files.0,
        env: env.0,
        status,
        scope,
        key_stored,
        created_at,
        settled_at,
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RequestError {
    #[error(
        "only the owner's own conversation or a task delegated for an owner request can install skills"
    )]
    NotAllowed,
    #[error("this attempt already asked or delegated; end it and wait for the answer")]
    AttemptParked,
    #[error("this request already asked to install {MAX_PER_TURN} skills")]
    BudgetExhausted,
    #[error(
        "skill {0} is already installed at commit {1}; the owner can delete it in Settings to reinstall"
    )]
    AlreadyInstalled(String, String),
    #[error("the request is no longer running")]
    NotRunning,
    #[error("database error")]
    Db,
}

impl From<sqlx::Error> for RequestError {
    fn from(err: sqlx::Error) -> Self {
        tracing::warn!(error = %err, "skill install request failed");
        RequestError::Db
    }
}

async fn installed_commit(
    conn: &mut SqliteConnection,
    id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT json_extract(source, '$.commit') FROM skills WHERE id = ?1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
}

/// Record an install card from the provider attempt of `turn_id`. A retry
/// for the same skill in this request returns its card.
pub async fn create(
    pool: &SqlitePool,
    turn_id: Uuid,
    reason: &str,
    env: &[String],
    package: &Package,
) -> Result<InstallInfo, RequestError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = super::turns::asking_caller(&mut tx, turn_id)
        .await?
        .ok_or(RequestError::NotRunning)?;
    if !super::turns::may_ask_owner(&mut tx, turn_id, &caller).await? {
        return Err(RequestError::NotAllowed);
    }
    let existing: Option<InstallRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {INSTALL_COLUMNS} FROM skill_installs WHERE turn_id = ?1 AND skill_id = ?2"
    )))
    .bind(turn_id)
    .bind(&package.id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = existing {
        return Ok(install(row));
    }
    if let Some(commit) = installed_commit(&mut tx, &package.id).await? {
        return Err(RequestError::AlreadyInstalled(package.id.clone(), commit));
    }
    if super::turns::attempt_parked(&mut tx, turn_id, caller.attempt).await? {
        return Err(RequestError::AttemptParked);
    }
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM skill_installs WHERE turn_id = ?1")
        .bind(turn_id)
        .fetch_one(&mut *tx)
        .await?;
    if total >= MAX_PER_TURN {
        return Err(RequestError::BudgetExhausted);
    }
    let row: InstallRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO skill_installs (id, bot_id, turn_id, attempt, skill_id, name, description,
             reason, source, files, contents, env_names)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) RETURNING {INSTALL_COLUMNS}"
    )))
    .bind(Uuid::new_v4())
    .bind(caller.bot_id)
    .bind(turn_id)
    .bind(caller.attempt)
    .bind(&package.id)
    .bind(&package.name)
    .bind(&package.description)
    .bind(reason)
    .bind(Json(&package.source))
    .bind(Json(crate::skills::file_infos(&package.files)))
    .bind(Json(&package.files))
    .bind(Json(env))
    .fetch_one(&mut *tx)
    .await?;
    events::turn_changed(&mut tx, caller.bot_id, turn_id).await?;
    tx.commit().await?;
    Ok(install(row))
}

/// Every install card of these turns, oldest first per turn.
pub async fn for_turns(
    conn: &mut SqliteConnection,
    turn_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<InstallInfo>>, sqlx::Error> {
    let rows: Vec<InstallRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {INSTALL_COLUMNS} FROM skill_installs
         WHERE {}
         ORDER BY created_at, id",
        in_ids("turn_id", "?1")
    )))
    .bind(Json(turn_ids))
    .fetch_all(&mut *conn)
    .await?;
    Ok(super::holds::by_turn(
        rows.into_iter().map(install),
        |row| row.turn_id,
    ))
}

#[derive(Debug, thiserror::Error)]
pub enum SettleError {
    #[error("not found")]
    NotFound,
    #[error("the request was already answered")]
    Settled(Status),
    #[error("send exactly the requested variables: {}", .0.join(", "))]
    EnvMismatch(Vec<String>),
    #[error("skill {0} was installed by another request first")]
    Exists(String),
    #[error(transparent)]
    Secret(#[from] SecretError),
}

impl From<sqlx::Error> for SettleError {
    fn from(err: sqlx::Error) -> Self {
        Self::Secret(SecretError::Db(err))
    }
}

/// The result of an owner answer. `changed` is false for a retry of an
/// answer already recorded; nothing was written then.
#[derive(Debug, Clone)]
pub struct Settled {
    pub install: InstallInfo,
    pub changed: bool,
    pub requeued: bool,
}

async fn load(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    id: Uuid,
) -> Result<InstallInfo, SettleError> {
    let row: Option<InstallRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM skill_installs r JOIN bots b ON b.id = r.bot_id
         WHERE r.id = ?1 AND r.bot_id = ?2 AND b.status = 'active'",
        INSTALL_COLUMNS
            .split(", ")
            .map(|c| format!("r.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ")
    )))
    .bind(id)
    .bind(bot_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(install).ok_or(SettleError::NotFound)
}

/// A card with the full text of every file, for the owner's review.
pub async fn review(
    pool: &SqlitePool,
    bot_id: Uuid,
    id: Uuid,
) -> Result<(InstallInfo, BTreeMap<String, String>), SettleError> {
    let mut conn = pool.acquire().await?;
    let card = load(&mut conn, bot_id, id).await?;
    let Json(contents): Json<BTreeMap<String, String>> =
        sqlx::query_scalar("SELECT contents FROM skill_installs WHERE id = ?1")
            .bind(id)
            .fetch_one(&mut *conn)
            .await?;
    Ok((card, contents))
}

fn answered(install: InstallInfo, wanted: Status) -> Result<Settled, SettleError> {
    if install.status == wanted {
        Ok(Settled {
            install,
            changed: false,
            requeued: false,
        })
    } else {
        Err(SettleError::Settled(install.status))
    }
}

async fn settle_in(
    conn: &mut SqliteConnection,
    card: &InstallInfo,
    status: Status,
    scope: Option<Scope>,
    key_stored: bool,
) -> Result<Settled, sqlx::Error> {
    let row: InstallRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE skill_installs SET status = ?2, scope = ?3, key_stored = ?4,
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 RETURNING {INSTALL_COLUMNS}"
    )))
    .bind(card.id)
    .bind(status)
    .bind(scope)
    .bind(key_stored)
    .fetch_one(&mut *conn)
    .await?;
    let requeued = super::turns::lifecycle::continue_waiting(conn, card.turn_id).await?;
    events::turn_changed(conn, card.bot_id, card.turn_id).await?;
    Ok(Settled {
        install: install(row),
        changed: true,
        requeued,
    })
}

/// Install a pending card's skill for `scope`, store its keys when `env`
/// is given, and continue its turn, atomically. `env` must name exactly
/// the card's variables. A retry after success changes nothing.
pub async fn approve(
    pool: &SqlitePool,
    key: Option<&SecretKey>,
    bot_id: Uuid,
    id: Uuid,
    scope: Scope,
    env: Option<&BTreeMap<String, String>>,
) -> Result<Settled, SettleError> {
    let card = load(&mut *pool.acquire().await?, bot_id, id).await?;
    if card.status != Status::Pending {
        return answered(card, Status::Installed);
    }
    let sealed = match env {
        Some(env) => {
            if !env.keys().eq(card.env.iter()) {
                return Err(SettleError::EnvMismatch(card.env));
            }
            let key = key.ok_or(SecretError::NoKey)?;
            Some(task_secrets::seal_env(key, &card.secret, env)?)
        }
        None => None,
    };
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let card = load(&mut tx, bot_id, id).await?;
    if card.status != Status::Pending {
        return answered(card, Status::Installed);
    }
    if installed_commit(&mut tx, &card.skill_id).await?.is_some() {
        return Err(SettleError::Exists(card.skill_id));
    }
    sqlx::query(
        "INSERT INTO skills (id, name, description, source, scope, bot_id, env_names, installed_by)
         SELECT skill_id, name, description, source, ?2, ?3, env_names, bot_id
         FROM skill_installs WHERE id = ?1",
    )
    .bind(id)
    .bind(scope)
    .bind((scope == Scope::Bot).then_some(card.bot_id))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO skill_files (skill_id, path, content)
         SELECT r.skill_id, f.key, f.value FROM skill_installs r, json_each(r.contents) f
         WHERE r.id = ?1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if let Some(sealed) = &sealed {
        task_secrets::write_in(
            &mut tx,
            &card.secret,
            sealed,
            &task_secrets::Meta::default(),
        )
        .await?;
    }
    let settled = settle_in(
        &mut tx,
        &card,
        Status::Installed,
        Some(scope),
        sealed.is_some(),
    )
    .await?;
    tx.commit().await?;
    Ok(settled)
}

/// The owner declines a pending card; its turn continues without it.
pub async fn decline(pool: &SqlitePool, bot_id: Uuid, id: Uuid) -> Result<Settled, SettleError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let card = load(&mut tx, bot_id, id).await?;
    if card.status != Status::Pending {
        return answered(card, Status::Declined);
    }
    let settled = settle_in(&mut tx, &card, Status::Declined, None, false).await?;
    tx.commit().await?;
    Ok(settled)
}

/// An owner answer a continuation has not received yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub id: Uuid,
    pub skill_id: String,
    pub installed: bool,
    pub scope: Option<Scope>,
    pub env: Vec<String>,
    pub key_stored: bool,
}

type OutcomeRow = (Uuid, String, Status, Option<Scope>, Json<Vec<String>>, bool);

pub async fn undelivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Vec<Outcome>, sqlx::Error> {
    let rows: Vec<OutcomeRow> = sqlx::query_as(
        "SELECT id, skill_id, status, scope, env_names, key_stored FROM skill_installs
         WHERE turn_id = ?1 AND delivered_attempt IS NULL AND status IN ('installed', 'declined')
         ORDER BY created_at, id",
    )
    .bind(turn_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, skill_id, status, scope, env, key_stored)| Outcome {
            id,
            skill_id,
            installed: status == Status::Installed,
            scope,
            env: env.0,
            key_stored,
        })
        .collect())
}

/// An installed skill as Settings lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    pub scope: Scope,
    pub bot_id: Option<Uuid>,
    pub bot_name: Option<String>,
    pub installed_by: Uuid,
    pub installed_by_name: String,
    pub installed_at: DateTime<Utc>,
    pub files: Vec<SkillFile>,
    pub env: Vec<String>,
    pub secret: String,
    pub key_stored: bool,
}

impl From<&SkillInfo> for InstalledSkill {
    fn from(skill: &SkillInfo) -> Self {
        Self {
            id: skill.id.clone(),
            name: skill.name.clone(),
            description: skill.description.clone(),
            source: (&skill.source).into(),
            scope: skill.scope.into(),
            bot_id: skill.bot_id,
            bot_name: skill.bot_name.clone(),
            installed_by: skill.installed_by,
            installed_by_name: skill.installed_by_name.clone(),
            installed_at: skill.installed_at,
            files: skill.files.iter().map(Into::into).collect(),
            env: skill.env.clone(),
            secret: skill.secret.clone(),
            key_stored: skill.key_stored,
        }
    }
}

type SkillRow = (
    String,
    String,
    String,
    Json<SkillSource>,
    Scope,
    Option<Uuid>,
    Option<String>,
    Uuid,
    String,
    DateTime<Utc>,
    Json<Vec<String>>,
    bool,
);

const SKILL_SELECT: &str =
    "SELECT s.id, s.name, s.description, s.source, s.scope, s.bot_id, b.name,
         s.installed_by, i.name, s.installed_at, s.env_names,
         EXISTS (SELECT 1 FROM task_secrets t WHERE t.name = 'skill-' || s.id)
     FROM skills s
     LEFT JOIN bots b ON b.id = s.bot_id
     JOIN bots i ON i.id = s.installed_by";

async fn files_of(
    conn: &mut SqliteConnection,
    id: &str,
) -> Result<BTreeMap<String, String>, sqlx::Error> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT path, content FROM skill_files WHERE skill_id = ?1 ORDER BY path")
            .bind(id)
            .fetch_all(&mut *conn)
            .await?;
    Ok(rows.into_iter().collect())
}

async fn file_infos(conn: &mut SqliteConnection, id: &str) -> Result<Vec<SkillFile>, sqlx::Error> {
    let rows: Vec<(String, i64, bool)> = sqlx::query_as(
        "SELECT path, length(CAST(content AS BLOB)), substr(content, 1, 2) = '#!'
         FROM skill_files WHERE skill_id = ?1 ORDER BY path",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(path, size, shebang)| SkillFile {
            script: crate::skills::is_script(&path, if shebang { "#!" } else { "" }),
            size: u64::try_from(size).unwrap_or_default(),
            path,
        })
        .collect())
}

fn skill(row: SkillRow, files: Vec<SkillFile>) -> SkillInfo {
    let (id, name, description, source, scope, bot_id, bot_name, by, by_name, at, env, key) = row;
    SkillInfo {
        secret: secret_name(&id),
        id,
        name,
        description,
        source: source.0,
        scope,
        bot_id,
        bot_name,
        installed_by: by,
        installed_by_name: by_name,
        installed_at: at,
        files,
        env: env.0,
        key_stored: key,
    }
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<SkillInfo>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let rows: Vec<SkillRow> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!("{SKILL_SELECT} ORDER BY s.id")))
            .fetch_all(&mut *conn)
            .await?;
    let mut skills = Vec::with_capacity(rows.len());
    for row in rows {
        let files = file_infos(&mut conn, &row.0).await?;
        skills.push(skill(row, files));
    }
    Ok(skills)
}

/// One installed skill with the text of every file.
pub async fn get(
    pool: &SqlitePool,
    id: &str,
) -> Result<Option<(SkillInfo, BTreeMap<String, String>)>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let row: Option<SkillRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SKILL_SELECT} WHERE s.id = ?1"
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let infos = file_infos(&mut conn, id).await?;
    let files = files_of(&mut conn, id).await?;
    Ok(Some((skill(row, infos), files)))
}

/// Remove a skill, its files, and its keys. Returns whether it existed.
pub async fn delete(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM skill_files WHERE skill_id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let removed = sqlx::query("DELETE FROM skills WHERE id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    if removed {
        sqlx::query("DELETE FROM task_secrets WHERE name = ?1")
            .bind(secret_name(id))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(removed)
}

/// The skills `bot_id` may use: shared ones and its own.
pub async fn visible(pool: &SqlitePool, bot_id: Uuid) -> Result<Vec<SkillInfo>, sqlx::Error> {
    Ok(list(pool)
        .await?
        .into_iter()
        .filter(|s| s.bot_id.is_none_or(|owner| owner == bot_id))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_source_reads_existing_json() {
        let json = r#"{"url":"https://github.com/org/tools/tree/main/brief","repo":"org/tools","path":"brief","commit":"4a6b2f6f0dfae01899cb9fc4266e20b813138377"}"#;
        let stored: SkillSource = serde_json::from_str(json).unwrap();
        let wire: else_wire::skills::SkillSource = (&stored).into();
        assert_eq!(
            wire,
            else_wire::skills::SkillSource {
                url: "https://github.com/org/tools/tree/main/brief".into(),
                repo: "org/tools".into(),
                path: "brief".into(),
                commit: "4a6b2f6f0dfae01899cb9fc4266e20b813138377".into(),
            }
        );
        assert_eq!(serde_json::to_string(&stored).unwrap(), json);
        let from_wire: SkillSource = wire.into();
        assert_eq!(serde_json::to_string(&from_wire).unwrap(), json);
    }

    #[test]
    fn skill_install_files_read_existing_json() {
        let json = r#"[{"path":"SKILL.md","size":150,"script":false},{"path":"scripts/brief.sh","size":62,"script":true}]"#;
        let files: Vec<SkillFile> = serde_json::from_str(json).unwrap();
        let wire: Vec<else_wire::skills::SkillFile> = files.iter().map(Into::into).collect();
        assert_eq!(
            wire,
            [
                else_wire::skills::SkillFile {
                    path: "SKILL.md".into(),
                    size: 150,
                    script: false
                },
                else_wire::skills::SkillFile {
                    path: "scripts/brief.sh".into(),
                    size: 62,
                    script: true
                },
            ]
        );
        assert_eq!(serde_json::to_string(&files).unwrap(), json);
    }
}
