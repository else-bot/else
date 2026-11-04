//! Commands and background jobs in a bot's computer (POC-018).
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
//! is gone. Commands run as the guest's unprivileged user.

mod commands;
mod files;

use std::time::Duration;

use sqlx::Row as _;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{Acquire, ComputerError, ComputerManager, ExecOutput};
use crate::db::task_secrets::{Env, redact};

/// Running background jobs per bot.
pub const MAX_JOBS: i64 = 4;
pub const MAX_JOB_DURATION: Duration = Duration::from_secs(6 * 3600);
/// Bytes of a command's stdout and of its stderr returned (the end).
pub const TAIL_BYTES: usize = 6_000;
const JOBS_DIR: &str = "/var/lib/elsebot/jobs";
/// A job still `starting` this long after creation was interrupted.
const STALE_START: Duration = Duration::from_secs(120);

/// Export task-secret variables sent on stdin as `NAME BASE64` lines
/// (values never appear in arguments; see `db::task_secrets`), exactly as
/// sent (a trailing newline survives the decoding), and list their names in
/// `ELSE_REDACT_VARS` for the output filter.
const ENV_FROM_STDIN: &str = r#"r=""
while IFS=' ' read -r n v; do [ -n "$n" ] || continue; x=$(printf %s "$v" | /usr/bin/base64 -d; echo .); export "$n=${x%.}"; r="$r $n"; done
export ELSE_REDACT_VARS="$r""#;

/// Copies stdin to stdout replacing every value of the variables named in
/// `ELSE_REDACT_VARS` (4 bytes or longer) with `[secret NAME]`, holding
/// back just enough to catch a value split across reads. Task-secret
/// output goes through it inside the guest, so no guest file the API
/// manages ever holds a value.
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

/// Run `$1` in its own session (its pid, in `$3/pid`, is the process group,
/// so Stop work can end it and its descendants) with its output in
/// temporary files, through the secret filter (`$4`) when task secrets were
/// sent; print the exit code, both sizes, then the last `$2` bytes of each,
/// separated by a NUL line. Whatever the command left running in its group
/// is ended with it. The run directory records the guest boot, so a kill
/// after a reboot never targets a reused process ID. The user's own tool
/// directories come first only for the user's command.
const RUN: &str = r#"cd "$HOME/work" 2>/dev/null || cd "$HOME"
@ENV@
mkdir -p "$3"; cat /proc/sys/kernel/random/boot_id > "$3/boot" 2>/dev/null
o=$(mktemp); e=$(mktemp)
if [ -n "$ELSE_REDACT_VARS" ]; then
  mkfifo "$3/o" "$3/e"
  /usr/bin/setsid /bin/sh -c '/usr/bin/python3 -c "$4" < "$1/o" > "$2" & /usr/bin/python3 -c "$4" < "$1/e" > "$3" & PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH" /bin/sh -c "$5" > "$1/o" 2> "$1/e" < /dev/null; c=$?; wait; exit $c' sh "$3" "$o" "$e" "$4" "$1" < /dev/null &
else
  PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH" /usr/bin/setsid /bin/sh -c "$1" > "$o" 2> "$e" < /dev/null &
fi
pg=$!; echo $pg > "$3/pid"
wait $pg; c=$?
/usr/bin/kill -s KILL -- "-$pg" 2>/dev/null
printf '%s\n%s\n%s\n' "$c" "$(wc -c < "$o")" "$(wc -c < "$e")"
tail -c "$2" "$o"; printf '\n\000\n'; tail -c "$2" "$e"
rm -f "$o" "$e"; rm -rf "$3""#;

/// Launch `$2` detached in its own session (its pid is the process group),
/// recording pid, boot, exit status, and output in `$1`. Task-secret
/// variables on stdin are exported to it, and its output then goes through
/// the secret filter (`$3`) before it reaches the output file.
const LAUNCH: &str = r#"mkdir -p "$1" && { cd "$HOME/work" 2>/dev/null || cd "$HOME"; }
@ENV@
cat /proc/sys/kernel/random/boot_id > "$1/boot" 2>/dev/null
if [ -n "$ELSE_REDACT_VARS" ]; then
  /usr/bin/setsid /bin/sh -c '{ PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH" /bin/sh -c "$2" < /dev/null; echo $? > "$1/code"; } 2>&1 | /usr/bin/python3 -c "$3" > "$1/out"; mv "$1/code" "$1/exit"' sh "$1" "$2" "$3" > /dev/null 2>&1 < /dev/null &
else
  /usr/bin/setsid /bin/sh -c 'PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH" /bin/sh -c "$2" > "$1/out" 2>&1 < /dev/null; echo $? > "$1/exit"' sh "$1" "$2" > /dev/null 2>&1 < /dev/null &
fi
echo $! > "$1/pid"; echo launched"#;

/// Where foreground commands record their process group.
const RUNS_DIR: &str = "/var/lib/elsebot/runs";
/// Output bytes read beyond a returned tail, so a secret value cut by the
/// tail boundary is still seen whole and redacted before the cut.
const REDACT_MARGIN: usize = crate::db::task_secrets::MAX_VALUE + 64;
/// The bytes of job output kept.
const JOB_TAIL: usize = 4000;

/// Report a job: `done <code>`, `running`, or `lost` (also when the guest
/// rebooted since it started), then the end of its output.
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

/// What a tool learns about a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobState {
    pub id: Uuid,
    pub status: String,
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
    /// Proven not started.
    NotLaunched(String),
    /// The guest may have started it.
    Uncertain(String),
    Interrupted,
}

/// What the guest says, if it can.
enum Inspect {
    Out(ExecOutput),
    /// The computer is confirmed not running.
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

/// A script with the task-secret prologue in place.
fn with_env(script: &str) -> String {
    script.replace("@ENV@", ENV_FROM_STDIN)
}

/// Task-secret variables as `NAME BASE64` lines for `ENV_FROM_STDIN`.
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

/// A foreground command while it runs.
#[derive(Clone)]
pub(super) struct Foreground {
    bot_id: Uuid,
    turn_id: Option<Uuid>,
    runtime_name: String,
    dir: String,
    /// True until the command returned; held while killing it.
    alive: crate::actor::Actor<bool>,
}

/// Parse the output of `RUN`.
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
    /// The guest user and its program prefix for running a script.
    pub(super) fn as_guest_user(&self, script: &str) -> (String, Vec<String>) {
        if self.config.desktop {
            (
                // Absolute paths: root resolves nothing through a PATH.
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
        self.start_job_with(bot_id, turn_id, command, max_duration, &Env::default(), &[])
            .await
    }

    /// [`Self::start_job`] with task-secret variables (POC-026), exported to
    /// the job at launch over stdin, its output filtered in the guest. The
    /// job records which secrets it used (names), never their values.
    pub async fn start_job_with(
        &self,
        bot_id: Uuid,
        turn_id: Option<Uuid>,
        command: &str,
        max_duration: Duration,
        env: &Env,
        secret_names: &[String],
    ) -> Result<Started, ComputerError> {
        let id = Uuid::new_v4();
        let duration = max_duration.min(MAX_JOB_DURATION);
        // Reserve a slot atomically before anything else.
        {
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
            let running: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM guest_jobs
                 WHERE bot_id = ?1 AND status IN ('starting', 'running', 'cancelling')",
            )
            .bind(bot_id)
            .fetch_one(&mut *tx)
            .await?;
            if running >= MAX_JOBS {
                return Ok(Started::TooMany);
            }
            // The computer row exists once a lease can be taken; create it
            // the way acquisition would if this is the first use.
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
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO guest_jobs (id, bot_id, turn_id, command, deadline, secrets)
                 VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', printf('%+f seconds', ?5)), ?6)",
            )
            .bind(id)
            .bind(bot_id)
            .bind(turn_id)
            .bind(command)
            .bind(duration.as_secs_f64())
            .bind(sqlx::types::Json(secret_names))
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
        }
        if crate::faults::hit("job.after_insert") {
            return Err(ComputerError::Interrupted);
        }
        let acquired = self.acquire(bot_id, &format!("job:{id}")).await?;
        let Acquire::Ready { lease_id, .. } = acquired else {
            self.fail_start(id, "the computer was not available")
                .await?;
            return Ok(Started::NotReady(acquired));
        };
        // Only while still starting: a stop meanwhile already ended it.
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
                // The guest may have accepted it. It keeps its lease and
                // stays `starting`; maintenance looks at its directory.
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

    /// Launch a job under its lease and the bot's operation lock, only
    /// while it is still `starting` (a kill holds the same lock).
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
        match launched {
            Ok(out) if out.stdout.contains("launched") => Launch::Launched,
            // The script may have started the job before failing or being
            // cut off, or the reply may have been lost: not proof either way.
            Ok(out) => Launch::Uncertain(out.stderr),
            Err(err) => Launch::Uncertain(err.to_string()),
        }
    }

    /// End a job that never got a lease (so never launched) or failed to
    /// launch.
    async fn fail_start(&self, id: Uuid, error: &str) -> Result<(), ComputerError> {
        sqlx::query(
            "UPDATE guest_jobs SET status = 'failed', error = ?2, finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1 AND status = 'starting'",
        )
        .bind(id)
        .bind(super::bounded(error, 2000))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A job of this bot, refreshed from the guest while it runs.
    pub async fn job(&self, bot_id: Uuid, job_id: Uuid) -> Result<Option<JobState>, ComputerError> {
        let found: Option<(String, Option<Uuid>)> =
            sqlx::query_as("SELECT status, lease_id FROM guest_jobs WHERE id = ?1 AND bot_id = ?2")
                .bind(job_id)
                .bind(bot_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some((status, lease)) = found else {
            return Ok(None);
        };
        if let (true, Some(lease)) = (status == "running", lease) {
            self.probe_job(bot_id, job_id, lease).await?;
        }
        self.load_job(job_id).await
    }

    async fn load_job(&self, job_id: Uuid) -> Result<Option<JobState>, ComputerError> {
        let row = sqlx::query(
            "SELECT id, status, exit_code, coalesce(output_tail, '') AS tail, command
             FROM guest_jobs WHERE id = ?1",
        )
        .bind(job_id)
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

    /// Run a job script in the bot's computer, under its operation lock,
    /// if the computer is running. This asks the guest for evidence about
    /// work already started, so it needs no lease and works for a stopped
    /// or deleted bot; it never wakes a computer. The caller holds the
    /// operation lock.
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
            // A stopped VM runs nothing; its jobs died with it.
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

    /// Ask the guest about a job and record what it reports. Without an
    /// answer (paused, uncertain) the job keeps its status and lease.
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
                return self.finish_job(job_id, "lost", None, "", Some(lease)).await;
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
            let status = if code == Some(0) {
                "completed"
            } else {
                "failed"
            };
            self.finish_job(job_id, status, code, &output, Some(lease))
                .await
        } else if first == "lost" {
            self.finish_job(job_id, "lost", None, &output, Some(lease))
                .await
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

    /// A job's output with its task-secret values redacted (the guest
    /// filter already removed them; this is a second check). The values
    /// are decrypted again by name: nothing derived from them is stored.
    async fn redact_job(&self, job_id: Uuid, output: &str) -> Result<String, ComputerError> {
        let row: Option<(Uuid, sqlx::types::Json<Vec<String>>)> =
            sqlx::query_as("SELECT bot_id, secrets FROM guest_jobs WHERE id = ?1")
                .bind(job_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some((bot_id, names)) = row.filter(|(_, n)| !n.is_empty()) else {
            return Ok(output.to_owned());
        };
        match crate::db::task_secrets::resolve(
            &self.pool,
            self.secret_key.as_deref(),
            bot_id,
            &names.0,
        )
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
        status: &str,
        code: Option<i32>,
        output: &str,
        lease: Option<Uuid>,
    ) -> Result<(), ComputerError> {
        sqlx::query(
            "UPDATE guest_jobs SET status = ?2, exit_code = ?3, output_tail = ?4, finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1 AND status IN ('starting', 'running', 'cancelling')",
        )
        .bind(job_id)
        .bind(status)
        .bind(code)
        .bind(output)
        .execute(&self.pool)
        .await?;
        match lease {
            Some(lease) => self.release(lease).await,
            None => Ok(()),
        }
    }

    /// Stop a job (its whole process group). Recorded as cancelled only
    /// once the guest confirms nothing of it runs, or `lost` once its
    /// computer is confirmed stopped; otherwise it keeps its status and
    /// lease, and the error says so. A revoked lease is not evidence.
    pub async fn cancel_job(
        &self,
        bot_id: Uuid,
        job_id: Uuid,
    ) -> Result<Option<JobState>, ComputerError> {
        let found: Option<(String, Option<Uuid>)> =
            sqlx::query_as("SELECT status, lease_id FROM guest_jobs WHERE id = ?1 AND bot_id = ?2")
                .bind(job_id)
                .bind(bot_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some((status, lease)) = found else {
            return Ok(None);
        };
        if matches!(status.as_str(), "starting" | "running" | "cancelling") {
            self.kill_job(bot_id, job_id, lease, false).await?;
        }
        self.load_job(job_id).await
    }

    /// Kill a job under the bot's operation lock (which a launch also
    /// holds). A job without a lease never launched and no longer can.
    /// With `stop_paused`, a paused computer holding the job is stopped.
    async fn kill_job(
        &self,
        bot_id: Uuid,
        job_id: Uuid,
        lease: Option<Uuid>,
        stop_paused: bool,
    ) -> Result<(), ComputerError> {
        let op = self.op(bot_id).await;
        let Some(lease) = lease else {
            sqlx::query(
                "UPDATE guest_jobs SET status = 'cancelled', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE id = ?1 AND lease_id IS NULL AND status IN ('starting', 'cancelling')",
            )
            .bind(job_id)
            .execute(&self.pool)
            .await?;
            return Ok(());
        };
        let dir = format!("{JOBS_DIR}/{job_id}");
        let note = match self.inspect_held(bot_id, KILL, &[&dir]).await? {
            Inspect::Out(out) if out.stdout.trim_end().ends_with("stopped") => {
                drop(op);
                return self
                    .finish_job(job_id, "cancelled", Some(130), "", Some(lease))
                    .await;
            }
            Inspect::Out(out) => format!("cancellation not confirmed: {}", out.stdout.trim()),
            Inspect::Gone => {
                drop(op);
                return self.finish_job(job_id, "lost", None, "", Some(lease)).await;
            }
            Inspect::Paused if stop_paused => {
                // Stopping the computer ends the job; the next pass sees it.
                self.stop_held(bot_id, super::Reason::ForJobs).await?;
                "the computer was paused; stopping it".into()
            }
            Inspect::Paused => "the computer is paused".into(),
            Inspect::Unknown => "the computer did not answer".into(),
        };
        sqlx::query("UPDATE guest_jobs SET error = ?2 WHERE id = ?1")
            .bind(job_id)
            .bind(super::bounded(&note, 2000))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Maintenance: refresh running jobs, kill jobs a bot-wide stop or
    /// deletion is cancelling, enforce deadlines, resolve jobs interrupted
    /// while starting, release the leases of jobs that ended. A failure on
    /// one job does not stop the others.
    pub(super) async fn poll_jobs(&self, cancel: &CancellationToken) -> Result<(), ComputerError> {
        let jobs: Vec<(Uuid, Uuid, Option<Uuid>, String, bool)> = sqlx::query_as(
            "SELECT id, bot_id, lease_id, status, deadline < strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') FROM guest_jobs
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
                if status == "cancelling" {
                    return self.kill_job(bot, job, lease, true).await;
                }
                let Some(lease) = lease else {
                    // Interrupted before it had a lease: nothing launched
                    // (a launch needs the lease).
                    sqlx::query(
                        "UPDATE guest_jobs SET status = 'failed', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                             error = 'interrupted before launch'
                         WHERE id = ?1 AND status = 'starting' AND lease_id IS NULL",
                    )
                    .bind(job)
                    .execute(&self.pool)
                    .await?;
                    return Ok(());
                };
                if overdue {
                    sqlx::query("UPDATE guest_jobs SET error = 'deadline exceeded' WHERE id = ?1")
                        .bind(job)
                        .execute(&self.pool)
                        .await?;
                    self.kill_job(bot, job, Some(lease), false).await
                } else {
                    if status == "starting" {
                        // Interrupted or unconfirmed launch: look at what
                        // the guest has (`lost` if it never started).
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
