//! One worker task per connected account. A worker owns its account's
//! connection; callers reach it through a bounded request queue and a change
//! signal, and stop it by cancelling its token and awaiting its task.

use std::collections::HashMap;
use std::future::Future;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::actor::Actor;

/// Accounts with a running worker at once.
const MAX_WORKERS: usize = 32;
/// Requests one worker queues before callers are told it is busy.
const QUEUE: usize = 16;

/// Why no worker was handed out.
#[derive(Debug)]
pub enum Refused {
    /// The registry is at [`MAX_WORKERS`].
    Full,
    /// [`Workers::shutdown`] has run.
    ShutDown,
}

/// A caller's handle on a running worker.
pub struct Worker<R> {
    requests: mpsc::Sender<R>,
    changed: watch::Sender<u64>,
}

impl<R> Clone for Worker<R> {
    fn clone(&self) -> Self {
        Self {
            requests: self.requests.clone(),
            changed: self.changed.clone(),
        }
    }
}

impl<R> Worker<R> {
    /// Queue a request without waiting; a full queue hands it back.
    pub fn try_send(&self, request: R) -> Result<(), R> {
        self.requests.try_send(request).map_err(|e| e.into_inner())
    }

    /// Tell the worker its account changed and should be reloaded.
    pub fn changed(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
}

/// What a worker task receives: its requests, the change signal, and the
/// token that stops it.
pub struct Inbox<R> {
    pub requests: mpsc::Receiver<R>,
    pub changed: watch::Receiver<u64>,
    pub stop: CancellationToken,
}

struct Slot<R> {
    worker: Worker<R>,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

enum Slots<R> {
    Open(HashMap<Uuid, Slot<R>>),
    ShutDown,
}

pub struct Workers<R> {
    slots: Actor<Slots<R>>,
}

impl<R: Send + 'static> Default for Workers<R> {
    fn default() -> Self {
        Self {
            slots: Actor::new(Slots::Open(HashMap::new())),
        }
    }
}

impl<R: Send + 'static> Workers<R> {
    /// The account's worker, and whether this call started it. A worker
    /// whose task has ended is replaced.
    pub async fn get_or_spawn<F, Fut>(&self, id: Uuid, run: F) -> Result<(Worker<R>, bool), Refused>
    where
        F: FnOnce(Inbox<R>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.slots
            .call(move |slots| {
                let Slots::Open(slots) = slots else {
                    return Err(Refused::ShutDown);
                };
                slots.retain(|_, slot| !slot.task.is_finished());
                if let Some(slot) = slots.get(&id) {
                    return Ok((slot.worker.clone(), false));
                }
                if slots.len() >= MAX_WORKERS {
                    return Err(Refused::Full);
                }
                let (requests, receiver) = mpsc::channel(QUEUE);
                let (changed, revision) = watch::channel(0);
                let stop = CancellationToken::new();
                let worker = Worker { requests, changed };
                let task = tokio::spawn(run(Inbox {
                    requests: receiver,
                    changed: revision,
                    stop: stop.clone(),
                }));
                slots.insert(
                    id,
                    Slot {
                        worker: worker.clone(),
                        stop,
                        task,
                    },
                );
                Ok((worker, true))
            })
            .await
    }

    /// Stop the account's worker and wait for its task to end.
    pub async fn stop(&self, id: Uuid) {
        let removed = self
            .slots
            .call(move |slots| match slots {
                Slots::Open(slots) => slots.remove(&id),
                Slots::ShutDown => None,
            })
            .await;
        if let Some(slot) = removed {
            slot.stop.cancel();
            let _ = slot.task.await;
        }
    }

    /// Stop every worker, refuse new ones, and wait for all of them.
    pub async fn shutdown(&self) {
        let slots: Vec<Slot<R>> = self
            .slots
            .call(|slots| match std::mem::replace(slots, Slots::ShutDown) {
                Slots::Open(slots) => slots.into_values().collect(),
                Slots::ShutDown => Vec::new(),
            })
            .await;
        for slot in &slots {
            slot.stop.cancel();
        }
        futures_util::future::join_all(slots.into_iter().map(|slot| slot.task)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_finished_worker_is_replaced_and_frees_its_place() {
        let workers = Workers::<()>::default();
        let id = Uuid::new_v4();
        workers.get_or_spawn(id, |_| async {}).await.unwrap();
        for _ in 1..MAX_WORKERS {
            workers
                .get_or_spawn(Uuid::new_v4(), |inbox| async move {
                    inbox.stop.cancelled().await
                })
                .await
                .unwrap();
        }
        tokio::task::yield_now().await;
        let (_, started) = workers
            .get_or_spawn(id, |inbox| async move { inbox.stop.cancelled().await })
            .await
            .expect("the finished worker no longer counts against the limit");
        assert!(started, "a finished worker is replaced");
        let (_, started) = workers.get_or_spawn(id, |_| async {}).await.unwrap();
        assert!(!started, "a running worker is reused");
        assert!(matches!(
            workers.get_or_spawn(Uuid::new_v4(), |_| async {}).await,
            Err(Refused::Full)
        ));
    }

    #[tokio::test]
    async fn no_worker_starts_after_shutdown() {
        let workers = Workers::<()>::default();
        workers.shutdown().await;
        assert!(matches!(
            workers.get_or_spawn(Uuid::new_v4(), |_| async {}).await,
            Err(Refused::ShutDown)
        ));
    }

    #[tokio::test]
    async fn stop_waits_for_the_task_to_end() {
        let workers = Workers::<()>::default();
        let (ended, mut finished) = watch::channel(false);
        let id = Uuid::new_v4();
        workers
            .get_or_spawn(id, move |inbox| async move {
                inbox.stop.cancelled().await;
                tokio::task::yield_now().await;
                let _ = ended.send(true);
            })
            .await
            .unwrap();
        workers.stop(id).await;
        assert!(*finished.borrow_and_update());
    }
}
