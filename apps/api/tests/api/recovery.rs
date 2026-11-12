use crate::common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use common::TestApp;
use else_api::computers::jobs::{JobStatus, Started};
use else_api::computers::{
    Acquire, ComputerConfig, ComputerError, ComputerManager, ComputerRuntime, ExecOutput,
    RuntimeError, RuntimeState, Spec,
};
use else_api::faults;
use serde_json::json;
use uuid::Uuid;

#[derive(Default)]
struct Guest {
    computers: Mutex<HashMap<String, RuntimeState>>,
    jobs: Mutex<HashMap<String, Option<i32>>>,
    launches: Mutex<Vec<String>>,
    kills: Mutex<Vec<String>>,
    fail_list: std::sync::atomic::AtomicBool,
    create_delay: Mutex<Duration>,
    lose_launch_reply: AtomicBool,
    kills_fail: AtomicBool,
    stops_fail: AtomicBool,
}

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

impl Guest {
    fn finish(&self, id: Uuid, code: i32) {
        for (dir, state) in self.jobs.lock().unwrap().iter_mut() {
            if dir.ends_with(&id.to_string()) {
                *state = Some(code);
            }
        }
    }
    fn launches(&self) -> usize {
        self.launches.lock().unwrap().len()
    }
    fn set(&self, name: &str, state: RuntimeState) -> Result<(), RuntimeError> {
        match self.computers.lock().unwrap().get_mut(name) {
            Some(s) => {
                *s = state;
                Ok(())
            }
            None => Err(RuntimeError::Failed {
                operation: "control".into(),
                message: "missing".into(),
            }),
        }
    }
}

fn out(stdout: impl Into<String>) -> ExecOutput {
    ExecOutput {
        status: Some(0),
        stdout: stdout.into(),
        stderr: String::new(),
        truncated: false,
    }
}

#[async_trait]
impl ComputerRuntime for Guest {
    async fn list(&self) -> Result<Vec<(String, RuntimeState)>, RuntimeError> {
        if self.fail_list.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(RuntimeError::Unavailable("unreachable".into()));
        }
        Ok(self
            .computers
            .lock()
            .unwrap()
            .iter()
            .map(|(n, s)| (n.clone(), *s))
            .collect())
    }
    async fn create(&self, spec: &Spec) -> Result<(), RuntimeError> {
        let delay = *self.create_delay.lock().unwrap();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        self.computers
            .lock()
            .unwrap()
            .insert(spec.name.clone(), RuntimeState::Running);
        Ok(())
    }
    async fn start(&self, name: &str) -> Result<(), RuntimeError> {
        self.set(name, RuntimeState::Running)
    }
    async fn stop(&self, name: &str) -> Result<(), RuntimeError> {
        if self.stops_fail.load(SeqCst) {
            return Err(RuntimeError::Unavailable("stop timed out".into()));
        }
        for state in self.jobs.lock().unwrap().values_mut() {
            state.get_or_insert(137);
        }
        self.set(name, RuntimeState::Stopped)
    }
    async fn pause(&self, name: &str) -> Result<(), RuntimeError> {
        self.set(name, RuntimeState::Paused)
    }
    async fn resume(&self, name: &str) -> Result<(), RuntimeError> {
        self.set(name, RuntimeState::Running)
    }
    async fn copy_out(&self, _: &str, _: &str, host: &std::path::Path) -> Result<(), RuntimeError> {
        std::fs::write(host, b"x").map_err(|e| RuntimeError::Unavailable(e.to_string()))
    }
    async fn exec(
        &self,
        name: &str,
        _: &str,
        args: &[&str],
        _: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        assert_eq!(
            self.computers.lock().unwrap().get(name),
            Some(&RuntimeState::Running),
            "exec only on a running computer"
        );
        let script = args[1];
        let rest = &args[3..];
        if script.contains("setsid") {
            self.launches.lock().unwrap().push(rest[0].to_owned());
            self.jobs.lock().unwrap().insert(rest[0].to_owned(), None);
            if self.lose_launch_reply.load(SeqCst) {
                return Err(RuntimeError::Unavailable("exec timed out".into()));
            }
            Ok(out("launched\n"))
        } else if script.contains("pgrep -g") {
            self.kills.lock().unwrap().push(rest[0].to_owned());
            if self.kills_fail.load(SeqCst) {
                return Ok(out("alive\n"));
            }
            if let Some(state) = self.jobs.lock().unwrap().get_mut(rest[0]) {
                state.get_or_insert(130);
            }
            Ok(out("stopped\n"))
        } else if script.contains("kill -0") {
            match self.jobs.lock().unwrap().get(rest[0]) {
                Some(None) => Ok(out("running\n")),
                Some(Some(code)) => Ok(out(format!("done {code}\n"))),
                None => Ok(out("lost\n")),
            }
        } else if script.contains("mktemp") {
            Ok(out(format!("0\n{}\n0\n{}\n\n\0\n", rest[0].len(), rest[0])))
        } else {
            Ok(out(""))
        }
    }
}

fn config(max_vms: u64) -> ComputerConfig {
    ComputerConfig {
        max_cpus: max_vms as u32,
        max_memory_mib: max_vms * (2048 + 256),
        idle_after: Duration::ZERO,
        ..ComputerConfig::default()
    }
}

async fn api(app: &TestApp, guest: &Arc<Guest>, max_vms: u64) -> ComputerManager {
    let manager = ComputerManager::new(app.db.pool.clone(), guest.clone(), config(max_vms));
    manager.reconcile_startup().await.unwrap();
    manager
}

fn never() -> tokio_util::sync::CancellationToken {
    tokio_util::sync::CancellationToken::new()
}

async fn observed(app: &TestApp, bot: Uuid) -> String {
    sqlx::query_scalar("SELECT observed_state FROM bot_computers WHERE bot_id = ?1")
        .bind(bot)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn job_status(app: &TestApp, job: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, error FROM guest_jobs WHERE id = ?1")
        .bind(job)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn age_jobs(app: &TestApp) {
    sqlx::query("UPDATE guest_jobs SET started_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '-10 minutes')")
        .execute(&app.db.pool)
        .await
        .unwrap();
}

async fn bot_status(app: &TestApp, bot: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
        .bind(bot)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn receipt(app: &TestApp, request: Uuid) -> String {
    sqlx::query_scalar("SELECT state FROM request_receipts WHERE request_id = ?1")
        .bind(request)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn stop_work(app: &TestApp, bot: Uuid) -> Uuid {
    let request = Uuid::new_v4();
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": request })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    request
}

async fn job_leases(app: &TestApp) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM computer_leases WHERE holder LIKE 'job:%' AND released_at IS NULL",
    )
    .fetch_one(&app.db.pool)
    .await
    .unwrap()
}

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_before_launch_never_launches_and_ends_the_job() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::installed().await;
    let guest = Arc::new(Guest::default());
    let bot: Uuid = app.create_bot("A").await.parse().unwrap();
    let first = api(&app, &guest, 2).await;
    faults::arm("job.after_insert");
    let crashed = first
        .start_job(bot, None, "work", Duration::from_secs(600))
        .await;
    assert!(matches!(crashed, Err(ComputerError::Interrupted)));
    drop(first);
    let restarted = api(&app, &guest, 2).await;
    age_jobs(&app).await;
    restarted.tick(&never()).await.unwrap();
    let job: Uuid = sqlx::query_scalar("SELECT id FROM guest_jobs")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(job_status(&app, job).await.0, "failed");
    assert_eq!(guest.launches(), 0, "nothing was launched, before or after");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_after_launch_keeps_the_same_job_running_once() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::installed().await;
    let guest = Arc::new(Guest::default());
    let bot: Uuid = app.create_bot("A").await.parse().unwrap();
    let first = api(&app, &guest, 2).await;
    faults::arm("job.after_launch");
    assert!(matches!(
        first
            .start_job(bot, None, "work", Duration::from_secs(600))
            .await,
        Err(ComputerError::Interrupted)
    ));
    drop(first);
    let restarted = api(&app, &guest, 2).await;
    age_jobs(&app).await;
    restarted.tick(&never()).await.unwrap();
    let job: Uuid = sqlx::query_scalar("SELECT id FROM guest_jobs")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        job_status(&app, job).await.0,
        "running",
        "recovered, not relaunched"
    );
    assert_eq!(
        observed(&app, bot).await,
        "running",
        "its lease keeps the computer"
    );
    guest.finish(job, 0);
    restarted.tick(&never()).await.unwrap();
    assert_eq!(job_status(&app, job).await.0, "completed");
    restarted.tick(&never()).await.unwrap();
    assert_eq!(
        observed(&app, bot).await,
        "stopped",
        "released, then idle-stopped"
    );
    assert_eq!(guest.launches(), 1, "launched exactly once");
}

#[tokio::test(flavor = "multi_thread")]
async fn crashes_around_start_stop_and_renewal_keep_reservations_until_reconciled() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::installed().await;
    let guest = Arc::new(Guest::default());
    let a: Uuid = app.create_bot("A").await.parse().unwrap();
    let b: Uuid = app.create_bot("B").await.parse().unwrap();

    let first = api(&app, &guest, 1).await;
    faults::arm("computer.after_start_decision");
    assert!(matches!(
        first.acquire(a, "test:a").await,
        Err(ComputerError::Interrupted)
    ));
    assert_eq!(observed(&app, a).await, "creating");
    assert!(matches!(
        first.acquire(b, "test:b").await.unwrap(),
        Acquire::Queued { .. }
    ));
    drop(first);
    let second = api(&app, &guest, 1).await;
    assert_eq!(observed(&app, a).await, "absent");
    let Acquire::Ready { lease_id, .. } = second.acquire(a, "test:a").await.unwrap() else {
        panic!()
    };

    sqlx::query(
        "UPDATE computer_leases SET expires_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '-1 seconds') WHERE id = ?1",
    )
    .bind(lease_id)
    .execute(&app.db.pool)
    .await
    .unwrap();
    faults::arm("lease.after_renew");
    assert!(matches!(
        second.renew(lease_id).await,
        Err(ComputerError::Interrupted)
    ));
    drop(second);
    let third = api(&app, &guest, 1).await;
    third.tick(&never()).await.unwrap();
    assert_eq!(
        observed(&app, a).await,
        "running",
        "the renewed lease still holds it"
    );

    third.release(lease_id).await.unwrap();
    faults::arm("computer.after_stop_decision");
    assert!(matches!(
        third.tick(&never()).await,
        Err(ComputerError::Interrupted)
    ));
    assert_eq!(observed(&app, a).await, "stopping");
    assert!(matches!(
        third.acquire(b, "test:b").await.unwrap(),
        Acquire::Queued { .. }
    ));
    drop(third);
    let fourth = api(&app, &guest, 1).await;
    assert_eq!(observed(&app, a).await, "running");
    fourth.tick(&never()).await.unwrap();
    assert_eq!(observed(&app, a).await, "stopped");
    assert!(matches!(
        fourth.acquire(b, "test:b").await.unwrap(),
        Acquire::Ready { .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_wait_and_an_unreachable_runtime_keep_reservations() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::installed().await;
    let guest = Arc::new(Guest::default());
    *guest.create_delay.lock().unwrap() = Duration::from_millis(500);
    let a: Uuid = app.create_bot("A").await.parse().unwrap();
    let b: Uuid = app.create_bot("B").await.parse().unwrap();
    let manager = Arc::new(api(&app, &guest, 1).await);
    let m = manager.clone();
    let waiting = tokio::spawn(async move { m.acquire(a, "test:a").await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    waiting.abort();
    let _ = waiting.await;
    assert_eq!(observed(&app, a).await, "creating");
    assert!(matches!(
        manager.acquire(b, "test:b").await.unwrap(),
        Acquire::Queued { .. }
    ));
    tokio::time::sleep(Duration::from_millis(600)).await;
    manager.tick(&never()).await.unwrap();
    assert_eq!(observed(&app, a).await, "absent");
    assert!(matches!(
        manager.acquire(b, "test:b").await.unwrap(),
        Acquire::Ready { .. }
    ));
    guest
        .fail_list
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let blind = ComputerManager::new(app.db.pool.clone(), guest.clone(), config(1));
    blind.reconcile_startup().await.unwrap();
    assert!(matches!(
        blind.acquire(b, "test:b").await.unwrap(),
        Acquire::Queued { .. }
    ));
    assert_eq!(
        observed(&app, a).await,
        "absent",
        "an unreachable runtime changes no state"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_work_cancels_background_jobs_and_completes_after_them() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::installed().await;
    let guest = Arc::new(Guest::default());
    let bot = app.create_bot("Busy").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let manager = api(&app, &guest, 2).await;
    let Started::Job(job) = manager
        .start_job(bot_id, None, "long", Duration::from_secs(600))
        .await
        .unwrap()
    else {
        panic!()
    };
    manager.close_desktop(bot_id).await.unwrap();
    manager.tick(&never()).await.unwrap();
    assert_eq!(job_status(&app, job).await.0, "running");
    let request = uuid::Uuid::new_v4();
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": request })),
        )
        .await;
    assert_eq!(stop.status, StatusCode::ACCEPTED, "{}", stop.body);
    assert_eq!(job_status(&app, job).await.0, "cancelling");
    manager.tick(&never()).await.unwrap();
    assert_eq!(job_status(&app, job).await.0, "cancelled");
    assert_eq!(guest.kills.lock().unwrap().len(), 1);
    let again = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": request })),
        )
        .await;
    assert_eq!(
        again.status,
        StatusCode::OK,
        "the stop completed: {}",
        again.body
    );
    assert_eq!(again.body["state"], "done");
    let Started::Job(other) = manager
        .start_job(bot_id, None, "again", Duration::from_secs(600))
        .await
        .unwrap()
    else {
        panic!()
    };
    guest.stops_fail.store(true, SeqCst);
    let deleted = app.owner("DELETE", &format!("/v1/bots/{bot}"), None).await;
    assert!(deleted.status.is_success());
    manager.tick(&never()).await.unwrap();
    assert_eq!(job_status(&app, other).await.0, "cancelled");
    assert_eq!(
        guest.kills.lock().unwrap().len(),
        2,
        "killed, not assumed gone"
    );
    assert_eq!(bot_status(&app, bot_id).await, "deleting");
    guest.stops_fail.store(false, SeqCst);
    manager.tick(&never()).await.unwrap();
    assert_eq!(observed(&app, bot_id).await, "stopped");
    assert_eq!(bot_status(&app, bot_id).await, "deleted");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_launch_whose_reply_is_lost_is_recovered_not_failed() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::installed().await;
    let guest = Arc::new(Guest::default());
    let bot: Uuid = app.create_bot("A").await.parse().unwrap();
    let manager = api(&app, &guest, 2).await;
    guest.lose_launch_reply.store(true, SeqCst);
    let Started::Job(job) = manager
        .start_job(bot, None, "work", Duration::from_secs(600))
        .await
        .unwrap()
    else {
        panic!()
    };
    let (status, error) = job_status(&app, job).await;
    assert_eq!(status, "starting", "not recorded as a failure");
    assert!(error.unwrap().contains("unconfirmed"));
    assert_eq!(job_leases(&app).await, 1, "it keeps its reservation");
    age_jobs(&app).await;
    manager.tick(&never()).await.unwrap();
    assert_eq!(
        job_status(&app, job).await.0,
        "running",
        "found in the guest"
    );
    guest.finish(job, 0);
    manager.tick(&never()).await.unwrap();
    assert_eq!(job_status(&app, job).await.0, "completed");
    assert_eq!(guest.launches(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelling_job_keeps_its_lease_and_fences_new_jobs_until_killed() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::installed().await;
    let guest = Arc::new(Guest::default());
    let bot: Uuid = app.create_bot("A").await.parse().unwrap();
    let manager = api(&app, &guest, 2).await;
    let Started::Job(job) = manager
        .start_job(bot, None, "work", Duration::from_secs(600))
        .await
        .unwrap()
    else {
        panic!()
    };
    let Acquire::Ready { .. } = manager.acquire(bot, "desktop:x").await.unwrap() else {
        panic!()
    };
    guest.kills_fail.store(true, SeqCst);
    let request = stop_work(&app, bot).await;
    manager.tick(&never()).await.unwrap();
    assert_eq!(job_status(&app, job).await.0, "cancelling");
    sqlx::query("UPDATE computer_leases SET expires_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '-1 minutes')")
        .execute(&app.db.pool)
        .await
        .unwrap();
    manager.tick(&never()).await.unwrap();
    assert_eq!(job_status(&app, job).await.0, "cancelling");
    assert_eq!(
        job_leases(&app).await,
        1,
        "not released before the kill is confirmed"
    );
    assert_eq!(receipt(&app, request).await, "pending");
    assert_eq!(
        manager
            .start_job(bot, None, "late", Duration::from_secs(600))
            .await
            .unwrap(),
        Started::Stopped
    );
    assert_eq!(guest.launches(), 1);
    guest.kills_fail.store(false, SeqCst);
    manager.tick(&never()).await.unwrap();
    assert_eq!(job_status(&app, job).await.0, "cancelled");
    assert_eq!(job_leases(&app).await, 0);
    assert_eq!(receipt(&app, request).await, "done");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_for_a_stopped_turn_is_refused() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::installed().await;
    let guest = Arc::new(Guest::default());
    let bot: Uuid = app.create_bot("A").await.parse().unwrap();
    let manager = api(&app, &guest, 2).await;
    let ended = Uuid::new_v4();
    assert_eq!(
        manager
            .start_job(bot, Some(ended), "late", Duration::from_secs(600))
            .await
            .unwrap(),
        Started::Stopped
    );
    assert_eq!(guest.launches(), 0);
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM guest_jobs")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(jobs, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_before_the_job_had_a_lease_completes() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::installed().await;
    let guest = Arc::new(Guest::default());
    let bot: Uuid = app.create_bot("A").await.parse().unwrap();
    let first = api(&app, &guest, 2).await;
    faults::arm("job.after_insert");
    assert!(matches!(
        first
            .start_job(bot, None, "work", Duration::from_secs(600))
            .await,
        Err(ComputerError::Interrupted)
    ));
    drop(first);
    let request = stop_work(&app, bot).await;
    assert_eq!(receipt(&app, request).await, "pending");
    let restarted = api(&app, &guest, 2).await;
    restarted.tick(&never()).await.unwrap();
    let (status,): (String,) = sqlx::query_as("SELECT status FROM guest_jobs")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "cancelled");
    assert_eq!(receipt(&app, request).await, "done");
    assert_eq!(guest.launches(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_ending_by_itself_during_a_stop_still_completes_the_stop() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::installed().await;
    let guest = Arc::new(Guest::default());
    let bot: Uuid = app.create_bot("A").await.parse().unwrap();
    let manager = api(&app, &guest, 2).await;
    let Started::Job(job) = manager
        .start_job(bot, None, "work", Duration::from_secs(600))
        .await
        .unwrap()
    else {
        panic!()
    };
    let request = stop_work(&app, bot).await;
    sqlx::query("UPDATE guest_jobs SET status = 'completed', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE id = ?1")
        .bind(job)
        .execute(&app.db.pool)
        .await
        .unwrap();
    manager.tick(&never()).await.unwrap();
    assert_eq!(receipt(&app, request).await, "done");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unconfirmed_job_cancel_is_retried_until_the_job_is_gone() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::installed().await;
    let guest = Arc::new(Guest::default());
    let bot: Uuid = app.create_bot("A").await.parse().unwrap();
    let manager = api(&app, &guest, 2).await;
    let Started::Job(job) = manager
        .start_job(bot, None, "work", Duration::from_secs(600))
        .await
        .unwrap()
    else {
        panic!()
    };
    guest.kills_fail.store(true, SeqCst);
    let state = manager.cancel_job(bot, job).await.unwrap().unwrap();
    assert_eq!(state.status, JobStatus::Cancelling);
    assert_eq!(
        job_leases(&app).await,
        1,
        "not released before the kill is confirmed"
    );
    guest.kills_fail.store(false, SeqCst);
    manager.tick(&never()).await.unwrap();
    assert_eq!(job_status(&app, job).await.0, "cancelled");
    assert_eq!(job_leases(&app).await, 0);
    assert_eq!(guest.kills.lock().unwrap().len(), 2);
    let again = manager.cancel_job(bot, job).await.unwrap().unwrap();
    assert_eq!(
        again.status,
        JobStatus::Cancelled,
        "a settled job stays settled"
    );
    assert_eq!(guest.kills.lock().unwrap().len(), 2);
    let other: Uuid = app.create_bot("B").await.parse().unwrap();
    assert_eq!(manager.cancel_job(other, job).await.unwrap(), None);
    assert_eq!(job_status(&app, job).await.0, "cancelled");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_cancel_on_a_paused_computer_resumes_it_and_stops_only_that_job() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::installed().await;
    let guest = Arc::new(Guest::default());
    let bot: Uuid = app.create_bot("A").await.parse().unwrap();
    let manager = api(&app, &guest, 2).await;
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let Started::Job(job) = manager
            .start_job(bot, None, "work", Duration::from_secs(600))
            .await
            .unwrap()
        else {
            panic!()
        };
        jobs.push(job);
    }
    let (cancelled, kept) = (jobs[0], jobs[1]);
    guest
        .set(&ComputerManager::runtime_name(bot), RuntimeState::Paused)
        .unwrap();
    sqlx::query("UPDATE bot_computers SET observed_state = 'paused' WHERE bot_id = ?1")
        .bind(bot)
        .execute(&app.db.pool)
        .await
        .unwrap();
    manager.cancel_job(bot, cancelled).await.unwrap().unwrap();
    manager.tick(&never()).await.unwrap();
    assert_eq!(job_status(&app, cancelled).await.0, "cancelled");
    assert_eq!(job_status(&app, kept).await.0, "running");
    assert_eq!(observed(&app, bot).await, "running");
    assert_eq!(
        job_leases(&app).await,
        1,
        "only the cancelled job's lease ends"
    );
    assert_eq!(guest.kills.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_settlement_confirms_a_pending_job_cancel() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::installed().await;
    let guest = Arc::new(Guest::default());
    let bot: Uuid = app.create_bot("A").await.parse().unwrap();
    let manager = api(&app, &guest, 2).await;
    let Started::Job(job) = manager
        .start_job(bot, None, "work", Duration::from_secs(600))
        .await
        .unwrap()
    else {
        panic!()
    };
    guest.kills_fail.store(true, SeqCst);
    manager.cancel_job(bot, job).await.unwrap().unwrap();
    drop(manager);
    guest.kills_fail.store(false, SeqCst);
    guest.finish(job, 0);
    let offline = api(&app, &guest, 2).await;
    offline.settle_offline(bot).await.unwrap();
    assert_eq!(job_status(&app, job).await.0, "cancelled");
    assert_eq!(job_leases(&app).await, 0);
}

async fn pause_with_bot_stop(app: &TestApp, guest: &Guest, bot: Uuid) {
    guest
        .set(&ComputerManager::runtime_name(bot), RuntimeState::Paused)
        .unwrap();
    sqlx::query("UPDATE bot_computers SET observed_state = 'paused' WHERE bot_id = ?1")
        .bind(bot)
        .execute(&app.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE bots SET cancel_pending = 1 WHERE id = ?1")
        .bind(bot)
        .execute(&app.db.pool)
        .await
        .unwrap();
    else_api::computers::jobs::request_cancel(&app.db.pool, bot, None)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_cancel_racing_a_bot_stop_does_not_resume_its_paused_work() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::installed().await;
    let guest = Arc::new(Guest::default());
    let bot: Uuid = app.create_bot("A").await.parse().unwrap();
    let manager = api(&app, &guest, 2).await;
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let Started::Job(job) = manager
            .start_job(bot, None, "work", Duration::from_secs(600))
            .await
            .unwrap()
        else {
            panic!()
        };
        jobs.push(job);
    }
    pause_with_bot_stop(&app, &guest, bot).await;
    manager.cancel_job(bot, jobs[0]).await.unwrap().unwrap();
    assert_eq!(observed(&app, bot).await, "stopped");
    assert!(guest.kills.lock().unwrap().is_empty());
    for job in jobs {
        assert_eq!(job_status(&app, job).await.0, "cancelled");
    }
    assert_eq!(job_leases(&app).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_settlement_settles_cancels_ended_by_a_bot_stop() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::installed().await;
    let guest = Arc::new(Guest::default());
    let bot: Uuid = app.create_bot("A").await.parse().unwrap();
    let manager = api(&app, &guest, 2).await;
    let Started::Job(job) = manager
        .start_job(bot, None, "work", Duration::from_secs(600))
        .await
        .unwrap()
    else {
        panic!()
    };
    drop(manager);
    pause_with_bot_stop(&app, &guest, bot).await;
    let offline = api(&app, &guest, 2).await;
    offline.settle_offline(bot).await.unwrap();
    assert_eq!(observed(&app, bot).await, "stopped");
    assert_eq!(job_status(&app, job).await.0, "cancelled");
    assert_eq!(job_leases(&app).await, 0);
}
