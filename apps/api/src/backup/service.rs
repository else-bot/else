use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{Row, SqlitePool};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::capture::{BackupConfig, Cadence, Coordinator};
use super::replicate::Replicator;
use super::s3::S3;
use super::settings::{
    self, Destination, DestinationInput, DestinationView, Error, LocalSchedule, LocalUpdate,
    Settings, Update,
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
    local_dir: std::path::PathBuf,
    cadence: Cadence,
    /// `ELSE_BACKUP_KEEP`, which applies while the environment sets the
    /// schedule.
    env_keep: usize,
    gate: Option<Arc<crate::maintenance::Gate>>,
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
    /// The encrypted size of the whole set.
    pub bytes: Option<u64>,
    /// What its remote run wrote to the bucket: only the chunks the bucket
    /// lacked. Absent for sets replicated before incremental uploads.
    pub uploaded_bytes: Option<u64>,
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
    /// Bytes stored under the destination's prefix after the last
    /// retention, or the sum of the kept sets' sizes before one was measured.
    pub remote_bytes: u64,
    pub unavailable_reason: Option<String>,
    pub last_error: Option<String>,
    pub local: LocalView,
    /// Why new work is paused right now, if it is.
    pub pause: Option<Value>,
}

/// The daily local backup on this server.
#[derive(Serialize)]
pub struct LocalView {
    pub enabled: bool,
    /// `HH:MM` on the server's clock.
    pub time: String,
    pub timezone: String,
    pub utc_offset_secs: i32,
    pub keep: usize,
    /// An environment variable sets the schedule; the owner cannot change it.
    pub managed_by_env: bool,
    pub next_at: Option<String>,
    pub copies: usize,
    pub bytes: u64,
    pub free_bytes: Option<u64>,
    pub recent: Vec<LocalRun>,
}

#[derive(Serialize)]
pub struct LocalRun {
    pub set_id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: &'static str,
    pub bytes: Option<u64>,
    pub took_secs: Option<u64>,
    pub paused_secs: Option<u64>,
    pub error: Option<String>,
}

struct RuntimeView {
    settings: Settings,
    running: bool,
    next_backup_at: Option<String>,
    next_local_at: Option<String>,
    last_error: Option<String>,
}

enum Command {
    Read(oneshot::Sender<RuntimeView>),
    Save(Update, oneshot::Sender<Result<(), Error>>),
    SaveLocal(LocalUpdate, oneshot::Sender<Result<(), Error>>),
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
    /// A refusal the owner can act on, such as an expiring lifecycle rule.
    Upload(Option<String>),
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

    fn upload(error: &BackupError) -> Self {
        Self::Upload(match error {
            BackupError::Refused(message) => Some(message.clone()),
            _ => None,
        })
    }

    fn message(&self) -> String {
        match self {
            Self::Capture(message) => message.clone(),
            Self::Upload(Some(message)) => format!("Remote upload refused. {message} The local backup is kept."),
            Self::Upload(None) => "Remote upload failed. Check the endpoint, bucket permissions, and credentials. The local backup is kept.".into(),
        }
    }
}

impl Engine {
    /// The backup config with the owner's local retention, unless the
    /// environment sets the schedule.
    fn config(&self, settings: &Settings) -> BackupConfig {
        let mut config = self.config.clone();
        if config.cadence == Cadence::Daily {
            config.keep = settings.local.keep;
        }
        config
    }

    fn worker(&self, settings: &Settings, with_remote: bool) -> Result<BackupWorker, Error> {
        let mut worker = BackupWorker::new(
            self.pool.clone(),
            self.params.clone(),
            self.key.clone(),
            self.config(settings),
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
                    .map_err(|e| Failure::upload(&e)),
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
                        Some(Err(error)) => Err(Failure::upload(&error)),
                        _ => Ok(()),
                    }
                }
            }
        }))
    }

    /// When the next local set is due. A daily set that was missed, for
    /// example while the server was off, waits for the next chosen time.
    fn next_local(&self, settings: &Settings) -> Option<Instant> {
        match self.config.cadence {
            Cadence::Off => None,
            Cadence::Every(interval) => {
                let worker = BackupWorker::new(
                    self.pool.clone(),
                    self.params.clone(),
                    self.key.clone(),
                    self.config(settings),
                );
                Some(Instant::now() + worker.next_due(interval).unwrap_or(Duration::from_secs(60)))
            }
            Cadence::Daily => settings
                .local
                .enabled
                .then(|| instant(next_daily(settings.local, chrono::Local::now()))),
        }
    }

    /// When to retry a failed local set: after `backoff`, but a daily set
    /// gives up a few hours after its time and waits for the next one, so
    /// it never pauses the owner's day.
    fn retry_local(&self, settings: &Settings, backoff: Duration) -> Option<Instant> {
        let retry = Instant::now() + backoff;
        if self.config.cadence != Cadence::Daily {
            return Some(retry);
        }
        let now = chrono::Local::now();
        let slot = next_daily(settings.local, now) - chrono::Duration::days(1);
        let retry_at = now + chrono::Duration::from_std(backoff).unwrap_or_default();
        if retry_at - slot <= daily_retry_window() {
            Some(retry)
        } else {
            self.next_local(settings)
        }
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

/// A daily set may still retry this long after its chosen time.
fn daily_retry_window() -> chrono::Duration {
    chrono::Duration::hours(3)
}

/// The first moment after `now` when the clock in `now`'s zone shows the
/// schedule's time. A time the clock skips (a daylight-saving jump) runs
/// an hour later; a time it shows twice runs the first time.
fn next_daily<Tz: chrono::TimeZone>(schedule: LocalSchedule, now: DateTime<Tz>) -> DateTime<Tz> {
    use chrono::offset::LocalResult;
    let zone = now.timezone();
    let mut day = now.date_naive();
    loop {
        let naive = day.and_time(schedule.at.time());
        let at = match zone.from_local_datetime(&naive) {
            LocalResult::Single(at) | LocalResult::Ambiguous(at, _) => Some(at),
            LocalResult::None => zone
                .from_local_datetime(&(naive + chrono::Duration::hours(1)))
                .earliest(),
        };
        if let Some(at) = at
            && at > now
        {
            return at;
        }
        day = day.succ_opt().expect("the calendar has a next day");
    }
}

fn instant(at: DateTime<chrono::Local>) -> Instant {
    Instant::now() + (at - chrono::Local::now()).to_std().unwrap_or_default()
}

/// The wall-clock time of a deadline, to the nearest second: reading the
/// two clocks apart would otherwise show 03:00 as 02:59:59.999.
fn wall_time(at: Instant) -> String {
    let at = Utc::now()
        + chrono::Duration::from_std(at.saturating_duration_since(Instant::now()))
            .unwrap_or_default();
    chrono::DurationRound::duration_round(at, chrono::Duration::seconds(1))
        .unwrap_or(at)
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
        let local_at = engine.next_local(&settings);
        let remote_at = engine.next_remote(&settings).await?;
        let (tx, rx) = mpsc::channel(32);
        let service = Self {
            tx,
            pool,
            probe_slots: Arc::new(tokio::sync::Semaphore::new(1)),
            local_dir: engine.config.dir.clone(),
            cadence: engine.config.cadence,
            env_keep: engine.config.keep,
            gate: engine
                .coordinator
                .scheduler
                .as_ref()
                .map(|scheduler| scheduler.gate().clone()),
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

    pub async fn save_local(&self, update: LocalUpdate) -> Result<Snapshot, Error> {
        self.request(|tx| Command::SaveLocal(update, tx)).await??;
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
            let rules = tokio::time::timeout(Duration::from_secs(15), s3.expiring_rule(&s3.key(super::chunks::CHUNKS))).await;
            if let Ok(Ok(Some(rule))) = rules {
                return Err(Error::ExpiringRule(rule));
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
            sqlx::query("SELECT r.set_id, COALESCE(r.verified_at, b.finished_at) AS finished_at, b.ciphertext_bytes, r.uploaded_bytes, COALESCE(b.local_status, 'completed') AS local_status, r.state AS remote_status, r.error FROM backup_remote r LEFT JOIN backup_runs b ON b.set_id = r.set_id WHERE r.destination = ?1 ORDER BY COALESCE(r.verified_at, b.finished_at, r.updated_at) DESC, r.set_id DESC LIMIT 10")
                .bind(&destination_id).fetch_all(&self.pool).await
        };
        let totals = async {
            sqlx::query("SELECT COUNT(*) AS count, COALESCE((SELECT stored_bytes FROM backup_bucket_usage WHERE destination = ?1), SUM(b.ciphertext_bytes), 0) AS bytes FROM backup_remote r LEFT JOIN backup_runs b ON b.set_id = r.set_id WHERE r.destination = ?1 AND r.state = 'completed'")
                .bind(&destination_id).fetch_one(&self.pool).await
        };
        let latest = async {
            sqlx::query_scalar::<_, i64>("SELECT ciphertext_bytes FROM backup_runs WHERE local_status = 'completed' AND ciphertext_bytes IS NOT NULL ORDER BY coalesce(json_extract(coverage, '$.whole_assistant'), 0) DESC, finished_at DESC LIMIT 1")
                .fetch_optional(&self.pool).await
        };
        let local_runs = async {
            sqlx::query("SELECT set_id, started_at, finished_at, local_status, ciphertext_bytes, paused_ms, error,
                    coalesce(json_extract(coverage, '$.whole_assistant'), 1) AS whole,
                    CAST(round((julianday(finished_at) - julianday(started_at)) * 86400) AS INTEGER) AS took
                 FROM backup_runs ORDER BY started_at DESC LIMIT 10")
                .fetch_all(&self.pool).await
        };
        let dir = self.local_dir.clone();
        let disk = tokio::task::spawn_blocking(move || {
            let sets = super::completed_sets(&dir).unwrap_or_default();
            let bytes = sets
                .iter()
                .fold(0u64, |sum, set| sum.saturating_add(size(&dir.join(set))));
            (sets.len(), bytes, free_space(&dir))
        });
        let (history, totals, latest, local_runs, disk) =
            tokio::join!(history, totals, latest, local_runs, disk);
        let (copies, local_bytes, free_bytes) = disk.map_err(|_| Error::Unavailable)?;
        let local_recent = local_runs?
            .into_iter()
            .map(|row| {
                let status = match (
                    row.get::<String, _>("local_status").as_str(),
                    row.get::<i64, _>("whole"),
                ) {
                    ("running", _) => "running",
                    ("completed", 0) => "partial",
                    ("completed", _) => "completed",
                    _ => "failed",
                };
                LocalRun {
                    set_id: row.get("set_id"),
                    started_at: row.get("started_at"),
                    finished_at: row.get("finished_at"),
                    status,
                    bytes: (status != "failed")
                        .then(|| row.get::<Option<i64>, _>("ciphertext_bytes"))
                        .flatten()
                        .map(|n| n.max(0) as u64),
                    took_secs: row.get::<Option<i64>, _>("took").map(|n| n.max(0) as u64),
                    paused_secs: row
                        .get::<Option<i64>, _>("paused_ms")
                        .map(|ms| (ms.max(0) as u64 + 500) / 1000),
                    error: (status == "failed")
                        .then(|| row.get::<Option<String>, _>("error"))
                        .flatten(),
                }
            })
            .collect();
        let recent = history?
            .into_iter()
            .map(|row| Recent {
                set_id: row.get("set_id"),
                finished_at: row.get("finished_at"),
                bytes: row
                    .get::<Option<i64>, _>("ciphertext_bytes")
                    .map(|n| n.max(0) as u64),
                uploaded_bytes: row
                    .get::<Option<i64>, _>("uploaded_bytes")
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
            local: LocalView {
                enabled: self.cadence != Cadence::Off && settings.local.enabled,
                time: match self.cadence {
                    Cadence::Daily | Cadence::Off => settings.local.at.to_string(),
                    Cadence::Every(interval) => {
                        format!("every {} min", interval.as_secs().div_ceil(60))
                    }
                },
                timezone: server_timezone(),
                utc_offset_secs: chrono::Local::now().offset().local_minus_utc(),
                keep: match self.cadence {
                    Cadence::Daily => settings.local.keep,
                    _ => self.env_keep,
                },
                managed_by_env: self.cadence != Cadence::Daily,
                next_at: view.next_local_at,
                copies,
                bytes: local_bytes,
                free_bytes,
                recent: local_recent,
            },
            pause: self
                .gate
                .as_ref()
                .and_then(|gate| gate.pause())
                .map(|pause| pause.json()),
        })
    }
}

/// The server's IANA time zone, or its UTC offset when that is unknown.
fn server_timezone() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| {
        let offset = chrono::Local::now().offset().local_minus_utc();
        format!(
            "UTC{}{:02}:{:02}",
            if offset < 0 { '-' } else { '+' },
            offset.abs() / 3600,
            offset.abs() % 3600 / 60
        )
    })
}

/// Bytes available to this user on the file system holding `path`.
fn free_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stat` is written by statvfs
    // before it is read, only when the call succeeds.
    let stat = unsafe {
        if libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) != 0 {
            return None;
        }
        stat.assume_init()
    };
    // The field types are u64 on Linux and narrower on macOS.
    #[allow(
        clippy::unnecessary_cast,
        reason = "statvfs field widths differ by platform"
    )]
    let (available, fragment) = (stat.f_bavail as u64, stat.f_frsize as u64);
    Some(available.saturating_mul(fragment))
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
                        next_local_at: local_at.map(wall_time),
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
                Command::SaveLocal(update, reply) => {
                    let result = async {
                        if engine.config.cadence != Cadence::Daily {
                            return Err(Error::Invalid(
                                "ELSE_BACKUP_INTERVAL_SECS sets the local backup schedule on this server.",
                            ));
                        }
                        let next = settings.updated_local(update)?;
                        settings::save(&engine.pool, &engine.key, &next).await?;
                        settings = next;
                        if job.is_none() {
                            local_at = engine.next_local(&settings);
                        }
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
                local_at = engine.next_local(&settings);
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
                            local_at = engine.retry_local(&settings, local_retry);
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
                                Failure::Upload(_) => {
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
                        local_at = engine.retry_local(&settings, Duration::from_secs(60));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;
    use chrono_tz::{America::New_York, Asia::Dubai};

    use super::*;
    use crate::backup::settings::TimeOfDay;

    fn at(time: &str) -> LocalSchedule {
        LocalSchedule {
            enabled: true,
            at: TimeOfDay::parse(time).unwrap(),
            keep: 7,
        }
    }

    #[test]
    fn the_daily_time_is_the_next_one_on_the_servers_clock() {
        let morning = Dubai.with_ymd_and_hms(2026, 11, 12, 10, 40, 0).unwrap();
        assert_eq!(
            next_daily(at("03:00"), morning),
            Dubai.with_ymd_and_hms(2026, 11, 13, 3, 0, 0).unwrap(),
            "a time already past today runs tomorrow, not now"
        );
        let night = Dubai.with_ymd_and_hms(2026, 11, 12, 1, 0, 0).unwrap();
        assert_eq!(
            next_daily(at("03:00"), night),
            Dubai.with_ymd_and_hms(2026, 11, 12, 3, 0, 0).unwrap()
        );
        let exactly = Dubai.with_ymd_and_hms(2026, 11, 12, 3, 0, 0).unwrap();
        assert_eq!(
            next_daily(at("03:00"), exactly),
            Dubai.with_ymd_and_hms(2026, 11, 13, 3, 0, 0).unwrap(),
            "the set that just ran is not repeated"
        );
    }

    #[test]
    fn daylight_saving_days_still_run_once() {
        // 2026-03-08 02:00 does not exist in New York; 2026-11-01 01:30 happens twice.
        let before_jump = New_York.with_ymd_and_hms(2026, 3, 8, 0, 0, 0).unwrap();
        assert_eq!(
            next_daily(at("02:30"), before_jump),
            New_York.with_ymd_and_hms(2026, 3, 8, 3, 30, 0).unwrap()
        );
        let before_repeat = New_York.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap();
        let first = next_daily(at("01:30"), before_repeat);
        assert_eq!(first.offset().to_string(), "EDT", "the first 01:30");
        assert_eq!(
            next_daily(at("01:30"), first).date_naive(),
            chrono::NaiveDate::from_ymd_opt(2026, 11, 2).unwrap(),
            "the repeated hour does not run it twice"
        );
    }
}
