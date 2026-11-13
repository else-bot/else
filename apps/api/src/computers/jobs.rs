//! Commands and background jobs in a bot's computer.
//!
//! A foreground command runs under the caller's lease. Its output goes to
//! files in the guest and only bounded tails come back, so neither the
//! runtime nor the API buffers unbounded output. A background job gets an
//! ID, a slot (at most `MAX_JOBS` per bot, reserved atomically), its own
//! `job:<id>` lease (so it keeps the computer running after the provider
//! turn that started it), a deadline, and guest-side files under
//! `/var/lib/elsebot/jobs/<id>` (`pid`, `exit`, `out`). Maintenance polls
//! running jobs, records their outcome, enforces deadlines, and releases
//! their leases; cancellation is recorded only once the job's process group
//! is gone. Commands run as the guest's unprivileged user, which has
//! passwordless sudo.

mod commands;
mod files;

use std::time::Duration;

use sqlx::Row as _;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{Acquire, ComputerError, ComputerManager, ExecOutput};
use crate::db::computer_job_waits as job_waits;
use crate::db::task_secrets::{Env, redact};

/// Running background jobs per bot.
pub const MAX_JOBS: i64 = 4;
pub const MAX_JOB_DURATION: Duration = Duration::from_secs(6 * 3600);
/// Bytes of a command's stdout and of its stderr returned (the end).
pub const TAIL_BYTES: usize = 6_000;
const JOBS_DIR: &str = "/var/lib/elsebot/jobs";
const STALE_START: Duration = Duration::from_secs(120);

const ENV_FROM_STDIN: &str = r#"r=""
while IFS=' ' read -r n v; do [ -n "$n" ] || continue; x=$(printf %s "$v" | /usr/bin/base64 -d; echo .); export "$n=${x%.}"; r="$r $n"; done
export ELSE_REDACT_VARS="$r""#;

pub(crate) const FILTER: &str = r#"import os, sys
names = os.environ.get("ELSE_REDACT_VARS", "").split()
vals = sorted(((os.environb.get(n.encode(), b""), n) for n in names), key=lambda v: -len(v[0]))
vals = [(v, ("[secret %s]" % n).encode()) for v, n in vals if len(v) >= 4]
keep = max((len(v) for v, _ in vals), default=1) - 1
out = sys.stdout.buffer
buf = b""
def clean(b):
    for v, m in vals:
        b = b.replace(v, m)
    return b
while True:
    chunk = os.read(0, 65536)
    if not chunk:
        break
    buf = clean(buf + chunk)
    if len(buf) > keep:
        cut = len(buf) - keep
        out.write(buf[:cut]); out.flush(); buf = buf[cut:]
out.write(clean(buf)); out.flush()"#;

const RUN: &str = r#"cd "$HOME/work" 2>/dev/null || cd "$HOME"
@ENV@
mkdir -p "$3"; cat /proc/sys/kernel/random/boot_id > "$3/boot" 2>/dev/null
o=$(mktemp); e=$(mktemp)
if [ -n "$ELSE_REDACT_VARS" ]; then
  mkfifo "$3/o" "$3/e"
  /usr/bin/setsid /bin/sh -c '/usr/bin/python3 -I -S -c "$4" < "$1/o" > "$2" & /usr/bin/python3 -I -S -c "$4" < "$1/e" > "$3" & PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH" /bin/sh -c "$5" > "$1/o" 2> "$1/e" < /dev/null; c=$?; wait; exit $c' sh "$3" "$o" "$e" "$4" "$1" < /dev/null &
else
  PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH" /usr/bin/setsid /bin/sh -c "$1" > "$o" 2> "$e" < /dev/null &
fi
pg=$!; echo $pg > "$3/pid"
wait $pg; c=$?
/usr/bin/kill -s KILL -- "-$pg" 2>/dev/null
printf '%s\n%s\n%s\n' "$c" "$(wc -c < "$o")" "$(wc -c < "$e")"
tail -c "$2" "$o"; printf '\n\000\n'; tail -c "$2" "$e"
rm -f "$o" "$e"; rm -rf "$3""#;

const LAUNCH: &str = r#"mkdir -p "$1" && { cd "$HOME/work" 2>/dev/null || cd "$HOME"; }
@ENV@
cat /proc/sys/kernel/random/boot_id > "$1/boot" 2>/dev/null
if [ -n "$ELSE_REDACT_VARS" ]; then
  /usr/bin/setsid /bin/sh -c '{ PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH" /bin/sh -c "$2" < /dev/null; echo $? > "$1/code"; } 2>&1 | /usr/bin/python3 -I -S -c "$3" > "$1/out"; mv "$1/code" "$1/exit"' sh "$1" "$2" "$3" > /dev/null 2>&1 < /dev/null &
else
  /usr/bin/setsid /bin/sh -c 'PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH" /bin/sh -c "$2" > "$1/out" 2>&1 < /dev/null; echo $? > "$1/exit"' sh "$1" "$2" > /dev/null 2>&1 < /dev/null &
fi
echo $! > "$1/pid"; echo launched"#;

const RUNS_DIR: &str = "/var/lib/elsebot/runs";
const REDACT_MARGIN: usize = crate::db::task_secrets::MAX_VALUE + 64;
const JOB_TAIL: usize = 4000;

const PROBE: &str = r#"b=$(cat "$1/boot" 2>/dev/null) && [ "$b" != "$(cat /proc/sys/kernel/random/boot_id)" ] && [ ! -f "$1/exit" ] && { echo lost; exit 0; }
if [ -f "$1/exit" ]; then echo "done $(cat "$1/exit")"; elif [ -f "$1/pid" ] && kill -0 "$(cat "$1/pid")" 2>/dev/null; then echo running; else echo lost; fi; tail -c "$2" "$1/out" 2>/dev/null"#;

/// Stop a job's whole process group and confirm it is gone. procps `kill`
/// takes a negative process group (the shell builtin does not). Once it is
/// gone, the browser work of any script that no longer runs is reconciled
/// (`else-browser reconcile` closes the tabs it opened and halts code in
/// those it attached to) and any key or button an input batch held is
/// released (`else-input --release-all`). Prints `stopped` or `alive`.
const KILL: &str = r#"settled() { [ -x /usr/local/bin/else-browser ] && /usr/local/bin/else-browser reconcile > /dev/null 2>&1; [ -x /usr/local/bin/else-input ] && DISPLAY=:99 /usr/local/bin/else-input --release-all > /dev/null 2>&1; echo stopped; exit 0; }
b=$(cat "$1/boot" 2>/dev/null) && [ "$b" != "$(cat /proc/sys/kernel/random/boot_id)" ] && settled
pg=$(cat "$1/pid" 2>/dev/null) || settled
alive() { pgrep -g "$pg" > /dev/null 2>&1; }
alive || settled
/usr/bin/kill -s TERM -- "-$pg" 2>/dev/null
for i in 1 2 3 4 5 6 7 8 9 10; do alive || break; sleep 0.3; done
alive && /usr/bin/kill -s KILL -- "-$pg" 2>/dev/null
for i in 1 2 3 4 5 6 7 8 9 10; do alive || break; sleep 0.2; done
if alive; then echo alive; else [ -f "$1/exit" ] || echo 130 > "$1/exit"; settled; fi"#;

/// Where a job is in its life, as stored in `guest_jobs.status`. A job is
/// `starting` until the guest acknowledges the launch, and `cancelling`
/// from a cancel request until the guest confirms its process group is
/// gone. The last four are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum JobStatus {
    Starting,
    Running,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
    Lost,
}

impl JobStatus {
    pub fn is_ended(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Lost
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Lost => "lost",
        }
    }
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a launched job ended, with the exit code each outcome records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Exited(Option<i32>),
    Cancelled,
    Lost,
}

impl Outcome {
    fn status(self) -> JobStatus {
        match self {
            Self::Exited(Some(0)) => JobStatus::Completed,
            Self::Exited(_) => JobStatus::Failed,
            Self::Cancelled => JobStatus::Cancelled,
            Self::Lost => JobStatus::Lost,
        }
    }

    fn exit_code(self) -> Option<i32> {
        match self {
            Self::Exited(code) => code,
            Self::Cancelled => Some(130),
            Self::Lost => None,
        }
    }
}

/// Ask for a job's process group to be stopped: one job, or with `job_id`
/// `None` every active job of the bot. The request is recorded before any
/// kill is attempted, so maintenance retries the kill until the guest
/// confirms it. Returns each job now `cancelling` with its lease.
pub async fn request_cancel<'c>(
    conn: impl sqlx::SqliteExecutor<'c>,
    bot_id: Uuid,
    job_id: Option<Uuid>,
) -> Result<Vec<(Uuid, Option<Uuid>)>, sqlx::Error> {
    sqlx::query_as(
        "UPDATE guest_jobs SET status = 'cancelling'
         WHERE bot_id = ?1 AND (?2 IS NULL OR id = ?2) AND status IN ('starting', 'running', 'cancelling')
         RETURNING id, lease_id",
    )
    .bind(bot_id)
    .bind(job_id)
    .fetch_all(conn)
    .await
}

/// What a tool learns about a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobState {
    pub id: Uuid,
    pub status: JobStatus,
    pub exit_code: Option<i32>,
    pub output_tail: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    Job(Uuid),
    /// The computer could not be held (queued, owner-stopped, ...).
    NotReady(Acquire),
    /// Too many jobs already run on this computer.
    TooMany,
    /// The turn that asked was stopped (or a bot-wide stop is pending).
    Stopped,
}

enum Launch {
    Launched,
    NotLaunched(String),
    Uncertain(String),
    Interrupted,
}

enum Inspect {
    Out(ExecOutput),
    Gone,
    Paused,
    Unknown,
}

/// A foreground command's result with bounded output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    /// The last `TAIL_BYTES` of each stream.
    pub stdout: String,
    pub stderr: String,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
}

fn tail(text: &str, max: usize) -> String {
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

fn with_env(script: &str) -> String {
    script.replace("@ENV@", ENV_FROM_STDIN)
}

fn env_input(env: &Env) -> Vec<u8> {
    use base64::Engine;
    let mut input = Vec::new();
    for (name, value) in &env.vars {
        input.extend_from_slice(name.as_bytes());
        input.push(b' ');
        input.extend_from_slice(
            base64::engine::general_purpose::STANDARD
                .encode(value.as_bytes())
                .as_bytes(),
        );
        input.push(b'\n');
    }
    input
}

#[derive(Clone)]
pub(super) struct Foreground {
    bot_id: Uuid,
    turn_id: Option<Uuid>,
    runtime_name: String,
    dir: String,
    alive: crate::actor::Actor<bool>,
}

fn parse_run(out: &ExecOutput) -> Option<CommandOutput> {
    let mut lines = out.stdout.splitn(4, '\n');
    let exit_code = lines.next()?.trim().parse::<i32>().ok();
    let stdout_bytes = lines.next()?.trim().parse().ok()?;
    let stderr_bytes = lines.next()?.trim().parse().ok()?;
    let rest = lines.next().unwrap_or_default();
    let (stdout, stderr) = rest.split_once("\n\0\n").unwrap_or((rest, ""));
    Some(CommandOutput {
        exit_code,
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
        stdout_bytes,
        stderr_bytes,
    })
}

impl ComputerManager {
    pub(super) fn as_guest_user(&self, script: &str) -> (String, Vec<String>) {
        if self.config.desktop {
            (
                "/usr/sbin/runuser".into(),
                vec![
                    "-u".into(),
                    "elsebot".into(),
                    "--".into(),
                    "/bin/sh".into(),
                    "-c".into(),
                    script.into(),
                    "sh".into(),
                ],
            )
        } else {
            (
                "/bin/sh".into(),
                vec!["-c".into(), script.into(), "sh".into()],
            )
        }
    }

    async fn guest(
        &self,
        lease: Uuid,
        script: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ExecOutput, ComputerError> {
        let (program, mut all) = self.as_guest_user(script);
        all.extend(args.iter().map(|a| (*a).to_owned()));
        let all: Vec<&str> = all.iter().map(String::as_str).collect();
        self.exec(lease, &program, &all, timeout).await
    }

    /// Start a background job with its own lease and deadline. Admission
    /// is fenced against bot-wide Stop work and deletion: the slot is
    /// reserved inside the immediate transaction (which they hold exclusively) only
    /// while the bot is active and the originating turn still runs, and the
    /// launch happens under the bot's operation lock only while the job is
    /// still `starting`, so a stop either sees the job or the job sees the
    /// stop.
    pub async fn start_job(
        &self,
        bot_id: Uuid,
        turn_id: Option<Uuid>,
        command: &str,
        max_duration: Duration,
    ) -> Result<Started, ComputerError> {
        self.start_job_with(
            bot_id,
            turn_id,
            command,
            max_duration,
            &Env::default(),
            &[],
            None,
        )
        .await
    }

    /// [`Self::start_job`] with task-secret variables, exported to
    /// the job at launch over stdin, its output filtered in the guest. The
    /// job records which secrets it used (names), never their values. A
    /// job linked to one of the bot's live webhooks fires it when it exits 0.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_job_with(
        &self,
        bot_id: Uuid,
        turn_id: Option<Uuid>,
        command: &str,
        max_duration: Duration,
        env: &Env,
        secret_names: &[String],
        webhook: Option<Uuid>,
    ) -> Result<Started, ComputerError> {
        let duration = max_duration.min(MAX_JOB_DURATION);
        let id = {
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            let bot: Option<(bool, bool)> =
                sqlx::query_as("SELECT status = 'active', cancel_pending FROM bots WHERE id = ?1")
                    .bind(bot_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            let Some((true, cancel_pending)) = bot else {
                return Ok(Started::NotReady(Acquire::BotInactive));
            };
            let current = match turn_id {
                Some(turn) => {
                    sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM turns WHERE id = ?1 AND bot_id = ?2
                                    AND status IN ('preparing', 'sending', 'running'))",
                    )
                    .bind(turn)
                    .bind(bot_id)
                    .fetch_one(&mut *tx)
                    .await?
                }
                None => !cancel_pending,
            };
            if !current {
                return Ok(Started::Stopped);
            }
            let Some(id) = self
                .reserve_job(
                    &mut tx,
                    bot_id,
                    turn_id,
                    command,
                    duration,
                    secret_names,
                    webhook,
                )
                .await?
            else {
                return Ok(Started::TooMany);
            };
            tx.commit().await?;
            id
        };
        if crate::faults::hit("job.after_insert") {
            return Err(ComputerError::Interrupted);
        }
        self.launch_reserved(bot_id, id, command, env).await
    }

    /// Take a job slot inside the caller's immediate transaction: the job
    /// is recorded `starting`, without a lease, until [`Self::launch_reserved`]
    /// launches it. Maintenance fails a job still unlaunched after
    /// `STALE_START`. Returns `None` when `MAX_JOBS` already run. The caller
    /// checks the bot is active and its work current.
    #[allow(clippy::too_many_arguments)]
    pub async fn reserve_job(
        &self,
        conn: &mut sqlx::SqliteConnection,
        bot_id: Uuid,
        turn_id: Option<Uuid>,
        command: &str,
        duration: Duration,
        secret_names: &[String],
        webhook: Option<Uuid>,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        let id = Uuid::new_v4();
        let duration = duration.min(MAX_JOB_DURATION);
        let running: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM guest_jobs
             WHERE bot_id = ?1 AND status IN ('starting', 'running', 'cancelling')",
        )
        .bind(bot_id)
        .fetch_one(&mut *conn)
        .await?;
        if running >= MAX_JOBS {
            return Ok(None);
        }
        sqlx::query(
            "INSERT INTO bot_computers (bot_id, runtime_name, image, cpus, memory_mib, network)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT DO NOTHING",
        )
        .bind(bot_id)
        .bind(Self::runtime_name(bot_id))
        .bind(&self.config.image)
        .bind(self.config.cpus as i32)
        .bind(self.config.memory_mib as i32)
        .bind(&self.config.network)
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO guest_jobs (id, bot_id, turn_id, command, deadline, secrets, webhook_id)
             VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', printf('%+f seconds', ?5)), ?6,
                     (SELECT id FROM webhooks WHERE id = ?7 AND bot_id = ?2 AND deleted_at IS NULL))",
        )
        .bind(id)
        .bind(bot_id)
        .bind(turn_id)
        .bind(command)
        .bind(duration.as_secs_f64())
        .bind(sqlx::types::Json(secret_names))
        .bind(webhook)
        .execute(&mut *conn)
        .await?;
        Ok(Some(id))
    }

    /// Hold the computer for a reserved job and launch it. A job stopped
    /// before it holds the computer is not launched.
    pub async fn launch_reserved(
        &self,
        bot_id: Uuid,
        id: Uuid,
        command: &str,
        env: &Env,
    ) -> Result<Started, ComputerError> {
        let acquired = self.acquire(bot_id, &format!("job:{id}")).await?;
        let Acquire::Ready { lease_id, .. } = acquired else {
            self.fail_start(id, "the computer was not available")
                .await?;
            return Ok(Started::NotReady(acquired));
        };
        let attached = sqlx::query(
            "UPDATE guest_jobs SET lease_id = ?2 WHERE id = ?1 AND status = 'starting'",
        )
        .bind(id)
        .bind(lease_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if attached == 0 {
            self.release(lease_id).await?;
            return Ok(Started::Stopped);
        }
        match self.launch(lease_id, id, command, env).await {
            Launch::Launched => {
                sqlx::query(
                    "UPDATE guest_jobs SET status = 'running' WHERE id = ?1 AND status = 'starting'",
                )
                .bind(id)
                .execute(&self.pool)
                .await?;
            }
            Launch::NotLaunched(error) => {
                self.fail_start(id, &error).await?;
                self.release(lease_id).await?;
            }
            Launch::Uncertain(error) => {
                sqlx::query("UPDATE guest_jobs SET error = ?2 WHERE id = ?1")
                    .bind(id)
                    .bind(super::bounded(
                        &format!("launch unconfirmed: {error}"),
                        2000,
                    ))
                    .execute(&self.pool)
                    .await?;
            }
            Launch::Interrupted => return Err(ComputerError::Interrupted),
        }
        Ok(Started::Job(id))
    }

    async fn launch(&self, lease: Uuid, id: Uuid, command: &str, env: &Env) -> Launch {
        let checked = match self.checked(lease).await {
            Ok(checked) => checked,
            Err(err @ (ComputerError::LeaseEnded | ComputerError::NotRunning)) => {
                return Launch::NotLaunched(err.to_string());
            }
            Err(err) => return Launch::Uncertain(err.to_string()),
        };
        let (_, name, _op) = checked;
        let starting: Result<bool, sqlx::Error> = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM guest_jobs WHERE id = ?1 AND status = 'starting')",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await;
        match starting {
            Ok(true) => {}
            Ok(false) => return Launch::NotLaunched("stopped before launch".into()),
            Err(err) => return Launch::Uncertain(err.to_string()),
        }
        let dir = format!("{JOBS_DIR}/{id}");
        let (program, mut all) = self.as_guest_user(&with_env(LAUNCH));
        all.push(dir);
        all.push(command.to_owned());
        all.push(FILTER.to_owned());
        let all: Vec<&str> = all.iter().map(String::as_str).collect();
        let launched = if env.vars.is_empty() {
            self.runtime
                .exec(&name, &program, &all, Duration::from_secs(30))
                .await
        } else {
            self.runtime
                .exec_input(
                    &name,
                    &program,
                    &all,
                    &env_input(env),
                    Duration::from_secs(30),
                )
                .await
        };
        if crate::faults::hit("job.after_launch") {
            return Launch::Interrupted;
        }
        // The launch script's own errors bypass the guest filter.
        let masked = |text: &str| redact(text, &env.redaction());
        match launched {
            Ok(out) if out.stdout.contains("launched") => Launch::Launched,
            Ok(out) => Launch::Uncertain(masked(&out.stderr)),
            Err(err) => Launch::Uncertain(masked(&err.to_string())),
        }
    }

    async fn fail_start(&self, id: Uuid, error: &str) -> Result<(), ComputerError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "UPDATE guest_jobs SET status = 'failed', error = ?2, finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1 AND status = 'starting'",
        )
        .bind(id)
        .bind(super::bounded(error, 2000))
        .execute(&mut *tx)
        .await?;
        job_waits::publish_settled(&mut tx, id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// A job of this bot, refreshed from the guest while it runs.
    pub async fn job(&self, bot_id: Uuid, job_id: Uuid) -> Result<Option<JobState>, ComputerError> {
        let found: Option<(JobStatus, Option<Uuid>)> =
            sqlx::query_as("SELECT status, lease_id FROM guest_jobs WHERE id = ?1 AND bot_id = ?2")
                .bind(job_id)
                .bind(bot_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some((status, lease)) = found else {
            return Ok(None);
        };
        if let (JobStatus::Running, Some(lease)) = (status, lease) {
            self.probe_job(bot_id, job_id, lease).await?;
        }
        self.load_job(bot_id, job_id).await
    }

    async fn load_job(
        &self,
        bot_id: Uuid,
        job_id: Uuid,
    ) -> Result<Option<JobState>, ComputerError> {
        let row = sqlx::query(
            "SELECT id, status, exit_code, coalesce(output_tail, '') AS tail, command
             FROM guest_jobs WHERE id = ?1 AND bot_id = ?2",
        )
        .bind(job_id)
        .bind(bot_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| JobState {
            id: r.get("id"),
            status: r.get("status"),
            exit_code: r.get("exit_code"),
            output_tail: r.get("tail"),
            command: r.get("command"),
        }))
    }

    async fn inspect_held(
        &self,
        bot_id: Uuid,
        script: &str,
        args: &[&str],
    ) -> Result<Inspect, ComputerError> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT runtime_name, observed_state FROM bot_computers WHERE bot_id = ?1",
        )
        .bind(bot_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((name, state)) = row else {
            return Ok(Inspect::Gone);
        };
        match state.as_str() {
            "running" => {}
            "absent" | "stopped" | "missing" => return Ok(Inspect::Gone),
            "paused" => return Ok(Inspect::Paused),
            _ => return Ok(Inspect::Unknown),
        }
        let (program, mut all) = self.as_guest_user(script);
        all.extend(args.iter().map(|a| (*a).to_owned()));
        let all: Vec<&str> = all.iter().map(String::as_str).collect();
        match self
            .runtime
            .exec(&name, &program, &all, Duration::from_secs(30))
            .await
        {
            Ok(out) => Ok(Inspect::Out(out)),
            Err(err) => {
                tracing::warn!(bot = %bot_id, error = %err, "inspecting a guest job failed");
                Ok(Inspect::Unknown)
            }
        }
    }

    async fn probe_job(
        &self,
        bot_id: Uuid,
        job_id: Uuid,
        lease: Uuid,
    ) -> Result<(), ComputerError> {
        let dir = format!("{JOBS_DIR}/{job_id}");
        let op = self.op(bot_id).await;
        let margin = (JOB_TAIL + REDACT_MARGIN).to_string();
        let out = match self.inspect_held(bot_id, PROBE, &[&dir, &margin]).await? {
            Inspect::Out(out) => out,
            Inspect::Gone => {
                drop(op);
                return self.finish_job(job_id, Outcome::Lost, "", lease).await;
            }
            Inspect::Paused | Inspect::Unknown => return Ok(()),
        };
        drop(op);
        let (first, rest) = out
            .stdout
            .split_once('\n')
            .unwrap_or((out.stdout.as_str(), ""));
        let output = tail(&self.redact_job(job_id, rest).await?, JOB_TAIL);
        if let Some(code) = first.strip_prefix("done ") {
            let code = code.trim().parse::<i32>().ok();
            self.finish_job(job_id, Outcome::Exited(code), &output, lease)
                .await
        } else if first == "lost" {
            self.finish_job(job_id, Outcome::Lost, &output, lease).await
        } else {
            sqlx::query(
                "UPDATE guest_jobs SET output_tail = ?2 WHERE id = ?1 AND status = 'running'",
            )
            .bind(job_id)
            .bind(output)
            .execute(&self.pool)
            .await?;
            Ok(())
        }
    }

    async fn redact_job(&self, job_id: Uuid, output: &str) -> Result<String, ComputerError> {
        let names: Option<sqlx::types::Json<Vec<String>>> =
            sqlx::query_scalar("SELECT secrets FROM guest_jobs WHERE id = ?1")
                .bind(job_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(names) = names.filter(|n| !n.is_empty()) else {
            return Ok(output.to_owned());
        };
        match crate::db::task_secrets::resolve(&self.pool, self.secret_key.as_deref(), &names.0)
            .await
        {
            Ok(env) => Ok(redact(output, &env.redaction())),
            Err(err) => {
                tracing::warn!(job = %job_id, error = %err, "a job's secrets were not available for redaction");
                Ok(output.to_owned())
            }
        }
    }

    async fn finish_job(
        &self,
        job_id: Uuid,
        outcome: Outcome,
        output: &str,
        lease: Uuid,
    ) -> Result<(), ComputerError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "UPDATE guest_jobs SET status = ?2, exit_code = ?3, output_tail = ?4, finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1 AND status IN ('starting', 'running', 'cancelling')",
        )
        .bind(job_id)
        .bind(outcome.status())
        .bind(outcome.exit_code())
        .bind(output)
        .execute(&mut *tx)
        .await?;
        job_waits::publish_settled(&mut tx, job_id).await?;
        tx.commit().await?;
        self.release(lease).await
    }

    /// Stop a job (its whole process group). Recorded as cancelled only
    /// once the guest confirms nothing of it runs, or `lost` once its
    /// computer is confirmed stopped; otherwise it stays `cancelling` with
    /// its lease, the error says why, and maintenance retries the kill. A
    /// revoked lease is not evidence.
    pub async fn cancel_job(
        &self,
        bot_id: Uuid,
        job_id: Uuid,
    ) -> Result<Option<JobState>, ComputerError> {
        let requested = request_cancel(&self.pool, bot_id, Some(job_id)).await?;
        if let Some(&(_, lease)) = requested.first() {
            self.kill_job(bot_id, job_id, lease).await?;
        }
        self.load_job(bot_id, job_id).await
    }

    async fn kill_job(
        &self,
        bot_id: Uuid,
        job_id: Uuid,
        lease: Option<Uuid>,
    ) -> Result<(), ComputerError> {
        let op = self.op(bot_id).await;
        let Some(lease) = lease else {
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            sqlx::query(
                "UPDATE guest_jobs SET status = 'cancelled', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE id = ?1 AND lease_id IS NULL AND status IN ('starting', 'cancelling')",
            )
            .bind(job_id)
            .execute(&mut *tx)
            .await?;
            job_waits::publish_settled(&mut tx, job_id).await?;
            tx.commit().await?;
            return Ok(());
        };
        let dir = format!("{JOBS_DIR}/{job_id}");
        let mut resumed = false;
        let note = loop {
            break match self.inspect_held(bot_id, KILL, &[&dir]).await? {
                Inspect::Out(out) if out.stdout.trim_end().ends_with("stopped") => {
                    drop(op);
                    return self.finish_job(job_id, Outcome::Cancelled, "", lease).await;
                }
                Inspect::Out(out) => format!("cancellation not confirmed: {}", out.stdout.trim()),
                Inspect::Gone => {
                    drop(op);
                    return self.finish_job(job_id, Outcome::Lost, "", lease).await;
                }
                Inspect::Paused if self.bot_stopping(bot_id).await? => {
                    if self.stop_held(bot_id, super::Reason::ForJobs).await? {
                        return self.settle_stopped(bot_id, job_id).await;
                    }
                    "the computer is paused and could not be stopped".into()
                }
                Inspect::Paused if !resumed => match self.resume_to_kill(bot_id).await? {
                    None => {
                        resumed = true;
                        continue;
                    }
                    Some(note) => note,
                },
                Inspect::Paused => "the computer is paused".into(),
                Inspect::Unknown => "the computer did not answer".into(),
            };
        };
        sqlx::query("UPDATE guest_jobs SET error = ?2 WHERE id = ?1")
            .bind(job_id)
            .bind(super::bounded(&note, 2000))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn bot_stopping(&self, bot_id: Uuid) -> Result<bool, ComputerError> {
        let active: Option<bool> = sqlx::query_scalar(
            "SELECT status = 'active' AND NOT cancel_pending FROM bots WHERE id = ?1",
        )
        .bind(bot_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(active != Some(true))
    }

    /// Record the job and every `cancelling` job of the bot as cancelled
    /// once its computer is confirmed stopped.
    async fn settle_stopped(&self, bot_id: Uuid, job_id: Uuid) -> Result<(), ComputerError> {
        let jobs: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT id, lease_id FROM guest_jobs
             WHERE bot_id = ?1 AND lease_id IS NOT NULL AND (id = ?2 OR status = 'cancelling')
               AND status IN ('starting', 'running', 'cancelling')",
        )
        .bind(bot_id)
        .bind(job_id)
        .fetch_all(&self.pool)
        .await?;
        for (job, lease) in jobs {
            self.finish_job(job, Outcome::Cancelled, "", lease).await?;
        }
        Ok(())
    }

    /// Resume a paused computer so one job's process group can be killed
    /// without stopping the computer, which would end the bot's other jobs.
    /// Returns why it was not resumed. The caller holds the operation lock.
    async fn resume_to_kill(&self, bot_id: Uuid) -> Result<Option<String>, ComputerError> {
        let name: Option<String> = sqlx::query_scalar(
            "SELECT runtime_name FROM bot_computers
             WHERE bot_id = ?1 AND observed_state = 'paused' AND desired_state = 'auto'
               AND NOT EXISTS (SELECT 1 FROM computer_upgrades WHERE bot_id = ?1 AND state = 'started')",
        )
        .bind(bot_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(name) = name else {
            return Ok(Some("the computer is paused".into()));
        };
        match self.resume_held(bot_id, &name).await {
            Ok(()) => {
                self.stop_held(bot_id, super::Reason::MustStop).await?;
                Ok(None)
            }
            Err(err) => {
                let note = format!("resuming the computer failed: {err}");
                self.observe_after_failure(bot_id, &note).await?;
                Ok(Some(note))
            }
        }
    }

    /// Retry the kill of each of the bot's `cancelling` jobs, for the
    /// offline upgrade command, which runs no maintenance loop.
    pub(super) async fn confirm_cancels(&self, bot_id: Uuid) -> Result<(), ComputerError> {
        let jobs: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
            "SELECT id, lease_id FROM guest_jobs WHERE bot_id = ?1 AND status = 'cancelling'",
        )
        .bind(bot_id)
        .fetch_all(&self.pool)
        .await?;
        for (job, lease) in jobs {
            self.kill_job(bot_id, job, lease).await?;
        }
        Ok(())
    }

    pub(super) async fn poll_jobs(&self, cancel: &CancellationToken) -> Result<(), ComputerError> {
        let jobs: Vec<(Uuid, Uuid, Option<Uuid>, JobStatus, bool)> = sqlx::query_as(
            "SELECT id, bot_id, lease_id, status, deadline < strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             FROM guest_jobs
             WHERE status IN ('running', 'cancelling')
                OR (status = 'starting' AND started_at < strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', printf('%+f seconds', -?1)))",
        )
        .bind(STALE_START.as_secs_f64())
        .fetch_all(&self.pool)
        .await?;
        for (job, bot, lease, status, overdue) in jobs {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let result = async {
                if status == JobStatus::Cancelling {
                    return self.kill_job(bot, job, lease).await;
                }
                let Some(lease) = lease else {
                    let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
                    sqlx::query(
                        "UPDATE guest_jobs SET status = 'failed', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                             error = 'interrupted before launch'
                         WHERE id = ?1 AND status = 'starting' AND lease_id IS NULL",
                    )
                    .bind(job)
                    .execute(&mut *tx)
                    .await?;
                    job_waits::publish_settled(&mut tx, job).await?;
                    tx.commit().await?;
                    return Ok(());
                };
                if overdue {
                    sqlx::query("UPDATE guest_jobs SET error = 'deadline exceeded' WHERE id = ?1")
                        .bind(job)
                        .execute(&self.pool)
                        .await?;
                    self.kill_job(bot, job, Some(lease)).await
                } else {
                    if status == JobStatus::Starting {
                        sqlx::query(
                            "UPDATE guest_jobs SET status = 'running' WHERE id = ?1 AND status = 'starting'",
                        )
                        .bind(job)
                        .execute(&self.pool)
                        .await?;
                    }
                    self.probe_job(bot, job, lease).await
                }
            }
            .await;
            if let Err(err) = result {
                tracing::warn!(job = %job, error = %err, "polling a guest job failed");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    const TOKEN: &str = "gho_dummy0123456789abcdefDUMMY";

    fn filtered(writes: &[Vec<u8>]) -> String {
        let mut filter = Command::new("python3")
            .args(["-I", "-S", "-c", FILTER])
            .env_clear()
            .env("ELSE_REDACT_VARS", " ELSE_GITHUB_TOKEN")
            .env("ELSE_GITHUB_TOKEN", TOKEN)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = filter.stdin.take().unwrap();
        let out = std::thread::scope(|scope| {
            scope.spawn(move || {
                for write in writes {
                    stdin.write_all(write).unwrap();
                    stdin.flush().unwrap();
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
            filter.wait_with_output().unwrap()
        });
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn the_guest_filter_masks_a_secret_split_across_reads() {
        let (head, rest) = TOKEN.split_at(9);
        let cases: Vec<Vec<Vec<u8>>> = vec![
            vec![format!("a {head}").into(), format!("{rest} b\n").into()],
            TOKEN.bytes().map(|byte| vec![byte]).collect(),
            vec![[vec![b'x'; 65_530], TOKEN.as_bytes().to_vec()].concat()],
            vec![format!("{TOKEN}{TOKEN}").into()],
            vec![format!("end {head}").into(), rest.into()],
        ];
        for writes in cases {
            let out = filtered(&writes);
            assert!(!out.contains(head), "{}", tail(&out, 200));
            assert!(out.contains("[secret ELSE_GITHUB_TOKEN]"));
        }
    }

    #[test]
    fn command_output_is_parsed_from_the_run_script_format() {
        let out = ExecOutput {
            status: Some(0),
            stdout: "3\n12\n4\nhello world\n\n\0\nwarn".into(),
            stderr: String::new(),
            truncated: false,
        };
        let parsed = parse_run(&out).unwrap();
        assert_eq!(parsed.exit_code, Some(3));
        assert_eq!((parsed.stdout_bytes, parsed.stderr_bytes), (12, 4));
        assert_eq!(parsed.stdout, "hello world\n");
        assert_eq!(parsed.stderr, "warn");
    }
}
