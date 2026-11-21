//! Capture one database backup set and publish it locally.
//!
//! Table digests and the database image share one SQLite snapshot. The image
//! is size-checked before allocation and encrypted from memory, never staged
//! as plaintext. Completion is published only after every object is synced;
//! the worker then moves the set into the local chunk store
//! ([`super::store::adopt`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    Application, Artifact, ArtifactKind, BackupError, BotEntry, Completion, Coverage, Covered,
    DATABASE, MANIFEST, Manifest, ObjectRef, ProjectState, ProviderState, SCHEMA_VERSION, Sqlite,
    TableDigest, VmState,
};
use crate::credentials::SecretKey;
use crate::db::bots::BotStatus;
use crate::maintenance::{Closed, Step};

mod sources;
mod staging;

use sources::{capture_computers, capture_files, capture_mcp, capture_projects};
use staging::StagedSet;

#[derive(Debug, Clone)]
pub struct BackupConfig {
    pub dir: PathBuf,
    /// Completed local sets kept (at least one).
    pub keep: usize,
    /// Largest dump accepted, in plaintext bytes; bounds temporary storage.
    pub max_bytes: u64,
    pub cadence: Cadence,
}

/// When local sets are captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    /// Never on a schedule (`ELSE_BACKUP_INTERVAL_SECS=0`).
    Off,
    /// A fixed interval after the newest set, for operators and tests
    /// (`ELSE_BACKUP_INTERVAL_SECS`).
    Every(std::time::Duration),
    /// Once a day at the owner's chosen time (the default).
    Daily,
}

#[derive(Debug, Clone)]
pub struct SetSummary {
    pub set_id: String,
    pub path: PathBuf,
    pub manifest: Manifest,
    /// Replication after this capture, when a bucket is configured: the
    /// local set is complete either way.
    pub remote: Option<Result<super::replicate::Stats, std::sync::Arc<BackupError>>>,
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

/// Capture, encrypt, and publish a set with the configured checkpoint sources.
pub async fn create_set(
    pool: &SqlitePool,
    key: &SecretKey,
    config: &BackupConfig,
    set_id: &str,
    cancel: &CancellationToken,
    coordinator: &Coordinator,
) -> Result<SetSummary, BackupError> {
    if !super::valid_set_id(set_id) {
        return Err(BackupError::Invalid("not a set ID".into()));
    }
    super::ensure_private_dir(&config.dir)?;
    let staged = StagedSet::create(&config.dir, set_id)?;
    let captured = async {
        let mut salt = [0u8; 32];
        getrandom::fill(&mut salt).map_err(|e| BackupError::Crypto(e.to_string()))?;
        let capture = Capture {
            pool,
            config,
            coordinator,
            cancel,
            set_id,
            staging: &staged.0,
            identity: super::identity(key, &salt)?,
        };
        capture_set(&capture, super::encryption_for(key, &salt)).await
    }
    .await;
    let (manifest, completion) = captured?;
    let path = staged
        .move_into(&config.dir, set_id)?
        .publish(&completion)?;
    Ok(SetSummary {
        set_id: set_id.to_owned(),
        path,
        manifest,
        remote: None,
    })
}

/// What a coordinated capture may use: the scheduler's gate and drain, and
/// filesystem sources. Without a checkpoint, only the database is captured.
#[derive(Clone, Default)]
pub struct Coordinator {
    /// The turns of the API this capture runs in, which it pauses.
    pub turns: Option<Turns>,
    pub computers: Option<std::sync::Arc<crate::computers::ComputerManager>>,
    /// How long active turns may take to finish before the set is deferred.
    pub drain: std::time::Duration,
    /// No API runs (its singleton lock is held by this process), so nothing
    /// else can change state during the capture.
    pub exclusive: bool,
    /// The bots' project directories (`ELSE_BOTS_DIR`), captured at the
    /// checkpoint.
    pub projects_dir: Option<PathBuf>,
    /// Shared MCP packages under the installation's data directory.
    pub mcp_dir: Option<PathBuf>,
}

/// The scheduler of the API a capture runs in, and the gate it admits
/// turns through, which the capture closes before it drains them.
#[derive(Clone)]
pub struct Turns {
    pub scheduler: std::sync::Arc<crate::scheduler::Scheduler>,
    pub gate: std::sync::Arc<crate::maintenance::Gate>,
}

/// How long the last complete backup of the bot computers paused new
/// work. Runs from before `paused_ms` existed count their whole duration.
async fn last_pause(pool: &SqlitePool) -> Result<Option<std::time::Duration>, sqlx::Error> {
    let ms: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(coalesce(paused_ms,
                (julianday(finished_at) - julianday(started_at)) * 86400000) AS INTEGER)
         FROM backup_runs
         WHERE local_status = 'completed' AND finished_at IS NOT NULL
           AND json_extract(coverage, '$.vm') IS NOT 'omitted'
         ORDER BY finished_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(ms.map(|ms| std::time::Duration::from_millis(ms.max(0) as u64)))
}

/// One capture: what it reads, where it stages, and the set's identity.
struct Capture<'a> {
    pool: &'a SqlitePool,
    config: &'a BackupConfig,
    coordinator: &'a Coordinator,
    cancel: &'a CancellationToken,
    set_id: &'a str,
    staging: &'a Path,
    identity: age::x25519::Identity,
}

/// What the checkpoint captured besides the database.
#[derive(Default)]
struct Sources {
    vm_artifacts: Vec<Artifact>,
    vm_entries: HashMap<Uuid, (VmState, Option<super::ComputerEntry>)>,
    file_artifacts: Vec<Artifact>,
    missing_files: usize,
    project_artifacts: Vec<Artifact>,
    project_states: HashMap<Uuid, ProjectState>,
    mcp: Option<Artifact>,
}

/// One consistent read of the database: its sealed image and the rows the
/// manifest describes.
struct Snapshot {
    sealed: super::Sealed,
    tables: Vec<TableDigest>,
    migration_version: i64,
    server_version: String,
    bots: Vec<(Uuid, String, BotStatus)>,
    sessions: Vec<(Uuid, String, String, String)>,
    has_computer: Vec<Uuid>,
}

impl Capture<'_> {
    /// Refuse a capture that cannot be consistent, then close the gate and
    /// drain live work when a scheduler runs.
    async fn quiesce(&self) -> Result<Option<Closed>, BackupError> {
        let (pool, coordinator) = (self.pool, self.coordinator);
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
            // Without the computers there is no per-computer lease check, and a
            // restore releases every lease, so a still-running job could be
            // stopped as idle afterwards.
            if coordinator.computers.is_none() {
                let jobs: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM guest_jobs
                                    WHERE status IN ('starting', 'running', 'cancelling'))",
                )
                .fetch_one(pool)
                .await?;
                if jobs {
                    return Err(BackupError::Deferred(
                        "background jobs are still running in the bot computers; \
                         let them finish or cancel them, then try again"
                            .into(),
                    ));
                }
            }
        }
        let Some(Turns { scheduler, gate }) = &coordinator.turns else {
            return Ok(None);
        };
        let deadline = tokio::time::Instant::now() + coordinator.drain;
        let expected = last_pause(pool).await?;
        let pause = crate::maintenance::Pause::backup(expected);
        let closed = match gate.close(coordinator.drain, pause).await {
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
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if let Err(calls) = closed.capture(left).await {
            return Err(BackupError::Deferred(format!(
                "{calls} tool calls did not finish within {} s",
                coordinator.drain.as_secs()
            )));
        }
        Ok(Some(closed))
    }

    /// Capture computers, host artifacts, project directories and shared
    /// MCPs. Projects and MCPs need the gate or an exclusive run.
    async fn sources(&self, gate: Option<&Closed>) -> Result<Sources, BackupError> {
        let (pool, coordinator, staging, limit) = (
            self.pool,
            self.coordinator,
            self.staging,
            self.config.max_bytes,
        );
        let identity = &self.identity;
        let bots: Vec<(Uuid, String, BotStatus)> =
            sqlx::query_as("SELECT id, name, status FROM bots ORDER BY created_at, id")
                .fetch_all(pool)
                .await?;
        let step = |step| {
            if let Some(closed) = gate {
                closed.step(step);
            }
        };
        let mut sources = Sources::default();
        if let Some(computers) = &coordinator.computers {
            let progress = |done, total| step(Step::Computers { done, total });
            (sources.vm_artifacts, sources.vm_entries) =
                capture_computers(computers, &bots, staging, identity, limit, &progress).await?;
            step(Step::Files);
            let missing;
            (sources.file_artifacts, missing) = capture_files(
                pool,
                &computers.config().state_dir,
                staging,
                identity,
                limit,
            )
            .await?;
            sources.missing_files = missing.len();
        } else {
            step(Step::Files);
        }
        let checkpointed = gate.is_some() || coordinator.exclusive;
        let projects = async {
            match &coordinator.projects_dir {
                Some(dir) if checkpointed => {
                    capture_projects(&bots, dir, staging, identity, limit).await
                }
                _ => Ok(Default::default()),
            }
        };
        let mcp = async {
            match &coordinator.mcp_dir {
                Some(dir) if checkpointed => capture_mcp(dir.clone(), staging, identity, limit)
                    .await
                    .map(Some),
                _ => Ok(None),
            }
        };
        step(Step::Projects);
        let (projects, mcp) = tokio::join!(projects, mcp);
        (sources.project_artifacts, sources.project_states) = projects?;
        sources.mcp = mcp?;
        Ok(sources)
    }

    /// Digest, read and seal the database in one SQLite snapshot.
    async fn snapshot(&self) -> Result<Snapshot, BackupError> {
        let mut tx = self.pool.begin().await?;
        let tables = table_digests(&mut tx).await?;
        let migration_version: i64 = sqlx::query_scalar(
            "SELECT coalesce(max(version), 0) FROM _sqlx_migrations WHERE success",
        )
        .fetch_one(&mut *tx)
        .await?;
        let server_version: String = sqlx::query_scalar("SELECT sqlite_version()")
            .fetch_one(&mut *tx)
            .await?;
        let bots: Vec<(Uuid, String, BotStatus)> =
            sqlx::query_as("SELECT id, name, status FROM bots ORDER BY created_at, id")
                .fetch_all(&mut *tx)
                .await?;
        let sessions: Vec<(Uuid, String, String, String)> = sqlx::query_as(
            "SELECT bot_id, provider_id, native_session_id, status FROM provider_sessions
             ORDER BY bot_id, created_at",
        )
        .fetch_all(&mut *tx)
        .await?;
        let has_computer: Vec<Uuid> =
            sqlx::query_scalar("SELECT bot_id FROM bot_computers WHERE observed_state <> 'absent'")
                .fetch_all(&mut *tx)
                .await?;
        let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&mut *tx)
            .await?;
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&mut *tx)
            .await?;
        if (pages as u64).saturating_mul(page_size as u64) > self.config.max_bytes {
            return Err(BackupError::TooLarge(self.config.max_bytes));
        }
        if self.cancel.is_cancelled() {
            return Err(BackupError::Interrupted);
        }
        if crate::faults::hit("backup.snapshot") {
            return Err(BackupError::Producer("snapshot failed".into()));
        }
        let image = tx.serialize(None).await?;
        tx.commit().await?;
        let target = self.staging.join(DATABASE);
        let (limit, identity, cancel) = (
            self.config.max_bytes,
            self.identity.clone(),
            self.cancel.clone(),
        );
        let sealed = super::blocking(move || {
            struct Input<'a> {
                bytes: &'a [u8],
                cancel: &'a CancellationToken,
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
                bytes: image.as_ref(),
                cancel: &cancel,
            };
            super::seal(&target, &identity, &mut input, limit)
        })
        .await;
        if self.cancel.is_cancelled() {
            return Err(BackupError::Interrupted);
        }
        Ok(Snapshot {
            sealed: sealed?,
            tables,
            migration_version,
            server_version,
            bots,
            sessions,
            has_computer,
        })
    }
}

/// Capture, encrypt and stage every object of the set, then its manifest.
async fn capture_set(
    capture: &Capture<'_>,
    encryption: super::Encryption,
) -> Result<(Manifest, Completion), BackupError> {
    let capture_started_at = now();
    let paused_at = tokio::time::Instant::now();
    let gate = capture.quiesce().await?;
    let sources = capture.sources(gate.as_ref()).await?;
    if capture.cancel.is_cancelled() {
        return Err(BackupError::Interrupted);
    }
    if let Some(closed) = &gate {
        closed.step(Step::Database);
    }
    let snapshot = capture.snapshot().await?;
    if let Some(closed) = gate {
        drop(closed);
        // Messages sent during the pause can now take over and run.
        if let Some(turns) = &capture.coordinator.turns {
            turns.scheduler.wake();
        }
        super::runs::paused(capture.pool, capture.set_id, paused_at.elapsed()).await?;
    }
    let coordinated = capture.coordinator.computers.is_some()
        && (capture.coordinator.turns.is_some() || capture.coordinator.exclusive);
    let manifest = build_manifest(
        capture.set_id,
        encryption,
        capture_started_at,
        sources,
        snapshot,
        coordinated,
    );
    let json = serde_json::to_vec(&manifest).expect("serializable");
    let sealed = super::seal(
        &capture.staging.join(MANIFEST),
        &capture.identity,
        &mut json.as_slice(),
        super::MAX_MANIFEST_BYTES,
    )?;
    super::sync_dir(capture.staging)?;
    let completion = Completion {
        schema_version: SCHEMA_VERSION,
        set_id: manifest.set_id.clone(),
        encryption: manifest.encryption.clone(),
        manifest: ObjectRef {
            object: MANIFEST.into(),
            ciphertext_bytes: sealed.ciphertext_bytes,
            ciphertext_sha256: sealed.ciphertext_sha256,
        },
        whole_assistant: manifest.coverage.whole_assistant,
    };
    Ok((manifest, completion))
}

fn build_manifest(
    set_id: &str,
    encryption: super::Encryption,
    capture_started_at: String,
    mut sources: Sources,
    snapshot: Snapshot,
    coordinated: bool,
) -> Manifest {
    let Snapshot {
        sealed,
        tables,
        migration_version,
        server_version,
        bots,
        sessions,
        has_computer,
    } = snapshot;
    let entries: Vec<BotEntry> = bots
        .into_iter()
        .map(|(id, name, status)| {
            let (vm, computer) = sources.vm_entries.remove(&id).unwrap_or_else(|| {
                if has_computer.contains(&id) {
                    (VmState::Omitted, None)
                } else {
                    (VmState::None, None)
                }
            });
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
                provider_state: ProviderState::OmittedReauthRequired,
                project: sources.project_states.remove(&id).unwrap_or_default(),
            }
        })
        .collect();
    let coverage = Coverage::derive(
        coordinated,
        &entries,
        sources.missing_files,
        sources.mcp.is_some(),
    );
    let mut artifacts = vec![Artifact {
        kind: ArtifactKind::Sqlite,
        object: DATABASE.into(),
        bot_id: None,
        ciphertext_bytes: sealed.ciphertext_bytes,
        ciphertext_sha256: sealed.ciphertext_sha256,
        plaintext_bytes: sealed.plaintext_bytes,
        plaintext_sha256: sealed.plaintext_sha256,
    }];
    artifacts.append(&mut sources.vm_artifacts);
    artifacts.append(&mut sources.file_artifacts);
    artifacts.append(&mut sources.project_artifacts);
    artifacts.extend(sources.mcp);
    Manifest {
        schema_version: SCHEMA_VERSION,
        set_id: set_id.to_owned(),
        created_at: now(),
        capture_started_at,
        capture_finished_at: now(),
        encryption,
        application: Application {
            version: env!("ELSE_VERSION").to_owned(),
            migration_version,
        },
        sqlite: Sqlite {
            server_version: server_version.clone(),
            dump_version: server_version,
            format: "sqlite3".into(),
            extensions: Vec::new(),
        },
        artifacts,
        tables,
        bots: entries,
        coverage,
    }
}

impl Coverage {
    /// What a set with these bot entries covers. A coordinated capture
    /// (computers and a checkpoint) recovers the whole assistant only when
    /// no active bot lacks its computer or project directory, every
    /// desktop browser was checkpointed, no host artifact is missing, and
    /// the shared MCPs are included.
    fn derive(coordinated: bool, bots: &[BotEntry], missing_files: usize, mcp: bool) -> Self {
        let lacking = |bot: &BotEntry| {
            let active = bot.status == BotStatus::Active;
            bot.vm == VmState::Missing
                || (active && bot.vm == VmState::Omitted)
                || (active && !matches!(bot.project, ProjectState::Included | ProjectState::None))
                || bot.computer.as_ref().is_some_and(|c| {
                    c.desktop_port.is_some() && c.browser_checkpoint.as_deref() != Some("ok")
                })
        };
        let whole = coordinated && mcp && missing_files == 0 && !bots.iter().any(lacking);
        let projects = if bots.iter().any(|b| b.project == ProjectState::Partial) {
            Covered::Partial
        } else if bots.iter().any(|b| {
            !matches!(
                b.project,
                ProjectState::Omitted | ProjectState::OmittedDeletedBot
            )
        }) {
            Covered::Included
        } else {
            Covered::Omitted
        };
        let mcp = if mcp {
            Covered::Included
        } else {
            Covered::Omitted
        };
        if coordinated {
            return Self {
                database: Covered::Included,
                vm: if whole {
                    Covered::Included
                } else {
                    Covered::Partial
                },
                projects,
                mcp,
                provider_state: ProviderState::OmittedReauthRequired,
                whole_assistant: whole,
                note: if whole {
                    "Whole assistant: the database, every active bot's computer (disk archives; \
                     each image is stored once, in the first archive that uses it, which a restore \
                     imports first), every bot's project directory (without node_modules, which a \
                     reinstall restores), and the shared MCP skills, documentation and config. \
                     Native provider sessions are not included: conversations are rebuilt from \
                     SQLite and providers may need a fresh login."
                        .into()
                } else {
                    format!(
                        "Partial: a bot's computer is missing from the runtime, a desktop browser was \
                         not checkpointed, a project directory was not captured whole, or host \
                         artifacts are missing ({missing_files}), or the shared MCP library was not captured, \
                         so this set does not recover the whole assistant."
                    )
                },
            };
        }
        let note = match (projects, mcp) {
            (_, Covered::Included) => format!(
                "The database{} and shared MCP skills, documentation and config are included. \
                 Bot computers and native provider sessions are not included. Restoring this set \
                 does not recover the whole assistant.",
                if projects == Covered::Omitted {
                    ""
                } else {
                    ", bot project directories"
                }
            ),
            (Covered::Omitted, _) => super::database_only_coverage().note,
            _ => "The database and the bots' project directories only: bot computers \
                  (files, browser profiles, running jobs) and native provider sessions are not \
                  included. Restoring this set does not recover the whole assistant."
                .into(),
        };
        Self {
            projects,
            mcp,
            note,
            ..super::database_only_coverage()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot(status: BotStatus, vm: VmState, project: ProjectState) -> BotEntry {
        BotEntry {
            id: Uuid::new_v4(),
            name: "Bot".into(),
            status,
            vm,
            computer: None,
            provider_state: ProviderState::OmittedReauthRequired,
            provider_sessions: Vec::new(),
            project,
        }
    }

    #[test]
    fn a_coordinated_set_is_whole_only_when_nothing_is_missing() {
        let active = bot(BotStatus::Active, VmState::Included, ProjectState::Included);
        let deleted = bot(
            BotStatus::Deleted,
            VmState::OmittedDeletedBot,
            ProjectState::OmittedDeletedBot,
        );
        let whole = Coverage::derive(true, &[active.clone(), deleted.clone()], 0, true);
        assert!(whole.whole_assistant);
        assert_eq!(
            (whole.vm, whole.projects, whole.mcp),
            (Covered::Included, Covered::Included, Covered::Included)
        );
        for (bots, missing_files, mcp, why) in [
            (
                vec![bot(
                    BotStatus::Active,
                    VmState::Omitted,
                    ProjectState::Included,
                )],
                0,
                true,
                "an active bot's computer was not captured",
            ),
            (
                vec![bot(
                    BotStatus::Active,
                    VmState::Missing,
                    ProjectState::Included,
                )],
                0,
                true,
                "a computer is gone from the runtime",
            ),
            (
                vec![bot(
                    BotStatus::Active,
                    VmState::Included,
                    ProjectState::Partial,
                )],
                0,
                true,
                "a project directory was read in part",
            ),
            (vec![active.clone()], 1, true, "a host artifact is missing"),
            (
                vec![active.clone()],
                0,
                false,
                "the shared MCPs are missing",
            ),
        ] {
            let partial = Coverage::derive(true, &bots, missing_files, mcp);
            assert!(!partial.whole_assistant, "{why}");
            assert_eq!(partial.vm, Covered::Partial, "{why}");
        }
        assert!(
            !Coverage::derive(false, &[active], 0, true).whole_assistant,
            "an uncoordinated set never recovers the whole assistant"
        );
    }

    #[test]
    fn an_uncoordinated_set_names_what_it_holds() {
        let with_project = bot(BotStatus::Active, VmState::None, ProjectState::Included);
        let without = bot(BotStatus::Active, VmState::None, ProjectState::Omitted);
        assert_eq!(
            Coverage::derive(false, std::slice::from_ref(&without), 0, false),
            super::super::database_only_coverage()
        );
        let projects = Coverage::derive(false, std::slice::from_ref(&with_project), 0, false);
        assert_eq!(
            (projects.vm, projects.projects, projects.mcp),
            (Covered::Omitted, Covered::Included, Covered::Omitted)
        );
        assert!(
            projects
                .note
                .starts_with("The database and the bots' project directories only")
        );
        let mcp = Coverage::derive(false, &[with_project], 0, true);
        assert_eq!(mcp.mcp, Covered::Included);
        assert!(
            mcp.note
                .starts_with("The database, bot project directories and shared MCP skills")
        );
        let mcp_only = Coverage::derive(false, &[without], 0, true);
        assert!(
            mcp_only
                .note
                .starts_with("The database and shared MCP skills")
        );
    }
}
