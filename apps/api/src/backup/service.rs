use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::capture::{BackupConfig, Coordinator};
use super::replicate::Replicator;
use super::s3::S3;
use super::settings::{
    self, Destination, DestinationInput, DestinationView, Error, Settings, Update,
};
use super::worker::BackupWorker;
use super::{BackupError, DatabaseParams};
use crate::config::Config;
use crate::credentials::SecretKey;

#[derive(Clone)]
pub struct Backups {
    tx: mpsc::Sender<Command>,
    pool: SqlitePool,
    initial_estimate: Estimate,
    probe_slots: Arc<tokio::sync::Semaphore>,
}

#[derive(Clone, Serialize)]
pub struct Estimate {
    pub bytes: u64,
    pub basis: String,
}

#[derive(Serialize)]
pub struct Recent {
    pub set_id: String,
    pub finished_at: Option<String>,
    pub bytes: Option<u64>,
    pub local_status: String,
    pub remote_status: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub revision: i64,
    pub enabled: bool,
    pub interval_secs: u64,
    pub keep: usize,
    pub destination: Option<DestinationView>,
    pub running: bool,
    pub next_backup_at: Option<String>,
    pub estimate: Option<Estimate>,
    pub recent: Vec<Recent>,
    pub remote_count: usize,
    pub remote_bytes: u64,
    pub unavailable_reason: Option<String>,
    pub last_error: Option<String>,
}

struct RuntimeView {
    settings: Settings,
    running: bool,
    next_backup_at: Option<String>,
    last_error: Option<String>,
}

enum Command {
    Read(oneshot::Sender<RuntimeView>),
    Save(Update, oneshot::Sender<Result<(), Error>>),
    Resolve(
        i64,
        DestinationInput,
        oneshot::Sender<Result<Destination, Error>>,
    ),
    Run(oneshot::Sender<Result<(), Error>>),
}

struct Engine {
    pool: SqlitePool,
    params: DatabaseParams,
    key: Arc<SecretKey>,
    config: BackupConfig,
    coordinator: Coordinator,
}

#[derive(Clone, Copy)]
enum Job {
    Local,
    Remote,
    Retry,
}

enum Failure {
    Capture(String),
    Upload,
}

impl Failure {
    fn capture(error: BackupError) -> Self {
        Self::Capture(match error {
            BackupError::Remote(_) => {
                "Remote storage could not be checked while preserving pending backups.".into()
            }
            _ => error.to_string(),
        })
    }

    fn message(&self) -> String {
        match self {
            Self::Capture(message) => message.clone(),
            Self::Upload => "Remote upload failed. Check the endpoint, bucket permissions, and credentials. The local backup is kept.".into(),
        }
    }
}

impl Engine {
    fn worker(&self, settings: &Settings, with_remote: bool) -> Result<BackupWorker, Error> {
        let mut worker = BackupWorker::new(
            self.pool.clone(),
            self.params.clone(),
            self.key.clone(),
            self.config.clone(),
        )
        .with_coordinator(self.coordinator.clone());
        if with_remote && let Some(destination) = &settings.destination {
            let s3 = S3::new(destination.s3_config())
                .map_err(|_| Error::Invalid("The saved backup destination is invalid."))?;
            worker = worker.with_replicator(Replicator::new(s3, self.pool.clone(), settings.keep));
        }
        Ok(worker)
    }

    fn spawn(
        &self,
        settings: &Settings,
        kind: Job,
        cancel: CancellationToken,
    ) -> Result<JoinHandle<Result<(), Failure>>, Error> {
        let automatic = settings.enabled && settings.interval_secs > 0;
        let worker = self.worker(settings, !matches!(kind, Job::Local) || automatic)?;
        Ok(tokio::spawn(async move {
            match kind {
                Job::Retry => worker
                    .replicate_now()
                    .await
                    .map(|_| ())
                    .map_err(|_| Failure::Upload),
                Job::Local => worker
                    .run_local_until(&cancel)
                    .await
                    .map(|_| ())
                    .map_err(Failure::capture),
                Job::Remote => {
                    let summary = worker
                        .run_once_until(&cancel)
                        .await
                        .map_err(Failure::capture)?;
                    match summary.remote {
                        Some(Err(_)) => Err(Failure::Upload),
                        _ => Ok(()),
                    }
                }
            }
        }))
    }

    fn next_local(&self) -> Option<Instant> {
        if self.config.interval.is_zero() {
            return None;
        }
        let worker = BackupWorker::new(
            self.pool.clone(),
            self.params.clone(),
            self.key.clone(),
            self.config.clone(),
        );
        Some(Instant::now() + worker.next_due().unwrap_or(Duration::from_secs(60)))
    }

    async fn next_remote(&self, settings: &Settings) -> Result<Option<Instant>, Error> {
        if !settings.enabled || settings.interval_secs == 0 {
            return Ok(None);
        }
        let Some(destination) = &settings.destination else {
            return Ok(None);
        };
        let s3 = S3::new(destination.s3_config())
            .map_err(|_| Error::Invalid("The saved destination is invalid."))?;
        let last: Option<String> = sqlx::query_scalar("SELECT MAX(verified_at) FROM backup_remote WHERE destination = ?1 AND state = 'completed'")
            .bind(s3.destination()).fetch_one(&self.pool).await?;
        let delay = last
            .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
            .map(|at| {
                Duration::from_secs(settings.interval_secs).saturating_sub(
                    (Utc::now() - at.with_timezone(&Utc))
                        .to_std()
                        .unwrap_or_default(),
                )
            })
            .unwrap_or_default();
        Ok(Some(Instant::now() + delay))
    }
}

fn wall_time(at: Instant) -> String {
    (Utc::now()
        + chrono::Duration::from_std(at.saturating_duration_since(Instant::now()))
            .unwrap_or_default())
    .to_rfc3339()
}

impl Backups {
    pub async fn start(
        config: &Config,
        pool: SqlitePool,
        key: Arc<SecretKey>,
        coordinator: Coordinator,
        cancel: CancellationToken,
    ) -> Result<(Self, JoinHandle<()>), Error> {
        let settings = settings::load_or_seed(&pool, &key, config).await?;
        let params = DatabaseParams::parse(
            config
                .require_database()
                .map_err(|_| Error::Unavailable)?
                .expose(),
        )
        .map_err(|_| Error::Unavailable)?;
        let (pages, page_size) = tokio::join!(
            sqlx::query_scalar::<_, i64>("PRAGMA page_count").fetch_one(&pool),
            sqlx::query_scalar::<_, i64>("PRAGMA page_size").fetch_one(&pool),
        );
        let database_bytes = (pages?.max(0) as u64).saturating_mul(page_size?.max(0) as u64);
        let projects = config.bots_dir.clone();
        let artifacts = config.computers.state_dir.join("artifacts");
        let mcp = config.data_dir.join("mcp");
        let initial_bytes = tokio::task::spawn_blocking(move || {
            database_bytes
                .saturating_add(size(&projects))
                .saturating_add(size(&artifacts))
                .saturating_add(size(&mcp))
        })
        .await
        .map_err(|_| Error::Unavailable)?;
        let engine = Engine {
            pool: pool.clone(),
            params,
            key,
            config: config.backup.clone(),
            coordinator,
        };
        if engine.worker(&settings, false)?.recover().await.is_err() {
            tracing::warn!("backup recovery failed; the next backup will retry");
        }
        let local_at = engine.next_local();
        let remote_at = engine.next_remote(&settings).await?;
        let (tx, rx) = mpsc::channel(32);
        let service = Self {
            tx,
            pool,
            probe_slots: Arc::new(tokio::sync::Semaphore::new(1)),
            initial_estimate: Estimate {
                bytes: initial_bytes,
                basis: "Rough database, file, project and shared MCP size. Computer snapshots and encryption overhead are excluded until the first complete backup.".into(),
            },
        };
        let task = tokio::spawn(run(engine, settings, rx, local_at, remote_at, cancel));
        Ok((service, task))
    }

    async fn request<R>(
        &self,
        make: impl FnOnce(oneshot::Sender<R>) -> Command,
    ) -> Result<R, Error> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(make(tx))
            .await
            .map_err(|_| Error::Unavailable)?;
        rx.await.map_err(|_| Error::Unavailable)
    }

    pub async fn save(&self, update: Update) -> Result<Snapshot, Error> {
        self.request(|tx| Command::Save(update, tx)).await??;
        self.snapshot().await
    }

    pub async fn run_now(&self) -> Result<(), Error> {
        self.request(Command::Run).await?
    }

    pub async fn test(&self, revision: i64, input: DestinationInput) -> Result<(), Error> {
        let destination = self
            .request(|tx| Command::Resolve(revision, input, tx))
            .await??;
        let permit = self
            .probe_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Unavailable)?;
        tokio::spawn(async move {
            let _permit = permit;
            let s3 = S3::new(destination.s3_config()).map_err(|_| Error::Invalid("The endpoint is invalid."))?;
            let key = s3.key(&format!(".connection-check/{}", uuid::Uuid::new_v4()));
            let payload = b"else.bot backup connection check".to_vec();
            let mut created = false;
            let check = async {
                s3.list(&s3.key(".connection-check/")).await?;
                s3.list_multipart(&s3.key(".connection-check/")).await?;
                if s3.put(&key, payload.clone(), true).await? != super::s3::Put::Created {
                    return Err(super::s3::S3Error::Protocol("probe key already exists".into()));
                }
                created = true;
                let bytes = s3.get_bytes(&key, 256).await?;
                if bytes != payload { return Err(super::s3::S3Error::Protocol("probe readback differs".into())); }
                Ok(())
            };
            let result = tokio::time::timeout(Duration::from_secs(30), check).await;
            let cleanup = if created { Some(tokio::time::timeout(Duration::from_secs(15), s3.delete(&key)).await) } else { None };
            if !matches!(result, Ok(Ok(()))) || !matches!(cleanup, Some(Ok(Ok(())))) {
                return Err(Error::Invalid("Connection check failed. Verify the endpoint, keys, and bucket list, read, write, and delete permissions."));
            }
            Ok(())
        }).await.map_err(|_| Error::Unavailable)?
    }

    pub async fn snapshot(&self) -> Result<Snapshot, Error> {
        let view = self.request(Command::Read).await?;
        let destination_id = view
            .settings
            .destination
            .as_ref()
            .map(|d| S3::new(d.s3_config()).map(|s| s.destination()))
            .transpose()
            .map_err(|_| Error::Unavailable)?;
        let history = async {
            sqlx::query("SELECT r.set_id, COALESCE(r.verified_at, b.finished_at) AS finished_at, b.ciphertext_bytes, COALESCE(b.local_status, 'completed') AS local_status, r.state AS remote_status, r.error FROM backup_remote r LEFT JOIN backup_runs b ON b.set_id = r.set_id WHERE r.destination = ?1 ORDER BY COALESCE(r.verified_at, b.finished_at, r.updated_at) DESC, r.set_id DESC LIMIT 10")
                .bind(&destination_id).fetch_all(&self.pool).await
        };
        let totals = async {
            sqlx::query("SELECT COUNT(*) AS count, COALESCE(SUM(b.ciphertext_bytes), 0) AS bytes FROM backup_remote r LEFT JOIN backup_runs b ON b.set_id = r.set_id WHERE r.destination = ?1 AND r.state = 'completed'")
                .bind(&destination_id).fetch_one(&self.pool).await
        };
        let latest = async {
            sqlx::query_scalar::<_, i64>("SELECT ciphertext_bytes FROM backup_runs WHERE local_status = 'completed' AND ciphertext_bytes IS NOT NULL ORDER BY finished_at DESC LIMIT 1")
                .fetch_optional(&self.pool).await
        };
        let (history, totals, latest) = tokio::join!(history, totals, latest);
        let recent = history?
            .into_iter()
            .map(|row| Recent {
                set_id: row.get("set_id"),
                finished_at: row.get("finished_at"),
                bytes: row
                    .get::<Option<i64>, _>("ciphertext_bytes")
                    .map(|n| n.max(0) as u64),
                local_status: row.get("local_status"),
                remote_status: row.get("remote_status"),
                error: row
                    .get::<Option<String>, _>("error")
                    .map(|_| "Remote upload failed. Check the destination and credentials.".into()),
            })
            .collect();
        let totals = totals?;
        let estimate = latest?.map(|bytes| Estimate { bytes: bytes.max(0) as u64, basis: "Based on the latest complete encrypted backup. The next backup may differ in size.".into() })
            .unwrap_or_else(|| self.initial_estimate.clone());
        let settings = view.settings;
        Ok(Snapshot {
            revision: settings.revision,
            enabled: settings.enabled,
            interval_secs: settings.interval_secs,
            keep: settings.keep,
            destination: settings.destination.map(|d| d.view),
            running: view.running,
            next_backup_at: view.next_backup_at,
            estimate: Some(estimate),
            recent,
            remote_count: totals.get::<i64, _>("count") as usize,
            remote_bytes: totals.get::<i64, _>("bytes").max(0) as u64,
            unavailable_reason: None,
            last_error: view.last_error,
        })
    }
}

fn size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_symlink() {
        return 0;
    }
    if meta.is_file() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .fold(0u64, |sum, entry| sum.saturating_add(size(&entry.path())))
}

async fn run(
    engine: Engine,
    mut settings: Settings,
    mut rx: mpsc::Receiver<Command>,
    mut local_at: Option<Instant>,
    mut remote_at: Option<Instant>,
    cancel: CancellationToken,
) {
    let mut job: Option<JoinHandle<Result<(), Failure>>> = None;
    let mut kind = Job::Local;
    let mut retry_at = None;
    let mut local_retry = Duration::from_secs(60);
    let mut remote_retry = Duration::from_secs(60);
    let mut local_error = None;
    let mut remote_error = None;
    if settings.enabled
        && settings.interval_secs > 0
        && let Ok(worker) = engine.worker(&settings, true)
        && let Some(replicator) = worker.replicator()
        && replicator
            .awaiting(&engine.config.dir)
            .await
            .is_ok_and(|v| !v.is_empty())
    {
        retry_at = Some(Instant::now());
    }
    loop {
        let wake = if job.is_some() {
            None
        } else {
            [local_at, remote_at, retry_at].into_iter().flatten().min()
        };
        tokio::select! {
            _ = cancel.cancelled() => {
                if let Some(mut task) = job
                    && tokio::time::timeout(Duration::from_secs(30), &mut task).await.is_err()
                {
                    task.abort();
                    let _ = task.await;
                }
                return;
            },
            Some(command) = rx.recv() => match command {
                Command::Read(reply) => {
                    let _ = reply.send(RuntimeView {
                        settings: settings.clone(),
                        running: job.is_some(),
                        next_backup_at: [remote_at, retry_at].into_iter().flatten().min().map(wall_time),
                        last_error: remote_error.clone().or_else(|| local_error.clone()),
                    });
                }
                Command::Resolve(revision, input, reply) => {
                    let result = if revision != settings.revision {
                        Err(Error::Conflict)
                    } else {
                        Destination::resolve(input, settings.destination.as_ref())
                    };
                    let _ = reply.send(result);
                }
                Command::Save(update, reply) => {
                    let result = async {
                        if job.is_some() { return Err(Error::Busy); }
                        let next = settings.updated(update)?;
                        settings::save(&engine.pool, &engine.key, &next).await?;
                        settings = next;
                        remote_at = engine.next_remote(&settings).await?;
                        retry_at = None;
                        remote_error = None;
                        Ok(())
                    }.await;
                    let _ = reply.send(result);
                }
                Command::Run(reply) => {
                    let result = if job.is_some() {
                        Err(Error::Busy)
                    } else if settings.destination.is_none() {
                        Err(Error::Invalid("Configure a backup destination first."))
                    } else {
                        engine.spawn(&settings, Job::Remote, cancel.child_token()).map(|task| {
                            kind = Job::Remote;
                            job = Some(task);
                        })
                    };
                    let _ = reply.send(result);
                }
            },
            outcome = async {
                match job.as_mut() {
                    Some(task) => task.await,
                    None => std::future::pending().await,
                }
            } => {
                job = None;
                let result = outcome.unwrap_or_else(|_| Err(Failure::Capture("Backup task stopped unexpectedly.".into())));
                local_at = engine.next_local();
                match result {
                    Ok(()) => {
                        match kind {
                            Job::Local => {
                                local_error = None;
                                local_retry = Duration::from_secs(60);
                            }
                            Job::Remote => {
                                local_error = None;
                                remote_error = None;
                                local_retry = Duration::from_secs(60);
                            }
                            Job::Retry => remote_error = None,
                        }
                        if !matches!(kind, Job::Local) {
                            retry_at = None;
                            remote_retry = Duration::from_secs(60);
                            remote_at = engine.next_remote(&settings).await.unwrap_or(None);
                            if remote_at.is_some_and(|at| at <= Instant::now()) {
                                remote_at = Some(Instant::now() + Duration::from_secs(settings.interval_secs.max(60)));
                            }
                        }
                    }
                    Err(error) => {
                        if matches!(kind, Job::Local) {
                            local_error = Some(error.message());
                            local_at = Some(Instant::now() + local_retry);
                            local_retry = (local_retry * 2).min(Duration::from_secs(3600));
                        } else {
                            remote_error = Some(error.message());
                        }
                        if !matches!(kind, Job::Local) && settings.enabled && settings.interval_secs > 0 {
                            match error {
                                Failure::Capture(_) => {
                                    remote_at = Some(Instant::now() + remote_retry);
                                    retry_at = None;
                                }
                                Failure::Upload => {
                                    retry_at = Some(Instant::now() + remote_retry);
                                    remote_at = Some(Instant::now() + Duration::from_secs(settings.interval_secs));
                                }
                            }
                        }
                        if !matches!(kind, Job::Local) {
                            remote_retry = (remote_retry * 2).min(Duration::from_secs(3600));
                        }
                    }
                }
            },
            _ = async {
                match wake {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                let now = Instant::now();
                kind = if remote_at.is_some_and(|at| at <= now) {
                    Job::Remote
                } else if local_at.is_some_and(|at| at <= now) {
                    Job::Local
                } else {
                    Job::Retry
                };
                match engine.spawn(&settings, kind, cancel.child_token()) {
                    Ok(task) => job = Some(task),
                    Err(error) => {
                        remote_error = Some(error.to_string());
                        remote_at = None;
                        retry_at = None;
                        local_at = Some(now + Duration::from_secs(60));
                    }
                }
            }
        }
    }
}
