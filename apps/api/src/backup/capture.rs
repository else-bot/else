//! Capture one database backup set and publish it locally.
//!
//! Table digests and the database image share one SQLite snapshot. The image
//! is size-checked before allocation and encrypted from memory, never staged
//! as plaintext. Completion is published only after every object is synced.

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use super::{
    Application, Artifact, BackupError, BotEntry, COMPLETE, Completion, DATABASE, DatabaseParams,
    MANIFEST, Manifest, ObjectRef, SCHEMA_VERSION, Sqlite, TableDigest,
};
use crate::credentials::SecretKey;

#[derive(Debug, Clone)]
pub struct BackupConfig {
    pub dir: PathBuf,
    /// Completed local sets kept (at least one).
    pub keep: usize,
    /// Largest dump accepted, in plaintext bytes; bounds temporary storage.
    pub max_bytes: u64,
    /// Time between scheduled sets; zero disables the schedule.
    pub interval: std::time::Duration,
}

#[derive(Debug, Clone)]
pub struct SetSummary {
    pub set_id: String,
    pub path: PathBuf,
    pub manifest: Manifest,
    /// Replication after this capture, when a bucket is configured: the
    /// local set is complete either way.
    pub remote: Option<Result<super::replicate::Stats, String>>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Row counts and content digests of every application table, in the
/// connection's current snapshot. Row hashes are streamed in sorted order
/// into one incremental SHA-256, so no table is aggregated in memory.
pub async fn table_digests(
    conn: &mut sqlx::SqliteConnection,
) -> Result<Vec<TableDigest>, sqlx::Error> {
    use futures_util::TryStreamExt;
    use sha2::Digest;
    use sqlx::{Row, ValueRef};
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         AND name NOT LIKE 'memory_search%' ORDER BY name",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut digests = Vec::new();
    for table in tables {
        let quoted = format!("\"{}\"", table.replace('"', "\"\""));
        let sql = format!("SELECT * FROM {quoted}");
        let mut rows = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch(&mut *conn);
        let mut hashes = Vec::new();
        while let Some(row) = rows.try_next().await? {
            let mut hash = sha2::Sha256::new();
            for i in 0..row.len() {
                use sqlx::TypeInfo;
                let raw = row.try_get_raw(i)?;
                if raw.is_null() {
                    hash.update([0]);
                    continue;
                }
                let bytes = match raw.type_info().name() {
                    "INTEGER" => row.try_get::<i64, _>(i)?.to_le_bytes().to_vec(),
                    "REAL" => row.try_get::<f64, _>(i)?.to_le_bytes().to_vec(),
                    _ => row.try_get::<Vec<u8>, _>(i)?,
                };
                hash.update([1]);
                hash.update((bytes.len() as u64).to_le_bytes());
                hash.update(bytes);
            }
            hashes.push(hex::encode(hash.finalize()));
        }
        hashes.sort_unstable();
        let mut hash = sha2::Sha256::new();
        for row in &hashes {
            hash.update(row.as_bytes());
        }
        digests.push(TableDigest {
            table,
            rows: hashes.len() as i64,
            sha256: hex::encode(hash.finalize()),
        });
    }
    Ok(digests)
}

fn publish_completion(set_dir: &Path, completion: &Completion) -> Result<(), BackupError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let temp = set_dir.join(".complete.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| BackupError::io(temp.display(), e))?;
    let bytes = serde_json::to_vec_pretty(completion).expect("serializable");
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| BackupError::io(temp.display(), e))?;
    drop(file);
    let linked = std::fs::hard_link(&temp, set_dir.join(COMPLETE));
    let _ = std::fs::remove_file(&temp);
    linked.map_err(|e| BackupError::io(set_dir.join(COMPLETE).display(), e))?;
    super::sync_dir(set_dir)
}

/// Capture, encrypt, and publish one database-only set.
pub async fn create_set(
    pool: &SqlitePool,
    params: &DatabaseParams,
    key: &SecretKey,
    config: &BackupConfig,
    set_id: &str,
    cancel: &tokio_util::sync::CancellationToken,
    coordinator: &Coordinator,
) -> Result<SetSummary, BackupError> {
    if !super::valid_set_id(set_id) {
        return Err(BackupError::Invalid("not a set ID".into()));
    }
    super::ensure_private_dir(&config.dir)?;
    let set_id = set_id.to_owned();
    let staging = super::staging_path(&config.dir, &set_id);
    super::private_dir(&staging)?;
    let result = match super::mark_owned(&staging) {
        Ok(()) => {
            capture_into(
                pool,
                params,
                key,
                config,
                &set_id,
                &staging,
                cancel,
                coordinator,
            )
            .await
        }
        Err(err) => Err(err),
    };
    match result {
        Ok((manifest, completion)) => {
            let final_dir = config.dir.join(&set_id);
            let published = (|| {
                super::rename_noreplace(&staging, &final_dir)?;
                super::sync_dir(&config.dir)?;
                if crate::faults::hit("backup.before_complete") {
                    return Err(BackupError::Interrupted);
                }
                publish_completion(&final_dir, &completion)
            })();
            if let Err(err) = published {
                let _ = std::fs::remove_dir_all(&staging);
                if !matches!(err, BackupError::Interrupted) {
                    let _ = std::fs::remove_dir_all(&final_dir);
                }
                return Err(err);
            }
            Ok(SetSummary {
                set_id,
                path: final_dir,
                manifest,
                remote: None,
            })
        }
        Err(err) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(err)
        }
    }
}

/// What a coordinated capture may use: the scheduler's gate and drain, and
/// the bot computers. Without them a set is database only, and says so.
#[derive(Clone, Default)]
pub struct Coordinator {
    pub scheduler: Option<std::sync::Arc<crate::scheduler::Scheduler>>,
    pub computers: Option<std::sync::Arc<crate::computers::ComputerManager>>,
    /// How long active turns may take to finish before the set is deferred.
    pub drain: std::time::Duration,
    /// No API runs (its singleton lock is held by this process), so nothing
    /// else can change state during the capture.
    pub exclusive: bool,
    /// The bots' project directories (`ELSE_BOTS_DIR`), captured at the
    /// checkpoint.
    pub projects_dir: Option<PathBuf>,
}

async fn seal_file(
    plain: PathBuf,
    target: PathBuf,
    identity: age::x25519::Identity,
    limit: u64,
) -> Result<super::Sealed, BackupError> {
    tokio::task::spawn_blocking(move || {
        let result = std::fs::File::open(&plain)
            .map_err(|e| BackupError::io(plain.display(), e))
            .and_then(|mut file| super::seal(&target, &identity, &mut file, limit));
        let _ = std::fs::remove_file(&plain);
        result
    })
    .await
    .map_err(|e| BackupError::Io(e.to_string()))?
}

async fn capture_computers(
    computers: &crate::computers::ComputerManager,
    bots: &[(uuid::Uuid, String, String)],
    staging: &Path,
    identity: &age::x25519::Identity,
    limit: u64,
) -> Result<
    (
        Vec<Artifact>,
        std::collections::HashMap<uuid::Uuid, (String, Option<super::ComputerEntry>)>,
    ),
    BackupError,
> {
    use crate::computers::{ComputerError, DiskCapture};
    let mut artifacts = Vec::new();
    let mut entries = std::collections::HashMap::new();
    for (bot, name, status) in bots {
        if status != "active" {
            entries.insert(*bot, ("omitted-deleted-bot".to_owned(), None));
            continue;
        }
        let object = format!("vm-{bot}.msb.age");
        let plain = staging.join(format!("vm-{bot}.msb"));
        let captured = computers.capture_disk(*bot, &plain).await;
        let captured = match captured {
            Ok(captured) => captured,
            Err(ComputerError::Busy(why)) => {
                let _ = std::fs::remove_file(&plain);
                return Err(BackupError::Deferred(format!(
                    "bot {name}'s computer is {why}"
                )));
            }
            Err(err) => {
                let _ = std::fs::remove_file(&plain);
                return Err(BackupError::Producer(format!(
                    "bot {name}'s computer: {err}"
                )));
            }
        };
        match captured {
            DiskCapture::NoComputer => {
                entries.insert(*bot, ("none".to_owned(), None));
            }
            DiskCapture::Missing => {
                entries.insert(*bot, ("missing".to_owned(), None));
            }
            DiskCapture::Exported {
                runtime_name,
                image,
                cpus,
                memory_mib,
                desired_state,
                desktop_port,
                was_running,
                browser_checkpoint,
                project_mount,
            } => {
                let sealed =
                    seal_file(plain, staging.join(&object), identity.clone(), limit).await?;
                let password_file = computers.desktop_password_file(*bot);
                let desktop_password = std::fs::read_to_string(&password_file)
                    .ok()
                    .map(|p| p.trim().to_owned())
                    .filter(|p| !p.is_empty() && p.len() <= 64);
                if desktop_port.is_some() && desktop_password.is_none() {
                    return Err(BackupError::Producer(format!(
                        "bot {name}'s desktop password ({}) is missing or invalid",
                        password_file.display()
                    )));
                }
                artifacts.push(Artifact {
                    kind: "vm".into(),
                    object: object.clone(),
                    bot_id: Some(*bot),
                    ciphertext_bytes: sealed.ciphertext_bytes,
                    ciphertext_sha256: sealed.ciphertext_sha256,
                    plaintext_bytes: sealed.plaintext_bytes,
                    plaintext_sha256: sealed.plaintext_sha256,
                });
                entries.insert(
                    *bot,
                    (
                        "included".to_owned(),
                        Some(super::ComputerEntry {
                            object,
                            runtime_name,
                            runtime_version: crate::computers::runtime::VERIFIED_VERSION.into(),
                            image,
                            cpus,
                            memory_mib,
                            desired_state,
                            network: computers.config().network.clone(),
                            desktop_port,
                            desktop_password,
                            was_running,
                            browser_checkpoint,
                            project_mount: project_mount.map(|m| super::ProjectMount {
                                guest: m.guest,
                                uid: m.uid,
                            }),
                        }),
                    ),
                );
            }
        }
    }
    Ok((artifacts, entries))
}

async fn capture_files(
    pool: &SqlitePool,
    state_dir: &Path,
    staging: &Path,
    identity: &age::x25519::Identity,
    limit: u64,
) -> Result<(Vec<Artifact>, Vec<uuid::Uuid>), BackupError> {
    let rows: Vec<(uuid::Uuid, uuid::Uuid, i64, Vec<u8>)> =
        sqlx::query_as("SELECT id, bot_id, size_bytes, sha256 FROM artifacts ORDER BY id")
            .fetch_all(pool)
            .await?;
    let mut artifacts = Vec::new();
    let mut missing = Vec::new();
    for (id, bot, size, sha) in rows {
        let source = state_dir
            .join("artifacts")
            .join(bot.to_string())
            .join(id.to_string());
        let regular =
            std::fs::symlink_metadata(&source).is_ok_and(|m| m.is_file() && m.len() == size as u64);
        if !regular {
            missing.push(id);
            continue;
        }
        let object = format!("file-{id}.age");
        let target = staging.join(&object);
        let identity = identity.clone();
        let sealed = tokio::task::spawn_blocking(move || {
            let mut file =
                std::fs::File::open(&source).map_err(|e| BackupError::io(source.display(), e))?;
            super::seal(&target, &identity, &mut file, limit)
        })
        .await
        .map_err(|e| BackupError::Io(e.to_string()))??;
        if sealed.plaintext_sha256 != hex::encode(&sha) {
            let _ = std::fs::remove_file(staging.join(&object));
            missing.push(id);
            continue;
        }
        artifacts.push(Artifact {
            kind: "file".into(),
            object,
            bot_id: Some(bot),
            ciphertext_bytes: sealed.ciphertext_bytes,
            ciphertext_sha256: sealed.ciphertext_sha256,
            plaintext_bytes: sealed.plaintext_bytes,
            plaintext_sha256: sealed.plaintext_sha256,
        });
    }
    Ok((artifacts, missing))
}

async fn capture_projects(
    bots: &[(uuid::Uuid, String, String)],
    projects_dir: &Path,
    staging: &Path,
    identity: &age::x25519::Identity,
    limit: u64,
) -> Result<(Vec<Artifact>, std::collections::HashMap<uuid::Uuid, String>), BackupError> {
    let mut artifacts = Vec::new();
    let mut states = std::collections::HashMap::new();
    for (bot, name, status) in bots {
        if status != "active" {
            states.insert(*bot, "omitted-deleted-bot".to_owned());
            continue;
        }
        let dir = projects_dir.join(bot.to_string());
        if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            states.insert(*bot, "none".to_owned());
            continue;
        }
        let object = super::projects::object(*bot);
        let plain = staging.join(format!("project-{bot}.tar"));
        let archive_target = plain.clone();
        let unreadable =
            tokio::task::spawn_blocking(move || super::projects::archive(&dir, &archive_target))
                .await
                .map_err(|e| BackupError::Io(e.to_string()))?
                .inspect_err(|_| {
                    let _ = std::fs::remove_file(&plain);
                })?;
        if !unreadable.is_empty() {
            tracing::warn!(bot = %name, entries = unreadable.len(), "project entries could not be read and are not in the set");
        }
        let sealed = seal_file(plain, staging.join(&object), identity.clone(), limit).await?;
        artifacts.push(Artifact {
            kind: "project".into(),
            object,
            bot_id: Some(*bot),
            ciphertext_bytes: sealed.ciphertext_bytes,
            ciphertext_sha256: sealed.ciphertext_sha256,
            plaintext_bytes: sealed.plaintext_bytes,
            plaintext_sha256: sealed.plaintext_sha256,
        });
        states.insert(
            *bot,
            if unreadable.is_empty() {
                "included"
            } else {
                "partial"
            }
            .to_owned(),
        );
    }
    Ok((artifacts, states))
}

#[allow(clippy::too_many_arguments)]
async fn capture_into(
    pool: &SqlitePool,
    _params: &DatabaseParams,
    key: &SecretKey,
    config: &BackupConfig,
    set_id: &str,
    staging: &Path,
    cancel: &tokio_util::sync::CancellationToken,
    coordinator: &Coordinator,
) -> Result<(Manifest, Completion), BackupError> {
    let capture_started_at = now();
    let mut salt = [0u8; 32];
    getrandom::fill(&mut salt).map_err(|e| BackupError::Crypto(e.to_string()))?;
    let encryption = super::encryption_for(key, &salt);
    let identity = super::identity(key, &salt)?;

    if coordinator.exclusive {
        let active: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM turns WHERE status IN {})",
            crate::db::turns::ACTIVE_STATUSES
        )))
        .fetch_one(pool)
        .await?;
        if active {
            return Err(BackupError::Deferred(
                "turns are still active; start the API once so it reconciles them".into(),
            ));
        }
    }
    let gate = match &coordinator.scheduler {
        Some(scheduler) => {
            let deadline = tokio::time::Instant::now() + coordinator.drain;
            let closed = match scheduler.gate().close(coordinator.drain).await {
                Ok(Some(closed)) => closed,
                Ok(None) => {
                    return Err(BackupError::Refused("another backup holds the gate".into()));
                }
                Err(admitted) => {
                    return Err(BackupError::Deferred(format!(
                        "{admitted} requests or tasks did not finish within {} s",
                        coordinator.drain.as_secs()
                    )));
                }
            };
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if !scheduler.drain(left).await? {
                return Err(BackupError::Deferred(format!(
                    "active turns did not finish within {} s",
                    coordinator.drain.as_secs()
                )));
            }
            Some(closed)
        }
        None => None,
    };
    let bot_rows: Vec<(uuid::Uuid, String, String)> =
        sqlx::query_as("SELECT id, name, status FROM bots ORDER BY created_at, id")
            .fetch_all(pool)
            .await?;
    let (mut vm_artifacts, mut vm_entries) = match &coordinator.computers {
        Some(computers) => {
            capture_computers(computers, &bot_rows, staging, &identity, config.max_bytes).await?
        }
        None => Default::default(),
    };
    let (mut file_artifacts, missing_files) = match &coordinator.computers {
        Some(computers) => {
            capture_files(
                pool,
                &computers.config().state_dir,
                staging,
                &identity,
                config.max_bytes,
            )
            .await?
        }
        None => Default::default(),
    };
    let (mut project_artifacts, mut project_states) = match (
        &coordinator.projects_dir,
        gate.is_some() || coordinator.exclusive,
    ) {
        (Some(dir), true) => {
            capture_projects(&bot_rows, dir, staging, &identity, config.max_bytes).await?
        }
        _ => Default::default(),
    };
    if cancel.is_cancelled() {
        return Err(BackupError::Interrupted);
    }

    let mut tx = pool.begin().await?;
    let tables = table_digests(&mut tx).await?;
    let migration_version: i64 =
        sqlx::query_scalar("SELECT coalesce(max(version), 0) FROM _sqlx_migrations WHERE success")
            .fetch_one(&mut *tx)
            .await?;
    let server_version: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(&mut *tx)
        .await?;
    let bots: Vec<(uuid::Uuid, String, String)> =
        sqlx::query_as("SELECT id, name, status FROM bots ORDER BY created_at, id")
            .fetch_all(&mut *tx)
            .await?;
    let sessions: Vec<(uuid::Uuid, String, String, String)> = sqlx::query_as(
        "SELECT bot_id, provider_id, native_session_id, status FROM provider_sessions
         ORDER BY bot_id, created_at",
    )
    .fetch_all(&mut *tx)
    .await?;
    let has_computer: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT bot_id FROM bot_computers WHERE observed_state <> 'absent'")
            .fetch_all(&mut *tx)
            .await?;

    let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(&mut *tx)
        .await?;
    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(&mut *tx)
        .await?;
    if (pages as u64).saturating_mul(page_size as u64) > config.max_bytes {
        return Err(BackupError::TooLarge(config.max_bytes));
    }
    if cancel.is_cancelled() {
        return Err(BackupError::Interrupted);
    }
    if crate::faults::hit("backup.snapshot") {
        return Err(BackupError::Producer("snapshot failed".into()));
    }
    let snapshot = tx.serialize(None).await?;
    tx.commit().await?;
    let target = staging.join(DATABASE);
    let limit = config.max_bytes;
    let dump_identity = identity.clone();
    let capture_cancel = cancel.clone();
    let sealed = tokio::task::spawn_blocking(move || {
        struct Input<'a> {
            bytes: &'a [u8],
            cancel: &'a tokio_util::sync::CancellationToken,
        }
        impl std::io::Read for Input<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.cancel.is_cancelled() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "backup cancelled",
                    ));
                }
                std::io::Read::read(&mut self.bytes, buf)
            }
        }
        let mut input = Input {
            bytes: snapshot.as_ref(),
            cancel: &capture_cancel,
        };
        super::seal(&target, &dump_identity, &mut input, limit)
    })
    .await
    .map_err(|e| BackupError::Producer(e.to_string()))?;
    if cancel.is_cancelled() {
        return Err(BackupError::Interrupted);
    }
    let sealed = sealed?;
    drop(gate);

    let coordinated = coordinator.computers.is_some()
        && (coordinator.scheduler.is_some() || coordinator.exclusive);
    let mut complete = coordinated;
    let entries: Vec<BotEntry> = bots
        .into_iter()
        .map(|(id, name, status)| {
            let (vm, computer) = vm_entries.remove(&id).unwrap_or_else(|| {
                if has_computer.contains(&id) {
                    ("omitted".to_owned(), None)
                } else {
                    ("none".to_owned(), None)
                }
            });
            if vm == "missing" || (vm == "omitted" && status == "active") {
                complete = false;
            }
            let project = project_states
                .remove(&id)
                .unwrap_or_else(|| "omitted".to_owned());
            if status == "active" && !matches!(project.as_str(), "included" | "none") {
                complete = false;
            }
            if computer.as_ref().is_some_and(|c| {
                c.desktop_port.is_some() && c.browser_checkpoint.as_deref() != Some("ok")
            }) {
                complete = false;
            }
            BotEntry {
                id,
                provider_sessions: sessions
                    .iter()
                    .filter(|(bot, ..)| *bot == id)
                    .map(|(_, provider, native, status)| super::SessionEntry {
                        provider: provider.clone(),
                        native_session_id: native.clone(),
                        status: status.clone(),
                    })
                    .collect(),
                name,
                status,
                vm,
                computer,
                provider_state: "omitted-reauth-required".into(),
                project,
            }
        })
        .collect();
    let mut artifacts = vec![Artifact {
        kind: "sqlite".into(),
        object: DATABASE.into(),
        bot_id: None,
        ciphertext_bytes: sealed.ciphertext_bytes,
        ciphertext_sha256: sealed.ciphertext_sha256,
        plaintext_bytes: sealed.plaintext_bytes,
        plaintext_sha256: sealed.plaintext_sha256,
    }];
    artifacts.append(&mut vm_artifacts);
    artifacts.append(&mut file_artifacts);
    let projects_captured =
        !project_artifacts.is_empty() || entries.iter().any(|b| !b.project.starts_with("omitted"));
    let projects_partial = entries.iter().any(|b| b.project == "partial");
    artifacts.append(&mut project_artifacts);
    if !missing_files.is_empty() {
        complete = false;
    }
    let projects = if !projects_captured {
        "omitted"
    } else if projects_partial {
        "partial"
    } else {
        "included"
    }
    .to_owned();
    let coverage = if coordinated {
        super::Coverage {
            database: "included".into(),
            vm: if complete {
                "included".into()
            } else {
                "partial".into()
            },
            projects,
            provider_state: "omitted-reauth-required".into(),
            whole_assistant: complete,
            note: if complete {
                "Whole assistant: the database, every active bot's computer (standalone disk \
                 archives with their images), and every bot's project directory. Native provider \
                 sessions are not included: conversations are rebuilt from SQLite and \
                 providers may need a fresh login."
                    .into()
            } else {
                format!(
                    "Partial: a bot's computer is missing from the runtime, a desktop browser was \
                     not checkpointed, a project directory was not captured whole, or host \
                     artifacts are missing ({}), so this set does not recover the whole assistant.",
                    missing_files.len()
                )
            },
        }
    } else if projects != "omitted" {
        super::Coverage {
            note: "The database and the bots' project directories only: bot computers \
                   (files, browser profiles, running jobs) and native provider sessions are not \
                   included. Restoring this set does not recover the whole assistant."
                .into(),
            projects,
            ..super::database_only_coverage()
        }
    } else {
        super::database_only_coverage()
    };

    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        set_id: set_id.to_owned(),
        created_at: now(),
        capture_started_at,
        capture_finished_at: now(),
        encryption: encryption.clone(),
        application: Application {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            migration_version,
        },
        sqlite: Sqlite {
            server_version: server_version.clone(),
            dump_version: server_version.clone(),
            format: "sqlite3".into(),
            extensions: Vec::new(),
        },
        artifacts,
        tables,
        bots: entries,
        coverage,
    };
    let json = serde_json::to_vec(&manifest).expect("serializable");
    let sealed_manifest = super::seal(
        &staging.join(MANIFEST),
        &identity,
        &mut json.as_slice(),
        super::MAX_MANIFEST_BYTES,
    )?;
    super::sync_dir(staging)?;
    let completion = Completion {
        schema_version: SCHEMA_VERSION,
        set_id: set_id.to_owned(),
        encryption,
        manifest: ObjectRef {
            object: MANIFEST.into(),
            ciphertext_bytes: sealed_manifest.ciphertext_bytes,
            ciphertext_sha256: sealed_manifest.ciphertext_sha256,
        },
        whole_assistant: manifest.coverage.whole_assistant,
    };
    Ok((manifest, completion))
}
