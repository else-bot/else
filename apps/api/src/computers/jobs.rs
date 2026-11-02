//! Commands and background jobs in a bot's computer (POC-018).
//!
//! A foreground command runs under the caller's lease and returns bounded
//! output. A background job gets an ID, its own `job:<id>` lease (so it
//! keeps the computer running after the provider turn that started it), a
//! deadline, and guest-side files under `/var/lib/elsebot/jobs/<id>`
//! (`pid`, `exit`, `out`). Maintenance polls running jobs, records their
//! outcome, enforces deadlines, and releases their leases. Commands run as
//! the guest's unprivileged user in its work directory.

use std::time::Duration;

use sqlx::Row as _;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{Acquire, ComputerError, ComputerManager, ExecOutput};

/// Running background jobs per bot.
pub const MAX_JOBS: i64 = 4;
pub const MAX_JOB_DURATION: Duration = Duration::from_secs(6 * 3600);
const JOBS_DIR: &str = "/var/lib/elsebot/jobs";

/// Launch `$2` detached in its own session, recording pid, exit status, and
/// output in `$1`.
const LAUNCH: &str = r#"mkdir -p "$1" && cd "$HOME/work" 2>/dev/null || cd "$HOME"; setsid sh -c 'sh -c "$2" > "$1/out" 2>&1; echo $? > "$1/exit"' sh "$1" "$2" > /dev/null 2>&1 < /dev/null & echo $! > "$1/pid"; echo launched"#;

/// Report a job: `done <code>`, `running`, or `lost`, then the output tail.
const PROBE: &str = r#"if [ -f "$1/exit" ]; then echo "done $(cat "$1/exit")"; elif [ -f "$1/pid" ] && kill -0 "$(cat "$1/pid")" 2>/dev/null; then echo running; else echo lost; fi; tail -c 4000 "$1/out" 2>/dev/null"#;

const KILL: &str = r#"[ -f "$1/pid" ] && kill -TERM -- "-$(cat "$1/pid")" 2>/dev/null; sleep 1; [ -f "$1/pid" ] && kill -KILL -- "-$(cat "$1/pid")" 2>/dev/null; echo 130 > "$1/exit"; echo killed"#;

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

fn tail(text: &str, max: usize) -> String {
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

impl ComputerManager {
    /// The guest user and its program prefix for running commands.
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
    ) -> Result<Result<ExecOutput, Acquire>, ComputerError> {
        let acquired = self.acquire(bot_id, holder).await?;
        let Acquire::Ready { lease_id, .. } = acquired else {
            return Ok(Err(acquired));
        };
        let out = self
            .guest(
                lease_id,
                r#"cd "$HOME/work" 2>/dev/null || cd "$HOME"; sh -c "$1""#,
                &[command],
                timeout,
            )
            .await?;
        Ok(Ok(out))
    }

    /// Start a background job with its own lease and deadline.
    pub async fn start_job(
        &self,
        bot_id: Uuid,
        turn_id: Option<Uuid>,
        command: &str,
        max_duration: Duration,
    ) -> Result<Started, ComputerError> {
        let running: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM guest_jobs WHERE bot_id = $1 AND status IN ('starting', 'running')",
        )
        .bind(bot_id)
        .fetch_one(&self.pool)
        .await?;
        if running >= MAX_JOBS {
            return Ok(Started::TooMany);
        }
        let id = Uuid::new_v4();
        let acquired = self.acquire(bot_id, &format!("job:{id}")).await?;
        let Acquire::Ready { lease_id, .. } = acquired else {
            return Ok(Started::NotReady(acquired));
        };
        let duration = max_duration.min(MAX_JOB_DURATION);
        // Recorded before dispatch: a crash leaves a `starting` job that
        // maintenance resolves by probing, never a silent second launch.
        sqlx::query(
            "INSERT INTO guest_jobs (id, bot_id, turn_id, command, lease_id, deadline)
             VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6))",
        )
        .bind(id)
        .bind(bot_id)
        .bind(turn_id)
        .bind(command)
        .bind(lease_id)
        .bind(duration.as_secs_f64())
        .execute(&self.pool)
        .await?;
        let dir = format!("{JOBS_DIR}/{id}");
        let launched = self
            .guest(lease_id, LAUNCH, &[&dir, command], Duration::from_secs(30))
            .await;
        let ok = matches!(&launched, Ok(out) if out.stdout.contains("launched"));
        sqlx::query(
            "UPDATE guest_jobs SET status = $2, error = $3,
                 finished_at = CASE WHEN $2 = 'failed' THEN now() END
             WHERE id = $1",
        )
        .bind(id)
        .bind(if ok { "running" } else { "failed" })
        .bind(match &launched {
            Ok(out) if !ok => Some(super::bounded(&out.stderr, 2000)),
            Err(err) => Some(super::bounded(&err.to_string(), 2000)),
            _ => None,
        })
        .execute(&self.pool)
        .await?;
        if !ok {
            self.release(lease_id).await?;
        }
        Ok(Started::Job(id))
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
        if matches!(status.as_str(), "starting" | "running") {
            if let Some(lease) = lease {
                self.probe_job(job_id, lease).await?;
            }
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

    /// Ask the guest about a running job and record what it reports.
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
            sqlx::query("UPDATE guest_jobs SET status = 'running', output_tail = $2 WHERE id = $1")
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
             WHERE id = $1 AND status IN ('starting', 'running')",
        )
        .bind(job_id)
        .bind(status)
        .bind(code)
        .bind(output)
        .execute(&self.pool)
        .await?;
        self.release(lease).await
    }

    /// Stop a job (its whole process group).
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
        if let (true, Some(lease)) = (matches!(status.as_str(), "starting" | "running"), lease) {
            let dir = format!("{JOBS_DIR}/{job_id}");
            let _ = self
                .guest(lease, KILL, &[&dir], Duration::from_secs(30))
                .await;
            self.finish_job(job_id, "cancelled", Some(130), "", lease)
                .await?;
        }
        self.load_job(job_id).await
    }

    /// Maintenance: refresh running jobs, enforce deadlines, release the
    /// leases of jobs that ended.
    pub(super) async fn poll_jobs(&self, cancel: &CancellationToken) -> Result<(), ComputerError> {
        let running: Vec<(Uuid, Uuid, Option<Uuid>, bool)> = sqlx::query_as(
            "SELECT id, bot_id, lease_id, deadline < now() FROM guest_jobs
             WHERE status IN ('starting', 'running')",
        )
        .fetch_all(&self.pool)
        .await?;
        for (job, bot, lease, overdue) in running {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let Some(lease) = lease else { continue };
            if overdue {
                self.cancel_job(bot, job).await?;
                sqlx::query("UPDATE guest_jobs SET status = 'failed', error = 'deadline exceeded' WHERE id = $1")
                    .bind(job)
                    .execute(&self.pool)
                    .await?;
            } else {
                self.probe_job(job, lease).await?;
            }
        }
        Ok(())
    }

    /// Copy a guest file into the host artifact store (at most `max` bytes)
    /// and register it. Returns (artifact ID, size, SHA-256, a text preview).
    pub async fn fetch_artifact(
        &self,
        lease: Uuid,
        bot_id: Uuid,
        turn_id: Option<Uuid>,
        guest_path: &str,
        max: u64,
    ) -> Result<Result<(Uuid, u64, String, String), String>, ComputerError> {
        use sha2::{Digest, Sha256};
        let size = self
            .guest(
                lease,
                r#"stat -c %s -- "$1""#,
                &[guest_path],
                Duration::from_secs(30),
            )
            .await?;
        let Ok(bytes) = size.stdout.trim().parse::<u64>() else {
            return Ok(Err("no such file".into()));
        };
        if bytes > max {
            return Ok(Err(format!(
                "the file is {bytes} bytes; the limit is {max}"
            )));
        }
        let name: String =
            sqlx::query_scalar("SELECT runtime_name FROM bot_computers WHERE bot_id = $1")
                .bind(bot_id)
                .fetch_one(&self.pool)
                .await?;
        let id = Uuid::new_v4();
        let dir = self
            .config
            .state_dir
            .join("artifacts")
            .join(bot_id.to_string());
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .map_err(|e| {
                    ComputerError::Runtime(super::RuntimeError::Unavailable(e.to_string()))
                })?;
        }
        let path = dir.join(id.to_string());
        {
            let _op = self.op(bot_id).await;
            self.runtime.copy_out(&name, guest_path, &path).await?;
        }
        let data = std::fs::read(&path)
            .map_err(|e| ComputerError::Runtime(super::RuntimeError::Unavailable(e.to_string())))?;
        if data.len() as u64 > max {
            let _ = std::fs::remove_file(&path);
            return Ok(Err("the file grew past the limit while copying".into()));
        }
        let digest = Sha256::digest(&data).to_vec();
        sqlx::query(
            "INSERT INTO artifacts (id, bot_id, turn_id, guest_path, size_bytes, sha256)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(bot_id)
        .bind(turn_id)
        .bind(guest_path)
        .bind(data.len() as i64)
        .bind(&digest)
        .execute(&self.pool)
        .await?;
        let preview = String::from_utf8_lossy(&data[..data.len().min(2000)]).into_owned();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Ok((id, data.len() as u64, hex, preview)))
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
            r#"mkdir -p -- "$(dirname -- "$1")" && printf %s "$2" | base64 -d > "$1""#,
            &[path, &encoded],
            Duration::from_secs(30),
        )
        .await
    }

    /// Read a slice of a guest file.
    pub async fn read_file(
        &self,
        lease: Uuid,
        path: &str,
        offset: u64,
        length: u64,
    ) -> Result<ExecOutput, ComputerError> {
        let offset = offset.to_string();
        let length = length.to_string();
        self.guest(
            lease,
            r#"stat -c %s -- "$1" && tail -c +"$(($2 + 1))" -- "$1" | head -c "$3""#,
            &[path, &offset, &length],
            Duration::from_secs(30),
        )
        .await
    }
}
