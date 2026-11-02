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

use std::time::Duration;

use sqlx::Row as _;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{Acquire, ComputerError, ComputerManager, ExecOutput};

/// Running background jobs per bot.
pub const MAX_JOBS: i64 = 4;
pub const MAX_JOB_DURATION: Duration = Duration::from_secs(6 * 3600);
/// Bytes of a command's stdout and of its stderr returned (the end).
pub const TAIL_BYTES: usize = 6_000;
const JOBS_DIR: &str = "/var/lib/elsebot/jobs";
/// A job still `starting` this long after creation was interrupted.
const STALE_START: Duration = Duration::from_secs(120);

/// Run `$1` with its output in temporary files; print the exit code, both
/// sizes, then the last bytes of each, separated by a NUL line.
const RUN: &str = r#"cd "$HOME/work" 2>/dev/null || cd "$HOME"
o=$(mktemp); e=$(mktemp)
sh -c "$1" > "$o" 2> "$e"; c=$?
printf '%s\n%s\n%s\n' "$c" "$(wc -c < "$o")" "$(wc -c < "$e")"
tail -c "$2" "$o"; printf '\n\000\n'; tail -c "$2" "$e"
rm -f "$o" "$e""#;

/// Launch `$2` detached in its own session (its pid is the process group),
/// recording pid, exit status, and output in `$1`.
const LAUNCH: &str = r#"mkdir -p "$1" && { cd "$HOME/work" 2>/dev/null || cd "$HOME"; }
setsid sh -c 'sh -c "$2" > "$1/out" 2>&1; echo $? > "$1/exit"' sh "$1" "$2" > /dev/null 2>&1 < /dev/null &
echo $! > "$1/pid"; echo launched"#;

/// Report a job: `done <code>`, `running`, or `lost`, then the output tail.
const PROBE: &str = r#"if [ -f "$1/exit" ]; then echo "done $(cat "$1/exit")"; elif [ -f "$1/pid" ] && kill -0 "$(cat "$1/pid")" 2>/dev/null; then echo running; else echo lost; fi; tail -c 4000 "$1/out" 2>/dev/null"#;

/// Stop a job's whole process group and confirm it is gone. procps `kill`
/// takes a negative process group (the shell builtin does not). Prints
/// `stopped` or `alive`.
const KILL: &str = r#"pg=$(cat "$1/pid" 2>/dev/null) || { echo stopped; exit 0; }
alive() { pgrep -g "$pg" > /dev/null 2>&1; }
alive || { echo stopped; exit 0; }
/usr/bin/kill -s TERM -- "-$pg" 2>/dev/null
for i in 1 2 3 4 5 6 7 8 9 10; do alive || break; sleep 0.3; done
alive && /usr/bin/kill -s KILL -- "-$pg" 2>/dev/null
for i in 1 2 3 4 5 6 7 8 9 10; do alive || break; sleep 0.2; done
if alive; then echo alive; else [ -f "$1/exit" ] || echo 130 > "$1/exit"; echo stopped; fi"#;

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
    fn as_guest_user(&self, script: &str) -> (String, Vec<String>) {
        if self.config.desktop {
            (
                "runuser".into(),
                vec![
                    "-u".into(),
                    "elsebot".into(),
                    "--".into(),
                    "sh".into(),
                    "-c".into(),
                    script.into(),
                    "sh".into(),
                ],
            )
        } else {
            ("sh".into(), vec!["-c".into(), script.into(), "sh".into()])
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

    /// Run a command in the bot's computer under `holder`'s lease (taken
    /// if needed), in the guest user's work directory.
    pub async fn run_command(
        &self,
        bot_id: Uuid,
        holder: &str,
        command: &str,
        timeout: Duration,
    ) -> Result<Result<CommandOutput, Acquire>, ComputerError> {
        let acquired = self.acquire(bot_id, holder).await?;
        let Acquire::Ready { lease_id, .. } = acquired else {
            return Ok(Err(acquired));
        };
        let out = self
            .guest(lease_id, RUN, &[command, &TAIL_BYTES.to_string()], timeout)
            .await?;
        Ok(Ok(parse_run(&out).unwrap_or(CommandOutput {
            // The command was killed before reporting (its timeout).
            exit_code: None,
            stdout: String::new(),
            stderr: tail(&out.stderr, TAIL_BYTES),
            stdout_bytes: 0,
            stderr_bytes: out.stderr.len() as u64,
        })))
    }

    /// Start a background job with its own lease and deadline.
    pub async fn start_job(
        &self,
        bot_id: Uuid,
        turn_id: Option<Uuid>,
        command: &str,
        max_duration: Duration,
    ) -> Result<Started, ComputerError> {
        let id = Uuid::new_v4();
        let duration = max_duration.min(MAX_JOB_DURATION);
        // Reserve a slot atomically before anything else.
        {
            let mut tx = self.pool.begin().await?;
            sqlx::query("SELECT pg_advisory_xact_lock(hashtext('else_jobs:' || $1::text))")
                .bind(bot_id)
                .execute(&mut *tx)
                .await?;
            let running: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM guest_jobs WHERE bot_id = $1 AND status IN ('starting', 'running')",
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
                "INSERT INTO bot_computers (bot_id, runtime_name, image, cpus, memory_mib)
                 VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
            )
            .bind(bot_id)
            .bind(Self::runtime_name(bot_id))
            .bind(&self.config.image)
            .bind(self.config.cpus as i32)
            .bind(self.config.memory_mib as i32)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO guest_jobs (id, bot_id, turn_id, command, deadline)
                 VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5))",
            )
            .bind(id)
            .bind(bot_id)
            .bind(turn_id)
            .bind(command)
            .bind(duration.as_secs_f64())
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
        sqlx::query("UPDATE guest_jobs SET lease_id = $2 WHERE id = $1")
            .bind(id)
            .bind(lease_id)
            .execute(&self.pool)
            .await?;
        let dir = format!("{JOBS_DIR}/{id}");
        let launched = self
            .guest(lease_id, LAUNCH, &[&dir, command], Duration::from_secs(30))
            .await;
        if crate::faults::hit("job.after_launch") {
            return Err(ComputerError::Interrupted);
        }
        match launched {
            Ok(out) if out.stdout.contains("launched") => {
                sqlx::query("UPDATE guest_jobs SET status = 'running' WHERE id = $1 AND status = 'starting'")
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
            }
            Ok(out) => {
                self.fail_start(id, &out.stderr).await?;
                self.release(lease_id).await?;
            }
            Err(err) => {
                self.fail_start(id, &err.to_string()).await?;
                self.release(lease_id).await?;
            }
        }
        Ok(Started::Job(id))
    }

    async fn fail_start(&self, id: Uuid, error: &str) -> Result<(), ComputerError> {
        sqlx::query(
            "UPDATE guest_jobs SET status = 'failed', error = $2, finished_at = now()
             WHERE id = $1 AND status = 'starting'",
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
            sqlx::query_as("SELECT status, lease_id FROM guest_jobs WHERE id = $1 AND bot_id = $2")
                .bind(job_id)
                .bind(bot_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some((status, lease)) = found else {
            return Ok(None);
        };
        if let (true, Some(lease)) = (status == "running", lease) {
            self.probe_job(job_id, lease).await?;
        }
        self.load_job(job_id).await
    }

    async fn load_job(&self, job_id: Uuid) -> Result<Option<JobState>, ComputerError> {
        let row = sqlx::query(
            "SELECT id, status, exit_code, coalesce(output_tail, '') AS tail, command
             FROM guest_jobs WHERE id = $1",
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

    /// Ask the guest about a job and record what it reports.
    async fn probe_job(&self, job_id: Uuid, lease: Uuid) -> Result<(), ComputerError> {
        let dir = format!("{JOBS_DIR}/{job_id}");
        let out = match self
            .guest(lease, PROBE, &[&dir], Duration::from_secs(30))
            .await
        {
            Ok(out) => out,
            // The lease ended (owner stop, deletion) or the computer is
            // not running: the job did not survive.
            Err(ComputerError::LeaseEnded | ComputerError::NotRunning) => {
                return self.finish_job(job_id, "lost", None, "", lease).await;
            }
            Err(err) => return Err(err),
        };
        let (first, rest) = out
            .stdout
            .split_once('\n')
            .unwrap_or((out.stdout.as_str(), ""));
        let output = tail(rest, 4000);
        if let Some(code) = first.strip_prefix("done ") {
            let code = code.trim().parse::<i32>().ok();
            let status = if code == Some(0) {
                "completed"
            } else {
                "failed"
            };
            self.finish_job(job_id, status, code, &output, lease).await
        } else if first == "lost" {
            self.finish_job(job_id, "lost", None, &output, lease).await
        } else {
            sqlx::query(
                "UPDATE guest_jobs SET output_tail = $2 WHERE id = $1 AND status = 'running'",
            )
            .bind(job_id)
            .bind(output)
            .execute(&self.pool)
            .await?;
            Ok(())
        }
    }

    async fn finish_job(
        &self,
        job_id: Uuid,
        status: &str,
        code: Option<i32>,
        output: &str,
        lease: Uuid,
    ) -> Result<(), ComputerError> {
        sqlx::query(
            "UPDATE guest_jobs SET status = $2, exit_code = $3, output_tail = $4, finished_at = now()
             WHERE id = $1 AND status IN ('starting', 'running', 'cancelling')",
        )
        .bind(job_id)
        .bind(status)
        .bind(code)
        .bind(output)
        .execute(&self.pool)
        .await?;
        self.release(lease).await
    }

    /// Stop a job (its whole process group). Recorded as cancelled only
    /// once the guest confirms nothing of it runs; otherwise it stays
    /// running with its lease, and the error says so.
    pub async fn cancel_job(
        &self,
        bot_id: Uuid,
        job_id: Uuid,
    ) -> Result<Option<JobState>, ComputerError> {
        let found: Option<(String, Option<Uuid>)> =
            sqlx::query_as("SELECT status, lease_id FROM guest_jobs WHERE id = $1 AND bot_id = $2")
                .bind(job_id)
                .bind(bot_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some((status, lease)) = found else {
            return Ok(None);
        };
        let live = matches!(status.as_str(), "starting" | "running" | "cancelling");
        if let (true, Some(lease)) = (live, lease) {
            let dir = format!("{JOBS_DIR}/{job_id}");
            match self
                .guest(lease, KILL, &[&dir], Duration::from_secs(30))
                .await
            {
                Ok(out) if out.stdout.trim_end().ends_with("stopped") => {
                    self.finish_job(job_id, "cancelled", Some(130), "", lease)
                        .await?;
                }
                Err(ComputerError::LeaseEnded | ComputerError::NotRunning) => {
                    self.finish_job(job_id, "lost", None, "", lease).await?;
                }
                Ok(out) if status == "cancelling" && out.stdout.trim().is_empty() => {}
                Ok(out) => {
                    sqlx::query("UPDATE guest_jobs SET error = $2 WHERE id = $1")
                        .bind(job_id)
                        .bind(super::bounded(
                            &format!("cancellation not confirmed: {}", out.stdout.trim()),
                            2000,
                        ))
                        .execute(&self.pool)
                        .await?;
                }
                Err(err) => return Err(err),
            }
        }
        self.load_job(job_id).await
    }

    /// Maintenance: refresh running jobs, enforce deadlines, resolve jobs
    /// interrupted while starting, release the leases of jobs that ended.
    /// A failure on one job does not stop the others.
    pub(super) async fn poll_jobs(&self, cancel: &CancellationToken) -> Result<(), ComputerError> {
        let jobs: Vec<(Uuid, Uuid, Option<Uuid>, String, bool)> = sqlx::query_as(
            "SELECT id, bot_id, lease_id, status, deadline < now() FROM guest_jobs
             WHERE status IN ('running', 'cancelling')
                OR (status = 'starting' AND started_at < now() - make_interval(secs => $1))",
        )
        .bind(STALE_START.as_secs_f64())
        .fetch_all(&self.pool)
        .await?;
        for (job, bot, lease, status, overdue) in jobs {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let result = async {
                let Some(lease) = lease else {
                    // Interrupted before it had a lease: nothing launched.
                    return self.fail_start(job, "interrupted before launch").await;
                };
                if status == "cancelling" {
                    // A bot-wide Stop work or deletion: kill it, then let
                    // the stop complete if nothing else remains.
                    self.cancel_job(bot, job).await?;
                    let mut tx = self.pool.begin().await?;
                    sqlx::query("SELECT 1 FROM bots WHERE id = $1 FOR UPDATE")
                        .bind(bot)
                        .execute(&mut *tx)
                        .await?;
                    crate::db::stop::settle(&mut tx, bot).await?;
                    tx.commit().await?;
                    Ok(())
                } else if overdue {
                    sqlx::query("UPDATE guest_jobs SET error = 'deadline exceeded' WHERE id = $1")
                        .bind(job)
                        .execute(&self.pool)
                        .await?;
                    self.cancel_job(bot, job).await.map(|_| ())
                } else if status == "starting" {
                    // Interrupted mid-launch: look at what the guest has.
                    sqlx::query("UPDATE guest_jobs SET status = 'running' WHERE id = $1 AND status = 'starting'")
                        .bind(job)
                        .execute(&self.pool)
                        .await?;
                    self.probe_job(job, lease).await
                } else {
                    self.probe_job(job, lease).await
                }
            }
            .await;
            if let Err(err) = result {
                tracing::warn!(job = %job, error = %err, "polling a guest job failed");
            }
        }
        Ok(())
    }

    /// Copy a regular guest file into the host artifact store (at most
    /// `max` bytes) and register it. Checked and copied under the bot's
    /// operation lock with the lease revalidated, so a stop cannot slip in
    /// (copying would wake a stopped computer). Anything over the limit, not
    /// a regular file, or failing midway is removed again.
    pub async fn fetch_artifact(
        &self,
        lease: Uuid,
        bot_id: Uuid,
        turn_id: Option<Uuid>,
        guest_path: &str,
        max: u64,
    ) -> Result<Result<(Uuid, u64, String, String), String>, ComputerError> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let (lease_bot, name, _op) = self.checked(lease).await?;
        if lease_bot != bot_id {
            return Err(ComputerError::LeaseEnded);
        }
        let (program, mut args) =
            self.as_guest_user(r#"[ -f "$1" ] && [ ! -L "$1" ] && stat -c %s -- "$1""#);
        args.push(guest_path.to_owned());
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let size = self
            .runtime
            .exec(&name, &program, &argv, Duration::from_secs(30))
            .await?;
        let Ok(bytes) = size.stdout.trim().parse::<u64>() else {
            return Ok(Err("not a regular file (or not readable)".into()));
        };
        if bytes > max {
            return Ok(Err(format!(
                "the file is {bytes} bytes; the limit is {max}"
            )));
        }
        let store = self
            .config
            .state_dir
            .join("artifacts")
            .join(bot_id.to_string());
        let io = |e: std::io::Error| {
            ComputerError::Runtime(super::RuntimeError::Unavailable(e.to_string()))
        };
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&store)
                .map_err(io)?;
        }
        let id = Uuid::new_v4();
        let temp = store.join(format!(".incoming-{id}"));
        let final_path = store.join(id.to_string());
        let remove = |path: &std::path::Path| {
            let _ = std::fs::remove_dir_all(path);
            let _ = std::fs::remove_file(path);
        };
        if let Err(err) = self.runtime.copy_out(&name, guest_path, &temp).await {
            remove(&temp);
            return Err(err.into());
        }
        // Verify what arrived: a regular file within the limit, hashed in
        // a stream rather than loaded whole.
        let checked = (|| -> std::io::Result<Result<(u64, Vec<u8>, Vec<u8>), String>> {
            let meta = std::fs::symlink_metadata(&temp)?;
            if !meta.is_file() {
                return Ok(Err("the copy was not a regular file".into()));
            }
            if meta.len() > max {
                return Ok(Err("the file grew past the limit while copying".into()));
            }
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
            let mut file = std::fs::File::open(&temp)?;
            let mut hasher = Sha256::new();
            let mut preview = Vec::new();
            let mut total = 0u64;
            let mut chunk = [0u8; 64 * 1024];
            loop {
                let n = file.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                total += n as u64;
                if total > max {
                    return Ok(Err("the file grew past the limit while copying".into()));
                }
                hasher.update(&chunk[..n]);
                if preview.len() < 2000 {
                    let take = (2000 - preview.len()).min(n);
                    preview.extend_from_slice(&chunk[..take]);
                }
            }
            Ok(Ok((total, hasher.finalize().to_vec(), preview)))
        })();
        let (total, digest, preview) = match checked {
            Ok(Ok(found)) => found,
            Ok(Err(reason)) => {
                remove(&temp);
                return Ok(Err(reason));
            }
            Err(err) => {
                remove(&temp);
                return Err(io(err));
            }
        };
        std::fs::rename(&temp, &final_path).map_err(|e| {
            remove(&temp);
            io(e)
        })?;
        sqlx::query(
            "INSERT INTO artifacts (id, bot_id, turn_id, guest_path, size_bytes, sha256)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(bot_id)
        .bind(turn_id)
        .bind(guest_path)
        .bind(total as i64)
        .bind(&digest)
        .execute(&self.pool)
        .await?;
        let preview = String::from_utf8_lossy(&preview).into_owned();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Ok((id, total, hex, preview)))
    }

    /// Run the guest browser helper (`else-browser`).
    pub async fn browser(&self, lease: Uuid, args: &[&str]) -> Result<ExecOutput, ComputerError> {
        self.guest(
            lease,
            r#"DISPLAY=:99 exec /usr/local/bin/else-browser "$@""#,
            args,
            Duration::from_secs(60),
        )
        .await
    }

    /// Write a text file in the guest (via base64, no shell quoting).
    pub async fn write_file(
        &self,
        lease: Uuid,
        path: &str,
        content: &str,
    ) -> Result<ExecOutput, ComputerError> {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
        self.guest(
            lease,
            r#"cd "$HOME/work" 2>/dev/null || cd "$HOME"; mkdir -p -- "$(dirname -- "$1")" && printf %s "$2" | base64 -d > "$1""#,
            &[path, &encoded],
            Duration::from_secs(30),
        )
        .await
    }

    /// Read `length` bytes of a guest file from `offset`, exactly: returns
    /// the file size and the raw bytes (sent as base64).
    pub async fn read_file(
        &self,
        lease: Uuid,
        path: &str,
        offset: u64,
        length: u64,
    ) -> Result<Result<(u64, Vec<u8>), String>, ComputerError> {
        use base64::Engine;
        let offset_arg = offset.to_string();
        let length_arg = length.to_string();
        let out = self
            .guest(
                lease,
                r#"cd "$HOME/work" 2>/dev/null || cd "$HOME"; [ -f "$1" ] || { echo "not a regular file" >&2; exit 1; }
stat -c %s -- "$1" && tail -c +"$(($2 + 1))" -- "$1" | head -c "$3" | base64 -w0"#,
                &[path, &offset_arg, &length_arg],
                Duration::from_secs(30),
            )
            .await?;
        if out.status != Some(0) {
            return Ok(Err(out.stderr.trim().chars().take(500).collect()));
        }
        let (size, encoded) = out.stdout.split_once('\n').unwrap_or(("0", ""));
        let size = size.trim().parse().unwrap_or(0);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|e| ComputerError::Runtime(super::RuntimeError::Unavailable(e.to_string())))?;
        Ok(Ok((size, bytes)))
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
