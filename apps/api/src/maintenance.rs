//! The maintenance gate for coordinated backups. Work that can
//! change state or wake a computer is admitted through the gate and holds
//! an [`Admission`] until it has finished: a turn claim (until the turn is
//! registered as running, which the drain then waits for), a mutating owner
//! request, a task-scoped tool call, a memory summary, and a compaction
//! step. Closing the gate refuses new admissions and waits until every
//! admitted one has ended, so the database, the bot computers, and the
//! conversation records are captured at one application checkpoint.
//! Reads, Stop work, and stopping a computer stay available.
//!
//! While the closer drains, turns that were already running keep their
//! tool calls ([`Gate::enter_turn`]), so they can finish instead of failing;
//! [`Closed::capture`] then refuses those too and waits for the calls in
//! flight. The gate publishes a [`Pause`] for the owner until it reopens.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use else_wire::maintenance::{PausePhase, PauseReason, ServerPause};
use tokio::sync::watch;

const CLOSED: usize = 1 << (usize::BITS - 1);
const ADMITTED: usize = CLOSED - 1;
/// Set in `turn_work` once the closer starts capturing: from then on even
/// running turns are refused.
const CAPTURING: usize = CLOSED;

#[derive(Debug)]
pub struct Gate {
    state: AtomicUsize,
    idle: tokio::sync::Notify,
    /// Tool calls from turns that were already running when the gate
    /// closed. They need no admission while the closer drains those turns,
    /// and the closer waits for them before it captures.
    turn_work: AtomicUsize,
    turn_idle: tokio::sync::Notify,
    pause: watch::Sender<Option<Pause>>,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            state: AtomicUsize::new(0),
            idle: tokio::sync::Notify::new(),
            turn_work: AtomicUsize::new(0),
            turn_idle: tokio::sync::Notify::new(),
            pause: watch::Sender::new(None),
        }
    }
}

/// Why new work is paused, shown to the owner while the gate is closed.
#[derive(Clone, Debug, PartialEq)]
pub struct Pause {
    pub reason: Reason,
    pub started_at: DateTime<Utc>,
    /// How long the pause is expected to last, from the last one like it.
    pub expected: Option<Duration>,
    pub step: Option<Step>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Backup,
    Update,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    WaitingForTasks,
    Computers { done: usize, total: usize },
    Files,
    Projects,
    Database,
}

impl Pause {
    pub fn backup(expected: Option<Duration>) -> Self {
        Self {
            reason: Reason::Backup,
            started_at: Utc::now(),
            expected,
            step: Some(Step::WaitingForTasks),
        }
    }

    pub fn update() -> Self {
        Self {
            reason: Reason::Update,
            started_at: Utc::now(),
            expected: None,
            step: None,
        }
    }
}

impl From<&Pause> for ServerPause {
    fn from(pause: &Pause) -> Self {
        let (phase, done, total) = match pause.step {
            None => (None, None, None),
            Some(Step::WaitingForTasks) => (Some(PausePhase::WaitingForTasks), None, None),
            Some(Step::Computers { done, total }) => {
                (Some(PausePhase::Computers), Some(done), Some(total))
            }
            Some(Step::Files) => (Some(PausePhase::Files), None, None),
            Some(Step::Projects) => (Some(PausePhase::Projects), None, None),
            Some(Step::Database) => (Some(PausePhase::Database), None, None),
        };
        Self {
            reason: match pause.reason {
                Reason::Backup => PauseReason::Backup,
                Reason::Update => PauseReason::Update,
            },
            started_at: Some(pause.started_at),
            expected_secs: pause.expected.map(|d| d.as_secs()),
            // Measured here so clients count down without trusting their clock.
            elapsed_secs: (Utc::now() - pause.started_at).num_seconds().max(0) as u64,
            phase,
            done,
            total,
        }
    }
}

/// Admitted work; the gate cannot close until it is dropped.
pub struct Admission(Arc<Gate>);

impl Admission {
    /// Admit work this admission hands off, such as a task it spawns. A
    /// closer already waits for this admission, so it waits for the share.
    pub fn share(&self) -> Self {
        self.0.state.fetch_add(1, Ordering::AcqRel);
        Self(self.0.clone())
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        if self.0.state.fetch_sub(1, Ordering::AcqRel) & ADMITTED == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

/// Work for a turn that is already running: admitted like any work while
/// the gate is open, and still let through while the closer drains turns.
pub enum TurnAdmission {
    Open(Admission),
    Draining(Draining),
}

pub struct Draining(Arc<Gate>);

impl Drop for Draining {
    fn drop(&mut self) {
        if self.0.turn_work.fetch_sub(1, Ordering::AcqRel) & ADMITTED == 1 {
            self.0.turn_idle.notify_waiters();
        }
    }
}

/// Reopens the gate when dropped, however the backup ends.
pub struct Closed(Arc<Gate>);

impl Drop for Closed {
    fn drop(&mut self) {
        // Release CLOSED last: once it clears, the next closer may set
        // CAPTURING and publish its own pause, which must survive.
        self.0.turn_work.fetch_and(ADMITTED, Ordering::AcqRel);
        self.0.pause.send_replace(None);
        self.0.state.fetch_and(ADMITTED, Ordering::AcqRel);
    }
}

impl Closed {
    /// Refuse running turns too, once their tool calls in flight have
    /// ended. `Err` (with turn work let through again) if some did not end
    /// within `timeout`.
    pub async fn capture(&self, timeout: Duration) -> Result<(), usize> {
        let gate = &self.0;
        gate.turn_work.fetch_or(CAPTURING, Ordering::AcqRel);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = gate.turn_idle.notified();
            let running = gate.turn_work.load(Ordering::Acquire) & ADMITTED;
            if running == 0 {
                return Ok(());
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                let running = gate.turn_work.load(Ordering::Acquire) & ADMITTED;
                if running == 0 {
                    return Ok(());
                }
                gate.turn_work.fetch_and(ADMITTED, Ordering::AcqRel);
                return Err(running);
            }
        }
    }

    /// Show the owner what the pause is doing now.
    pub fn step(&self, step: Step) {
        self.0.pause.send_modify(|pause| {
            if let Some(pause) = pause {
                pause.step = Some(step);
            }
        });
    }
}

impl Gate {
    pub fn is_closed(&self) -> bool {
        self.state.load(Ordering::Acquire) & CLOSED != 0
    }

    /// The current pause, `None` while the gate is open.
    pub fn pause(&self) -> Option<Pause> {
        self.pause.borrow().clone()
    }

    pub fn watch_pause(&self) -> watch::Receiver<Option<Pause>> {
        self.pause.subscribe()
    }

    /// Admit work, or `None` while the gate is closed.
    pub fn enter(self: &Arc<Self>) -> Option<Admission> {
        self.state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & CLOSED == 0 && state < ADMITTED).then_some(state + 1)
            })
            .ok()?;
        Some(Admission(self.clone()))
    }

    /// Admit work for a turn that is already running: a tool call. The
    /// caller has checked that the turn runs. `None` only once the closer
    /// captures.
    pub fn enter_turn(self: &Arc<Self>) -> Option<TurnAdmission> {
        if let Some(admission) = self.enter() {
            return Some(TurnAdmission::Open(admission));
        }
        self.turn_work
            .try_update(Ordering::AcqRel, Ordering::Acquire, |work| {
                (work & CAPTURING == 0 && work < ADMITTED).then_some(work + 1)
            })
            .ok()?;
        Some(TurnAdmission::Draining(Draining(self.clone())))
    }

    /// Close the gate and wait until every admitted piece of work has
    /// ended. `pause` is shown to the owner until the gate reopens.
    /// `Ok(None)` if another closer holds it; `Err` (with the gate
    /// reopened) if admitted work did not end within `timeout`.
    pub async fn close(
        self: &Arc<Self>,
        timeout: Duration,
        pause: Pause,
    ) -> Result<Option<Closed>, usize> {
        if self.state.fetch_or(CLOSED, Ordering::AcqRel) & CLOSED != 0 {
            return Ok(None);
        }
        self.pause.send_replace(Some(pause));
        let closed = Closed(self.clone());
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.idle.notified();
            let admitted = self.state.load(Ordering::Acquire) & ADMITTED;
            if admitted == 0 {
                return Ok(Some(closed));
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                let admitted = self.state.load(Ordering::Acquire) & ADMITTED;
                if admitted == 0 {
                    return Ok(Some(closed));
                }
                return Err(admitted);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_closer_waits_for_a_share_its_admission_handed_off() {
        let gate = Arc::new(Gate::default());
        let admission = gate.enter().unwrap();
        let share = admission.share();
        drop(admission);
        assert!(
            gate.close(Duration::from_millis(50), Pause::backup(None))
                .await
                .is_err(),
            "the share keeps the gate from closing"
        );
        drop(share);
        assert!(matches!(
            gate.close(Duration::from_secs(1), Pause::backup(None))
                .await,
            Ok(Some(_))
        ));
    }

    #[tokio::test]
    async fn the_next_closer_keeps_its_capture_and_pause() {
        let gate = Arc::new(Gate::default());
        let first = gate
            .close(Duration::from_secs(1), Pause::update())
            .await
            .unwrap()
            .unwrap();
        drop(first);
        let second = gate
            .close(Duration::from_secs(1), Pause::backup(None))
            .await
            .unwrap()
            .unwrap();
        second.capture(Duration::from_secs(1)).await.unwrap();
        assert!(
            gate.enter_turn().is_none(),
            "capturing refuses running turns"
        );
        assert_eq!(gate.pause().map(|p| p.reason), Some(Reason::Backup));
        drop(second);
        assert!(gate.pause().is_none());
        assert!(gate.enter_turn().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_closers_never_clear_each_others_capture() {
        let gate = Arc::new(Gate::default());
        let mut closers = Vec::new();
        for _ in 0..4 {
            let gate = gate.clone();
            closers.push(tokio::spawn(async move {
                for _ in 0..500 {
                    let Ok(Some(closed)) = gate
                        .close(Duration::from_secs(1), Pause::backup(None))
                        .await
                    else {
                        tokio::task::yield_now().await;
                        continue;
                    };
                    closed.capture(Duration::from_secs(1)).await.unwrap();
                    tokio::task::yield_now().await;
                    assert!(
                        gate.enter_turn().is_none(),
                        "another closer cleared the capture"
                    );
                    assert!(gate.pause().is_some(), "another closer cleared the pause");
                }
            }));
        }
        for closer in closers {
            closer.await.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_admissions_never_enter_a_closed_gate() {
        let gate = Arc::new(Gate::default());
        let stop = tokio_util::sync::CancellationToken::new();
        let mut workers = Vec::new();
        for _ in 0..8 {
            let gate = gate.clone();
            let stop = stop.clone();
            workers.push(tokio::spawn(async move {
                while !stop.is_cancelled() {
                    let admission = gate.enter();
                    tokio::task::yield_now().await;
                    drop(admission);
                }
            }));
        }
        for _ in 0..100 {
            let closed = gate
                .close(Duration::from_secs(1), Pause::update())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(gate.state.load(Ordering::Acquire), CLOSED);
            assert!(gate.enter().is_none());
            tokio::task::yield_now().await;
            assert_eq!(gate.state.load(Ordering::Acquire), CLOSED);
            drop(closed);
        }
        stop.cancel();
        for worker in workers {
            worker.await.unwrap();
        }
        assert_eq!(gate.state.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn closing_waits_for_admitted_work_and_refuses_new_work() {
        let gate = Arc::new(Gate::default());
        let admitted = gate.enter().unwrap();
        assert_eq!(
            gate.close(Duration::from_millis(50), Pause::update())
                .await
                .err(),
            Some(1)
        );
        assert!(!gate.is_closed(), "a failed close reopens");
        let closer = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.close(Duration::from_secs(5), Pause::update()).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(gate.enter().is_none(), "nothing new while closing");
        drop(admitted);
        let closed = closer.await.unwrap().unwrap().unwrap();
        assert!(gate.is_closed());
        assert!(
            gate.close(Duration::from_millis(1), Pause::update())
                .await
                .unwrap()
                .is_none()
        );
        drop(closed);
        assert!(gate.enter().is_some());
    }

    #[tokio::test]
    async fn running_turns_keep_working_while_draining_and_stop_at_capture() {
        let gate = Arc::new(Gate::default());
        assert!(matches!(gate.enter_turn(), Some(TurnAdmission::Open(_))));
        let closed = gate
            .close(Duration::from_millis(50), Pause::backup(None))
            .await
            .unwrap()
            .unwrap();
        assert!(gate.enter().is_none(), "no new work while draining");
        let call = gate.enter_turn();
        assert!(matches!(call, Some(TurnAdmission::Draining(_))));
        let capture = {
            let gate = gate.clone();
            tokio::spawn(async move {
                let closed = closed;
                let captured = closed.capture(Duration::from_secs(5)).await;
                assert!(gate.enter_turn().is_none(), "captures refuse turn work");
                (closed, captured)
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !capture.is_finished(),
            "capture waits for the call in flight"
        );
        drop(call);
        let (closed, captured) = capture.await.unwrap();
        assert_eq!(captured, Ok(()));
        drop(closed);
        assert!(matches!(gate.enter_turn(), Some(TurnAdmission::Open(_))));
    }

    #[tokio::test]
    async fn a_capture_that_times_out_lets_turn_work_through_again() {
        let gate = Arc::new(Gate::default());
        let closed = gate
            .close(Duration::from_millis(50), Pause::backup(None))
            .await
            .unwrap()
            .unwrap();
        let call = gate.enter_turn().unwrap();
        assert_eq!(closed.capture(Duration::from_millis(20)).await, Err(1));
        assert!(matches!(
            gate.enter_turn(),
            Some(TurnAdmission::Draining(_))
        ));
        drop(call);
    }

    #[tokio::test]
    async fn the_pause_is_published_while_closed_and_cleared_on_reopen() {
        let gate = Arc::new(Gate::default());
        let mut watch = gate.watch_pause();
        assert_eq!(gate.pause(), None);
        let closed = gate
            .close(
                Duration::from_millis(50),
                Pause {
                    started_at: "2099-01-01T00:00:00Z".parse().unwrap(),
                    ..Pause::backup(Some(Duration::from_secs(300)))
                },
            )
            .await
            .unwrap()
            .unwrap();
        watch.changed().await.unwrap();
        let pause = watch.borrow_and_update().clone().unwrap();
        assert_eq!(pause.step, Some(Step::WaitingForTasks));
        assert_eq!(
            serde_json::json!(ServerPause::from(&pause)),
            serde_json::json!({
                "reason": "backup",
                "started_at": "2099-01-01T00:00:00.000Z",
                "expected_secs": 300,
                "elapsed_secs": 0,
                "phase": "waiting_for_tasks",
                "done": null,
                "total": null,
            })
        );
        closed.step(Step::Computers { done: 6, total: 11 });
        watch.changed().await.unwrap();
        let pause = watch.borrow_and_update().clone().unwrap();
        let payload = serde_json::json!(ServerPause::from(&pause));
        assert_eq!(payload["phase"], "computers");
        assert_eq!(payload["done"], 6);
        assert_eq!(payload["total"], 11);
        assert!(
            gate.close(Duration::from_millis(1), Pause::update())
                .await
                .unwrap()
                .is_none(),
            "a second closer is refused and leaves the pause alone"
        );
        assert_eq!(gate.pause().unwrap().reason, Reason::Backup);
        drop(closed);
        watch.changed().await.unwrap();
        assert_eq!(*watch.borrow(), None);
    }

    #[tokio::test]
    async fn a_close_that_times_out_clears_its_pause() {
        let gate = Arc::new(Gate::default());
        let admitted = gate.enter().unwrap();
        assert!(
            gate.close(Duration::from_millis(20), Pause::update())
                .await
                .is_err()
        );
        assert_eq!(gate.pause(), None);
        drop(admitted);
    }
}
