//! Capture one database backup set and publish it locally.
//!
//! The table digests and `pg_dump` share one exported snapshot, so the rows
//! the manifest describes are exactly the rows in the dump. The dump streams
//! from `pg_dump`'s stdout through age into a private staging directory; it
//! is never written in plaintext. The set becomes visible under its final
//! name only after every object and the encrypted manifest are synced, and
//! counts as a backup only once `complete.json` is linked in (never
//! replacing one). A failed producer, encryption error, size overrun, or
//! interruption leaves no completed set.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sqlx::PgPool;

use super::{
    Application, Artifact, BackupError, BotEntry, COMPLETE, Completion, DATABASE, MANIFEST,
    Manifest, ObjectRef, PgParams, Postgres, SCHEMA_VERSION, TableDigest,
};
use crate::credentials::SecretKey;

#[derive(Debug, Clone)]
pub struct BackupConfig {
    pub dir: PathBuf,
    pub pg_dump: PathBuf,
    pub pg_restore: PathBuf,
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
pub async fn table_digests(conn: &mut sqlx::PgConnection) -> Result<Vec<TableDigest>, sqlx::Error> {
    use futures_util::TryStreamExt;
    use sha2::Digest;
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename::text FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut digests = Vec::new();
    for table in tables {
        // The name comes from the catalog; quote it as an identifier.
        let quoted = format!("\"{}\"", table.replace('"', "\"\""));
        let sql = format!("SELECT md5(t::text) AS h FROM public.{quoted} t ORDER BY 1");
        let mut rows = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql)).fetch(&mut *conn);
        let mut hasher = sha2::Sha256::new();
        let mut count = 0i64;
        while let Some(hash) = rows.try_next().await? {
            hasher.update(hash.as_bytes());
            count += 1;
        }
        drop(rows);
        digests.push(TableDigest {
            table,
            rows: count,
            sha256: hex::encode(hasher.finalize()),
        });
    }
    Ok(digests)
}

fn dump_version(pg_dump: &Path) -> Result<String, BackupError> {
    let mut command = Command::new(pg_dump);
    command.arg("--version").env_clear();
    let (_, out, _) = super::run_bounded(command)
        .map_err(|e| BackupError::Producer(format!("{}: {e}", pg_dump.display())))?;
    Ok(out
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect())
}

/// The running `pg_dump`, so a cancelled capture can kill it.
#[derive(Default)]
struct Producer {
    pid: std::sync::atomic::AtomicI32,
    cancelled: std::sync::atomic::AtomicBool,
}

impl Producer {
    fn kill(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        self.cancelled.store(true, SeqCst);
        let pid = self.pid.load(SeqCst);
        if pid > 0 {
            // SAFETY: signals our own child, which is reaped by its thread.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// Run `pg_dump` in the exported snapshot and seal its output. The child is
/// killed and reaped if encryption fails; its exit status must be success.
fn dump_and_seal(
    pg_dump: &Path,
    params: &PgParams,
    snapshot: &str,
    target: &Path,
    identity: &age::x25519::Identity,
    limit: u64,
    producer: &Producer,
) -> Result<super::Sealed, BackupError> {
    use std::sync::atomic::Ordering::SeqCst;
    let mut child = Command::new(pg_dump)
        .args(["--format=custom", "--no-owner", "--no-acl"])
        .arg(format!("--snapshot={snapshot}"))
        .env_clear()
        .envs(params.env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| BackupError::Producer(format!("{}: {e}", pg_dump.display())))?;
    producer.pid.store(child.id() as i32, SeqCst);
    if producer.cancelled.load(SeqCst) {
        let _ = child.kill();
    }
    let mut stdout = child.stdout.take().expect("piped");
    let errors = super::drain_tail(child.stderr.take().expect("piped"));
    let sealed = super::seal(target, identity, &mut stdout, limit);
    if sealed.is_err() {
        let _ = child.kill();
    }
    drop(stdout);
    let status = child
        .wait()
        .map_err(|e| BackupError::Producer(e.to_string()))?;
    producer.pid.store(0, SeqCst);
    let errors = errors.join().unwrap_or_default();
    if producer.cancelled.load(SeqCst) {
        let _ = std::fs::remove_file(target);
        return Err(BackupError::Interrupted);
    }
    match sealed {
        Err(err) => Err(err),
        Ok(_) if !status.success() => {
            let _ = std::fs::remove_file(target);
            Err(BackupError::Producer(format!(
                "exited with {status}: {}",
                errors.lines().last().unwrap_or_default()
            )))
        }
        Ok(sealed) => Ok(sealed),
    }
}

/// Write `complete.json` without replacing an existing one: a synced
/// temporary file hard-linked into place, then the directory synced.
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
    pool: &PgPool,
    params: &PgParams,
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
                // Never leave a set this run made behind half-published.
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
}

/// Seal a plaintext file into an artifact and remove the plaintext.
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

/// Capture every active bot's computer disk into sealed artifacts. Live
/// work defers the whole set; nothing is captured half-way.
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
            } => {
                let sealed =
                    seal_file(plain, staging.join(&object), identity.clone(), limit).await?;
                let password_file = computers.desktop_password_file(*bot);
                let desktop_password = std::fs::read_to_string(&password_file)
                    .ok()
                    .map(|p| p.trim().to_owned())
                    .filter(|p| !p.is_empty() && p.len() <= 64);
                // A desktop's password is required to use it after restore.
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
                        }),
                    ),
                );
            }
        }
    }
    Ok((artifacts, entries))
}

/// Seal every host artifact (files bots fetched from their computers) whose
/// file matches its recorded size and hash. Returns the artifacts and the
/// IDs that could not be included.
async fn capture_files(
    pool: &PgPool,
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

#[allow(clippy::too_many_arguments)]
async fn capture_into(
    pool: &PgPool,
    params: &PgParams,
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

    // The application checkpoint: no new turn, mutation, or desktop wake
    // while the computers and the database are captured; current turns
    // finish first or the set is deferred.
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
            // Closing waits for admitted claims, requests, tool calls, and
            // memory work; then the turns themselves drain.
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
    if cancel.is_cancelled() {
        return Err(BackupError::Interrupted);
    }

    // One snapshot for the digests and the dump; held open until the dump
    // has finished. Computers are already stopped and recorded so.
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let snapshot: String = sqlx::query_scalar("SELECT pg_export_snapshot()")
        .fetch_one(&mut *tx)
        .await?;
    let tables = table_digests(&mut tx).await?;
    let migration_version: i64 =
        sqlx::query_scalar("SELECT coalesce(max(version), 0) FROM _sqlx_migrations WHERE success")
            .fetch_one(&mut *tx)
            .await?;
    let server_version: String = sqlx::query_scalar("SHOW server_version")
        .fetch_one(&mut *tx)
        .await?;
    let extensions: Vec<(String, String)> = sqlx::query_as(
        "SELECT extname::text, extversion FROM pg_extension WHERE extname <> 'plpgsql' ORDER BY 1",
    )
    .fetch_all(&mut *tx)
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

    let pg_dump = config.pg_dump.clone();
    let dump_params = params.clone();
    let target = staging.join(DATABASE);
    let limit = config.max_bytes;
    let dump_identity = identity.clone();
    let producer = std::sync::Arc::new(Producer::default());
    let running = producer.clone();
    let mut handle = tokio::task::spawn_blocking(move || {
        let version = dump_version(&pg_dump)?;
        dump_and_seal(
            &pg_dump,
            &dump_params,
            &snapshot,
            &target,
            &dump_identity,
            limit,
            &running,
        )
        .map(|sealed| (sealed, version))
    });
    // A shutdown kills and reaps the dump rather than abandoning it.
    let joined = tokio::select! {
        joined = &mut handle => joined,
        _ = cancel.cancelled() => {
            producer.kill();
            (&mut handle).await
        }
    };
    let sealed = joined.map_err(|e| BackupError::Producer(e.to_string()))?;
    tx.commit().await?;
    let (sealed, dump_version) = sealed?;
    // The sources are fixed: work may resume while the set is published.
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
            // A desktop whose browser was not checkpointed may carry a
            // stale profile lock.
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
            }
        })
        .collect();
    let mut artifacts = vec![Artifact {
        kind: "postgres".into(),
        object: DATABASE.into(),
        bot_id: None,
        ciphertext_bytes: sealed.ciphertext_bytes,
        ciphertext_sha256: sealed.ciphertext_sha256,
        plaintext_bytes: sealed.plaintext_bytes,
        plaintext_sha256: sealed.plaintext_sha256,
    }];
    artifacts.append(&mut vm_artifacts);
    artifacts.append(&mut file_artifacts);
    if !missing_files.is_empty() {
        complete = false;
    }
    let coverage = if coordinated {
        super::Coverage {
            database: "included".into(),
            vm: if complete {
                "included".into()
            } else {
                "partial".into()
            },
            provider_state: "omitted-reauth-required".into(),
            whole_assistant: complete,
            note: if complete {
                "Whole assistant: the database and every active bot's computer (standalone disk \
                 archives with their images). Native provider sessions are not included: \
                 conversations are rebuilt from PostgreSQL and providers may need a fresh login."
                    .into()
            } else {
                format!(
                    "Partial: a bot's computer is missing from the runtime, a desktop browser was \
                     not checkpointed, or host artifacts are missing ({}), so this set does not \
                     recover the whole assistant.",
                    missing_files.len()
                )
            },
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
        postgres: Postgres {
            server_version,
            dump_version,
            format: "custom".into(),
            extensions,
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
