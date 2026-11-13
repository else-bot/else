//! Starting a background job: reserve its slot, hold the computer under the
//! job's own lease, and launch it in the guest.

use std::time::Duration;

use uuid::Uuid;

use super::{End, FILTER, JOBS_DIR, MAX_JOB_DURATION, MAX_JOBS, Started, env_input, with_env};
use crate::computers::{Acquire, ComputerError, ComputerManager};
use crate::db::task_secrets::{Env, redact};

const LAUNCH: &str = r#"mkdir -p "$1" && { cd "$HOME/work" 2>/dev/null || cd "$HOME"; }
@ENV@
cat /proc/sys/kernel/random/boot_id > "$1/boot" 2>/dev/null
if [ -n "$ELSE_REDACT_VARS" ]; then
  /usr/bin/setsid /bin/sh -c '{ PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH" /bin/sh -c "$2" < /dev/null; echo $? > "$1/code"; } 2>&1 | /usr/bin/python3 -I -S -c "$3" > "$1/out"; mv "$1/code" "$1/exit"' sh "$1" "$2" "$3" > /dev/null 2>&1 < /dev/null &
else
  /usr/bin/setsid /bin/sh -c 'PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH" /bin/sh -c "$2" > "$1/out" 2>&1 < /dev/null; echo $? > "$1/exit"' sh "$1" "$2" > /dev/null 2>&1 < /dev/null &
fi
echo $! > "$1/pid"; echo launched"#;

enum Launch {
    Launched,
    NotLaunched(String),
    Uncertain(String),
    Interrupted,
}

impl ComputerManager {
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
                    .bind(crate::computers::bounded(
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
        self.end_job(id, End::StartFailed(error)).await
    }
}
