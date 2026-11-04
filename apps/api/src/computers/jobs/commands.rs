//! Foreground command execution and interruption.

use super::{
    Acquire, CommandOutput, ComputerError, ComputerManager, Env, FILTER, Foreground, Inspect, KILL,
    REDACT_MARGIN, RUN, RUNS_DIR, TAIL_BYTES, env_input, parse_run, redact, tail, with_env,
};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

impl ComputerManager {
    /// Run a command under a lease, acquired on first use, in the guest user's work directory.
    pub async fn run_command(
        &self,
        bot_id: Uuid,
        holder: &str,
        command: &str,
        timeout: Duration,
    ) -> Result<Result<CommandOutput, Acquire>, ComputerError> {
        self.run_command_with(bot_id, holder, command, timeout, &Env::default())
            .await
    }

    /// [`Self::run_command`] with task-secret variables (POC-026), sent on
    /// the runtime's stdin, and its output filtered in the guest and
    /// redacted again here (before the tail is cut).
    ///
    /// The command holds the bot's operation lock from before it is
    /// recorded until after it is retired, so its computer runs throughout
    /// and a kill aimed at it can never reach a computer stopped since. It
    /// is recorded in `guest_runs` (Stop work and deletion settle only once
    /// it ended) and in memory, where maintenance ends it, with its whole
    /// process group, once the turn that started it is no longer running.
    /// A command whose runtime call failed or was cut off by its timeout is
    /// killed, and confirmed gone, before it is retired. Callers that may be
    /// cancelled run this in a task of its own (the tool layer does), so a
    /// dropped request never leaves the lock or the record behind.
    pub async fn run_command_with(
        &self,
        bot_id: Uuid,
        holder: &str,
        command: &str,
        timeout: Duration,
        env: &Env,
    ) -> Result<Result<CommandOutput, Acquire>, ComputerError> {
        self.run_foreground(bot_id, holder, command, timeout, env, false)
            .await
    }

    /// A desktop input batch (POC-029): a foreground command that, under the
    /// operation lock, also requires that the owner's desktop session is not
    /// open (desktop admission takes the same lock, so they exclude each
    /// other).
    pub async fn run_input(
        &self,
        bot_id: Uuid,
        holder: &str,
        command: &str,
        timeout: Duration,
        env: &Env,
    ) -> Result<Result<CommandOutput, Acquire>, ComputerError> {
        self.run_foreground(bot_id, holder, command, timeout, env, true)
            .await
    }

    async fn run_foreground(
        &self,
        bot_id: Uuid,
        holder: &str,
        command: &str,
        timeout: Duration,
        env: &Env,
        exclusive_of_owner: bool,
    ) -> Result<Result<CommandOutput, Acquire>, ComputerError> {
        let acquired = self.acquire(bot_id, holder).await?;
        let Acquire::Ready { lease_id, .. } = acquired else {
            return Ok(Err(acquired));
        };
        let (bot, runtime_name, op) = self.checked(lease_id).await?;
        // Decided again under the lock: a request that waited behind other
        // work must not run after its turn was stopped, or (for input) once
        // the owner opened the desktop.
        let turn: Option<Uuid> = holder.strip_prefix("turn:").and_then(|t| t.parse().ok());
        if let Some(turn) = turn {
            let live: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM turns WHERE id = ?1
                                AND status IN ('preparing', 'sending', 'running'))",
            )
            .bind(turn)
            .fetch_one(&self.pool)
            .await?;
            if !live {
                return Err(ComputerError::Refused(
                    "STOPPED: this turn was stopped; nothing was run.".into(),
                ));
            }
        }
        if exclusive_of_owner {
            let owner: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM computer_leases WHERE bot_id = ?1 AND holder = ?2
                                AND released_at IS NULL AND expires_at > strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))",
            )
            .bind(bot)
            .bind(format!("desktop:{bot}"))
            .fetch_one(&self.pool)
            .await?;
            if owner {
                return Err(ComputerError::Refused(
                    "OWNER_USING_DESKTOP: the owner has your desktop open; nothing was done. \
                     Wait until they close it."
                        .into(),
                ));
            }
        }
        let run_id = Uuid::new_v4();
        let dir = format!("{RUNS_DIR}/{run_id}");
        let turn_id: Option<Uuid> = holder.strip_prefix("turn:").and_then(|t| t.parse().ok());
        sqlx::query(
            "INSERT INTO guest_runs (id, bot_id, turn_id, runtime_name)
             SELECT ?1, ?2, t.id, ?4 FROM (SELECT ?3 AS want) w
             LEFT JOIN turns t ON t.id = w.want",
        )
        .bind(run_id)
        .bind(bot)
        .bind(turn_id)
        .bind(&runtime_name)
        .execute(&self.pool)
        .await?;
        let alive = crate::actor::Actor::new(true);
        let registered_name = runtime_name.clone();
        let registered_dir = dir.clone();
        let registered_alive = alive.clone();
        self.foreground
            .call(move |foreground| {
                foreground.insert(
                    run_id,
                    Foreground {
                        bot_id,
                        turn_id,
                        runtime_name: registered_name,
                        dir: registered_dir,
                        alive: registered_alive,
                    },
                )
            })
            .await;
        let script = with_env(RUN);
        let tail_bytes = (TAIL_BYTES + REDACT_MARGIN).to_string();
        let (program, mut all) = self.as_guest_user(&script);
        all.extend([
            command.to_owned(),
            tail_bytes,
            dir.clone(),
            FILTER.to_owned(),
        ]);
        let all: Vec<&str> = all.iter().map(String::as_str).collect();
        let out = if env.vars.is_empty() {
            self.runtime
                .exec(&runtime_name, &program, &all, timeout)
                .await
        } else {
            self.runtime
                .exec_input(&runtime_name, &program, &all, &env_input(env), timeout)
                .await
        };
        let parsed = out.as_ref().ok().and_then(parse_run);
        let mut end_reason = "returned";
        if parsed.is_none() {
            // Cut off (its timeout) or the reply was lost: the group may
            // still run. End it before retiring the record.
            end_reason = "killed after timeout";
            let (kill_program, mut kill_args) = self.as_guest_user(KILL);
            kill_args.push(dir.clone());
            let kill_args: Vec<&str> = kill_args.iter().map(String::as_str).collect();
            let killed = self
                .runtime
                .exec(
                    &runtime_name,
                    &kill_program,
                    &kill_args,
                    Duration::from_secs(30),
                )
                .await;
            if !killed.as_ref().is_ok_and(|k| k.stdout.contains("stopped")) {
                end_reason = "kill unconfirmed";
                tracing::warn!(bot = %bot_id, run = %run_id, "a timed-out command's group may survive");
            }
        }
        // Retired while the operation lock is still held.
        alive.call(|alive| *alive = false).await;
        self.foreground
            .call(move |foreground| foreground.remove(&run_id))
            .await;
        if end_reason != "kill unconfirmed" {
            sqlx::query("UPDATE guest_runs SET ended_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), end_reason = ?2 WHERE id = ?1")
                .bind(run_id)
                .bind(end_reason)
                .execute(&self.pool)
                .await?;
        }
        drop(op);
        sqlx::query("UPDATE bot_computers SET last_used_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE bot_id = ?1")
            .bind(bot)
            .execute(&self.pool)
            .await?;
        let out = out?;
        let redaction = env.redaction();
        let mut result = parsed.unwrap_or(CommandOutput {
            // The command was killed before reporting (its timeout).
            exit_code: None,
            stdout: String::new(),
            stderr: tail(&out.stderr, TAIL_BYTES),
            stdout_bytes: 0,
            stderr_bytes: out.stderr.len() as u64,
        });
        result.stdout = tail(&redact(&result.stdout, &redaction), TAIL_BYTES);
        result.stderr = tail(&redact(&result.stderr, &redaction), TAIL_BYTES);
        Ok(Ok(result))
    }

    /// End foreground commands whose turn is no longer running (called by
    /// maintenance every 2 seconds). A command this process runs is killed
    /// without the operation lock, which the command itself holds (so its
    /// computer runs), and only while it is still registered as alive. A
    /// command an earlier API process left (recorded, not registered) is
    /// killed under the operation lock if its computer is running, and only
    /// in the boot it started in. Each kill runs in its own task: one
    /// unresponsive computer delays no other bot's. The pass waits up to
    /// 10 seconds for its kills; slower ones continue and are not repeated.
    pub async fn end_orphaned_commands(self: &Arc<Self>) -> Result<(), ComputerError> {
        let open: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
            "SELECT r.id, r.bot_id, r.runtime_name FROM guest_runs r
             LEFT JOIN turns t ON t.id = r.turn_id
             JOIN bots b ON b.id = r.bot_id
             LEFT JOIN bot_computers c ON c.bot_id = r.bot_id
             WHERE r.ended_at IS NULL
               AND NOT (t.status IS NOT NULL AND t.status IN ('preparing', 'sending', 'running')
                        AND b.status = 'active' AND coalesce(c.desired_state, 'auto') = 'auto')",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut tasks = Vec::new();
        for (run_id, bot_id, runtime_name) in open {
            let registered = self
                .foreground
                .call(move |foreground| foreground.get(&run_id).cloned())
                .await;
            // A command of this process without a turn (an operator or test
            // holder) is not the orphan loop's to end.
            if registered.as_ref().is_some_and(|f| f.turn_id.is_none()) {
                continue;
            }
            if !self
                .killing
                .call(move |killing| killing.insert(run_id))
                .await
            {
                continue;
            }
            let manager = self.clone();
            tasks.push(tokio::spawn(async move {
                let dir = format!("{RUNS_DIR}/{run_id}");
                let outcome = match registered {
                    Some(run) => manager.kill_registered(&run).await,
                    None => manager.kill_left_over(bot_id, &runtime_name, &dir).await,
                };
                match outcome {
                    Ok(Some(reason)) => {
                        tracing::info!(bot = %bot_id, run = %run_id, reason, "ended a foreground command whose turn stopped");
                        if reason != "still running" {
                            let _ = sqlx::query(
                                "UPDATE guest_runs SET ended_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), end_reason = ?2
                                 WHERE id = ?1 AND ended_at IS NULL",
                            )
                            .bind(run_id)
                            .bind(reason)
                            .execute(&manager.pool)
                            .await;
                        }
                    }
                    Ok(None) => {}
                    Err(err) => tracing::warn!(
                        bot = %bot_id, run = %run_id, error = %err,
                        "ending a foreground command failed; retried next pass"
                    ),
                }
                manager.killing.call(move |killing| killing.remove(&run_id)).await;
            }));
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            for task in tasks {
                let _ = task.await;
            }
        })
        .await;
        Ok(())
    }

    /// End every foreground command of a bot that this process runs (an
    /// owner stop: it must not wait behind them), without the operation
    /// lock, which they hold.
    pub async fn end_bot_commands(&self, bot_id: Uuid) {
        self.end_commands(move |run, _| run.bot_id == bot_id).await;
    }

    /// End every foreground command of a turn (its caller went away).
    pub async fn end_turn_commands(&self, turn_id: Uuid) {
        self.end_commands(move |run, _| run.turn_id == Some(turn_id))
            .await;
    }

    async fn end_commands(&self, matches: impl Fn(&Foreground, Uuid) -> bool + Send + 'static) {
        let runs: Vec<(Uuid, Foreground)> = self
            .foreground
            .call(move |foreground| {
                foreground
                    .iter()
                    .filter(|(id, run)| matches(run, **id))
                    .map(|(id, run)| (*id, run.clone()))
                    .collect()
            })
            .await;
        for (id, run) in runs {
            if let Err(err) = self.kill_registered(&run).await {
                tracing::warn!(run = %id, error = %err, "ending a foreground command failed");
            }
        }
    }

    /// Kill a command this process runs. `None` when it already returned
    /// (its own path retires it).
    async fn kill_registered(
        &self,
        run: &Foreground,
    ) -> Result<Option<&'static str>, ComputerError> {
        let run = run.clone();
        let runtime = self.runtime.clone();
        let (program, mut all) = self.as_guest_user(KILL);
        all.push(run.dir.clone());
        run.alive
            .call_async(move |alive| {
                Box::pin(async move {
                    if !*alive {
                        return Ok(None);
                    }
                    let all: Vec<&str> = all.iter().map(String::as_str).collect();
                    let out = runtime
                        .exec(&run.runtime_name, &program, &all, Duration::from_secs(30))
                        .await?;
                    // The command's own path records the end once its call returns.
                    Ok(if out.stdout.contains("stopped") {
                        None
                    } else {
                        Some("still running")
                    })
                })
            })
            .await
    }

    /// Kill a command an earlier API process left, under the operation lock.
    async fn kill_left_over(
        &self,
        bot_id: Uuid,
        runtime_name: &str,
        dir: &str,
    ) -> Result<Option<&'static str>, ComputerError> {
        let _op = self.op(bot_id).await;
        let current: Option<String> =
            sqlx::query_scalar("SELECT runtime_name FROM bot_computers WHERE bot_id = ?1")
                .bind(bot_id)
                .fetch_optional(&self.pool)
                .await?;
        if current.as_deref() != Some(runtime_name) {
            return Ok(Some("computer replaced"));
        }
        Ok(match self.inspect_held(bot_id, KILL, &[dir]).await? {
            Inspect::Gone => Some("computer stopped"),
            Inspect::Out(out) if out.stdout.contains("stopped") => Some("killed after restart"),
            Inspect::Out(_) => Some("still running"),
            Inspect::Paused | Inspect::Unknown => None,
        })
    }
}
