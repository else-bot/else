use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use else_wire::backups::{
    BackupEstimate, BackupRun, BackupSnapshot, LocalBackupRun, LocalBackupSettings,
    LocalBackupStatus,
};
use else_wire::maintenance::ServerPause;
use sqlx::{Row, SqlitePool};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::BackupError;
use super::capture::{BackupConfig, Cadence, Coordinator};
use super::replicate::{RemoteState, Replicator};
use super::runs::LocalStatus;
use super::s3::S3;
use super::settings::{
    self, Destination, DestinationInput, Error, LocalSchedule, LocalUpdate, Settings, Update,
};
use super::worker::BackupWorker;
use crate::config::Config;
use crate::credentials::SecretKey;

#[derive(Clone)]
pub struct Backups {
    tx: mpsc::Sender<Command>,
    pool: SqlitePool,
    initial_estimate: BackupEstimate,
    probe_slots: Arc<tokio::sync::Semaphore>,
    local_dir: std::path::PathBuf,
    cadence: Cadence,
    /// `ELSE_BACKUP_KEEP`, which applies while the environment sets the
    /// schedule.
    env_keep: usize,
    gate: Option<Arc<crate::maintenance::Gate>>,
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
    key: Arc<SecretKey>,
    config: BackupConfig,
    coordinator: Coordinator,
}

/// The first wait after a failed run; each further failure doubles it, up
/// to [`MAX_RETRY`].
const FIRST_RETRY: Duration = Duration::from_secs(60);
const MAX_RETRY: Duration = Duration::from_secs(3600);

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
        let mut worker =
            BackupWorker::new(self.pool.clone(), self.key.clone(), self.config(settings))
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
                let worker =
                    BackupWorker::new(self.pool.clone(), self.key.clone(), self.config(settings));
                Some(Instant::now() + worker.next_due(interval).unwrap_or(FIRST_RETRY))
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
        let slot = last_daily(settings.local, now);
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
    let mut day = now.date_naive();
    loop {
        if let Some(at) = daily_slot(schedule, &now.timezone(), day)
            && at > now
        {
            return at;
        }
        day = day.succ_opt().expect("the calendar has a next day");
    }
}

/// The latest daily slot at or before `now`: the one a failed backup
/// retries for.
fn last_daily<Tz: chrono::TimeZone>(schedule: LocalSchedule, now: DateTime<Tz>) -> DateTime<Tz> {
    let mut day = now.date_naive();
    loop {
        if let Some(at) = daily_slot(schedule, &now.timezone(), day)
            && at <= now
        {
            return at;
        }
        day = day.pred_opt().expect("the calendar has a previous day");
    }
}

/// The schedule's instant on `day`. A time skipped by a DST change runs an
/// hour later; a repeated time runs at its first occurrence.
fn daily_slot<Tz: chrono::TimeZone>(
    schedule: LocalSchedule,
    zone: &Tz,
    day: chrono::NaiveDate,
) -> Option<DateTime<Tz>> {
    use chrono::offset::LocalResult;
    let naive = day.and_time(schedule.at.time());
    match zone.from_local_datetime(&naive) {
        LocalResult::Single(at) | LocalResult::Ambiguous(at, _) => Some(at),
        LocalResult::None => zone
            .from_local_datetime(&(naive + chrono::Duration::hours(1)))
            .earliest(),
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
                .turns
                .as_ref()
                .map(|turns| turns.gate.clone()),
            initial_estimate: BackupEstimate {
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

    pub async fn save(&self, update: Update) -> Result<BackupSnapshot, Error> {
        self.request(|tx| Command::Save(update, tx)).await??;
        self.snapshot().await
    }

    pub async fn save_local(&self, update: LocalUpdate) -> Result<BackupSnapshot, Error> {
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

    pub async fn snapshot(&self) -> Result<BackupSnapshot, Error> {
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
                .fold(size(&dir.join(super::chunks::CHUNKS)), |sum, set| {
                    sum.saturating_add(size(&dir.join(set)))
                });
            (sets.len(), bytes, free_space(&dir))
        });
        let (history, totals, latest, local_runs, disk) =
            tokio::join!(history, totals, latest, local_runs, disk);
        let (copies, local_bytes, free_bytes) = disk.map_err(|_| Error::Unavailable)?;
        let local_recent = local_runs?
            .into_iter()
            .map(|row| {
                let status = match row.try_get::<LocalStatus, _>("local_status")? {
                    LocalStatus::Completed if row.get::<i64, _>("whole") == 0 => {
                        LocalBackupStatus::Partial
                    }
                    status => status.into(),
                };
                Ok(LocalBackupRun {
                    set_id: Some(row.get("set_id")),
                    started_at: row.get("started_at"),
                    finished_at: row.get("finished_at"),
                    status,
                    bytes: (status != LocalBackupStatus::Failed)
                        .then(|| row.get::<Option<i64>, _>("ciphertext_bytes"))
                        .flatten()
                        .map(|n| n.max(0) as u64),
                    took_secs: row.get::<Option<i64>, _>("took").map(|n| n.max(0) as u64),
                    paused_secs: row
                        .get::<Option<i64>, _>("paused_ms")
                        .map(|ms| (ms.max(0) as u64 + 500) / 1000),
                    error: (status == LocalBackupStatus::Failed)
                        .then(|| row.get::<Option<String>, _>("error"))
                        .flatten(),
                })
            })
            .collect::<Result<_, sqlx::Error>>()?;
        let recent = history?
            .into_iter()
            .map(|row| {
                Ok(BackupRun {
                    set_id: row.get("set_id"),
                    finished_at: row.get("finished_at"),
                    bytes: row
                        .get::<Option<i64>, _>("ciphertext_bytes")
                        .map(|n| n.max(0) as u64),
                    uploaded_bytes: row
                        .get::<Option<i64>, _>("uploaded_bytes")
                        .map(|n| n.max(0) as u64),
                    local_status: row.try_get::<LocalStatus, _>("local_status")?.into(),
                    remote_status: row
                        .try_get::<Option<RemoteState>, _>("remote_status")?
                        .map(Into::into),
                    error: row.get::<Option<String>, _>("error").map(|_| {
                        "Remote upload failed. Check the destination and credentials.".into()
                    }),
                })
            })
            .collect::<Result<_, sqlx::Error>>()?;
        let totals = totals?;
        let estimate = latest?.map(|bytes| BackupEstimate { bytes: bytes.max(0) as u64, basis: "Based on the latest complete encrypted backup. The next backup may differ in size.".into() })
            .unwrap_or_else(|| self.initial_estimate.clone());
        let settings = view.settings;
        Ok(BackupSnapshot {
            revision: settings.revision,
            enabled: settings.enabled,
            interval_secs: settings.interval_secs,
            keep: settings.keep,
            destination: settings.destination.map(|d| d.config.into()),
            running: view.running,
            next_backup_at: view.next_backup_at,
            estimate: Some(estimate),
            recent,
            remote_count: totals.get::<i64, _>("count") as usize,
            remote_bytes: totals.get::<i64, _>("bytes").max(0) as u64,
            unavailable_reason: None,
            last_error: view.last_error,
            local: Some(LocalBackupSettings {
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
            }),
            pause: self
                .gate
                .as_ref()
                .and_then(|gate| gate.pause())
                .as_ref()
                .map(ServerPause::from),
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
    let stat = rustix::fs::statvfs(path).ok()?;
    Some(stat.f_bavail.saturating_mul(stat.f_frsize))
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

/// What the service is doing: nothing, or one backup task.
enum Activity {
    Idle,
    Running {
        kind: Job,
        task: JoinHandle<Result<(), Failure>>,
    },
}

/// The schedule of local captures or of remote runs: when the next one is
/// due, how long a failure waits before it retries, and the last error.
struct Track {
    due: Option<Instant>,
    backoff: Duration,
    error: Option<String>,
}

impl Track {
    fn new(due: Option<Instant>) -> Self {
        Self {
            due,
            backoff: FIRST_RETRY,
            error: None,
        }
    }

    fn succeeded(&mut self) {
        self.backoff = FIRST_RETRY;
        self.error = None;
    }

    /// Record a failure and return how long its retry waits.
    fn failed(&mut self, message: String) -> Duration {
        let wait = self.backoff;
        self.backoff = (self.backoff * 2).min(MAX_RETRY);
        self.error = Some(message);
        wait
    }
}

async fn run(
    engine: Engine,
    mut settings: Settings,
    mut rx: mpsc::Receiver<Command>,
    local_at: Option<Instant>,
    remote_at: Option<Instant>,
    cancel: CancellationToken,
) {
    let mut activity = Activity::Idle;
    let mut local = Track::new(local_at);
    let mut remote = Track::new(remote_at);
    // An upload-only retry of the remote track, apart from its full runs.
    let mut upload_retry = None;
    if settings.enabled
        && settings.interval_secs > 0
        && let Ok(worker) = engine.worker(&settings, true)
        && let Some(replicator) = worker.replicator()
        && replicator
            .awaiting(&engine.config.dir)
            .await
            .is_ok_and(|v| !v.is_empty())
    {
        upload_retry = Some(Instant::now());
    }
    loop {
        let wake = match activity {
            Activity::Running { .. } => None,
            Activity::Idle => [local.due, remote.due, upload_retry]
                .into_iter()
                .flatten()
                .min(),
        };
        tokio::select! {
            _ = cancel.cancelled() => {
                if let Activity::Running { mut task, .. } = activity
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
                        running: matches!(activity, Activity::Running { .. }),
                        next_backup_at: [remote.due, upload_retry].into_iter().flatten().min().map(wall_time),
                        next_local_at: local.due.map(wall_time),
                        last_error: remote.error.clone().or_else(|| local.error.clone()),
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
                        if matches!(activity, Activity::Running { .. }) { return Err(Error::Busy); }
                        let next = settings.updated(update)?;
                        settings::save(&engine.pool, &engine.key, &next).await?;
                        settings = next;
                        remote.due = engine.next_remote(&settings).await?;
                        remote.error = None;
                        upload_retry = None;
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
                        // A capture reschedules when it ends; an upload-only
                        // retry does not, so apply the new time now.
                        if matches!(activity, Activity::Idle | Activity::Running { kind: Job::Retry, .. }) {
                            local.due = engine.next_local(&settings);
                        }
                        Ok(())
                    }.await;
                    let _ = reply.send(result);
                }
                Command::Run(reply) => {
                    let result = if matches!(activity, Activity::Running { .. }) {
                        Err(Error::Busy)
                    } else if settings.destination.is_none() {
                        Err(Error::Invalid("Configure a backup destination first."))
                    } else {
                        engine.spawn(&settings, Job::Remote, cancel.child_token()).map(|task| {
                            activity = Activity::Running { kind: Job::Remote, task };
                        })
                    };
                    let _ = reply.send(result);
                }
            },
            outcome = async {
                match &mut activity {
                    Activity::Running { task, .. } => task.await,
                    Activity::Idle => std::future::pending().await,
                }
            } => {
                let Activity::Running { kind, .. } = std::mem::replace(&mut activity, Activity::Idle) else {
                    unreachable!("only a running task finishes");
                };
                let result = outcome.unwrap_or_else(|_| Err(Failure::Capture("Backup task stopped unexpectedly.".into())));
                // An upload-only retry captured nothing, so a local backup
                // that fell due meanwhile still runs.
                if !matches!(kind, Job::Retry) {
                    local.due = engine.next_local(&settings);
                }
                match (kind, result) {
                    (Job::Local, Ok(())) => local.succeeded(),
                    (Job::Local, Err(error)) => {
                        let wait = local.failed(error.message());
                        local.due = engine.retry_local(&settings, wait);
                    }
                    (Job::Remote | Job::Retry, Ok(())) => {
                        if matches!(kind, Job::Remote) {
                            local.succeeded();
                        }
                        remote.succeeded();
                        upload_retry = None;
                        remote.due = engine.next_remote(&settings).await.unwrap_or(None);
                        if remote.due.is_some_and(|at| at <= Instant::now()) {
                            remote.due = Some(Instant::now() + Duration::from_secs(settings.interval_secs).max(FIRST_RETRY));
                        }
                    }
                    (Job::Remote | Job::Retry, Err(error)) => {
                        let wait = remote.failed(error.message());
                        if settings.enabled && settings.interval_secs > 0 {
                            match error {
                                Failure::Capture(_) => {
                                    remote.due = Some(Instant::now() + wait);
                                    upload_retry = None;
                                }
                                Failure::Upload(_) => {
                                    upload_retry = Some(Instant::now() + wait);
                                    remote.due = Some(Instant::now() + Duration::from_secs(settings.interval_secs));
                                }
                            }
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
                let kind = if remote.due.is_some_and(|at| at <= now) {
                    Job::Remote
                } else if local.due.is_some_and(|at| at <= now) {
                    Job::Local
                } else {
                    Job::Retry
                };
                match engine.spawn(&settings, kind, cancel.child_token()) {
                    Ok(task) => activity = Activity::Running { kind, task },
                    Err(error) => {
                        remote.error = Some(error.to_string());
                        remote.due = None;
                        upload_retry = None;
                        local.due = engine.retry_local(&settings, FIRST_RETRY);
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

    #[test]
    fn a_retry_counts_from_the_slot_it_missed_across_daylight_saving() {
        // The retry window runs from the actual slot, not 24 hours before
        // the next one, so a DST day neither shrinks nor stretches it.
        let after_jump = New_York.with_ymd_and_hms(2026, 3, 8, 4, 0, 0).unwrap();
        let slot = last_daily(at("01:30"), after_jump);
        assert_eq!(
            slot,
            New_York.with_ymd_and_hms(2026, 3, 8, 1, 30, 0).unwrap()
        );
        assert_eq!(after_jump - slot, chrono::Duration::minutes(90));
        let after_repeat = New_York.with_ymd_and_hms(2026, 11, 1, 3, 0, 0).unwrap();
        let slot = last_daily(at("01:30"), after_repeat);
        assert_eq!(slot.offset().to_string(), "EDT", "the first 01:30");
        assert_eq!(after_repeat - slot, chrono::Duration::minutes(150));
        let before = Dubai.with_ymd_and_hms(2026, 11, 12, 2, 0, 0).unwrap();
        assert_eq!(
            last_daily(at("03:00"), before),
            Dubai.with_ymd_and_hms(2026, 11, 11, 3, 0, 0).unwrap(),
            "before today's time, the slot is yesterday's"
        );
    }
}
