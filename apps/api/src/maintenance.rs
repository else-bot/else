//! The maintenance gate for coordinated backups (POC-022). Work that can
//! change state or wake a computer is admitted through the gate and holds
//! an [`Admission`] until it has finished: a turn claim (until the turn is
//! registered as running, which the drain then waits for), a mutating owner
//! request, a task-scoped tool call, a memory summary, and a compaction
//! step. Closing the gate refuses new admissions and waits until every
//! admitted one has ended, so the database, the bot computers, and the
//! conversation records are captured at one application checkpoint.
//! Reads, Stop work, and stopping a computer stay available.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const CLOSED: usize = 1 << (usize::BITS - 1);
const ADMITTED: usize = CLOSED - 1;

#[derive(Debug, Default)]
pub struct Gate {
    state: AtomicUsize,
    idle: tokio::sync::Notify,
}

/// Admitted work; the gate cannot close until it is dropped.
pub struct Admission(Arc<Gate>);

impl Drop for Admission {
    fn drop(&mut self) {
        if self.0.state.fetch_sub(1, Ordering::AcqRel) & ADMITTED == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

/// Reopens the gate when dropped, however the backup ends.
pub struct Closed(Arc<Gate>);

impl Drop for Closed {
    fn drop(&mut self) {
        self.0.state.fetch_and(ADMITTED, Ordering::AcqRel);
    }
}

impl Gate {
    pub fn is_closed(&self) -> bool {
        self.state.load(Ordering::Acquire) & CLOSED != 0
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

    /// Close the gate and wait until every admitted piece of work has
    /// ended. `Ok(None)` if another closer holds it; `Err` (with the gate
    /// reopened) if admitted work did not end within `timeout`.
    pub async fn close(self: &Arc<Self>, timeout: Duration) -> Result<Option<Closed>, usize> {
        if self.state.fetch_or(CLOSED, Ordering::AcqRel) & CLOSED != 0 {
            return Ok(None);
        }
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

/// Requests allowed while the gate is closed (and that need no admission):
/// reads (except opening a desktop, which wakes a computer), Stop work,
/// stopping a computer, and preparing or abandoning an update (which close
/// and reopen the gate themselves).
pub fn allowed(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;
    if path.ends_with("/maintenance/update") {
        return matches!(*method, Method::POST | Method::DELETE);
    }
    if matches!(*method, Method::GET | Method::HEAD) {
        return !path.ends_with("/desktop");
    }
    *method == Method::POST && (path.ends_with("/cancel") || path.ends_with("/computer/stop"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let closed = gate.close(Duration::from_secs(1)).await.unwrap().unwrap();
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
        // Admitted work holds the gate open past the timeout.
        assert_eq!(gate.close(Duration::from_millis(50)).await.err(), Some(1));
        assert!(!gate.is_closed(), "a failed close reopens");
        let closer = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.close(Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(gate.enter().is_none(), "nothing new while closing");
        drop(admitted);
        let closed = closer.await.unwrap().unwrap().unwrap();
        assert!(gate.is_closed());
        assert!(
            gate.close(Duration::from_millis(1))
                .await
                .unwrap()
                .is_none()
        );
        drop(closed);
        assert!(gate.enter().is_some());
    }
}
