//! Single-owner state with a bounded mailbox. Admission awaits capacity;
//! accepted operations finish even if their caller is cancelled.

use futures_util::future::BoxFuture;
use tokio::sync::{mpsc, oneshot};

const CAPACITY: usize = 64;
type Message<S> = Box<dyn for<'a> FnOnce(&'a mut S) -> BoxFuture<'a, ()> + Send>;

pub(crate) struct Actor<S> {
    tx: mpsc::Sender<Message<S>>,
}

impl<S> Clone for Actor<S> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<S: Send + 'static> Actor<S> {
    pub fn new(mut state: S) -> Self {
        let (tx, mut rx) = mpsc::channel::<Message<S>>(CAPACITY);
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                message(&mut state).await;
            }
        });
        Self { tx }
    }

    pub async fn call<R: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut S) -> R + Send + 'static,
    ) -> R {
        self.call_async(move |state| Box::pin(async move { operation(state) }))
            .await
    }

    pub async fn call_async<R: Send + 'static>(
        &self,
        operation: impl for<'a> FnOnce(&'a mut S) -> BoxFuture<'a, R> + Send + 'static,
    ) -> R {
        let (tx, rx) = oneshot::channel();
        let message: Message<S> = Box::new(move |state| {
            Box::pin(async move {
                let result = operation(state).await;
                let _ = tx.send(result);
            })
        });
        assert!(self.tx.send(message).await.is_ok(), "state actor stopped");
        rx.await.expect("state actor stopped during an operation")
    }
}

impl<S: Default + Send + 'static> Default for Actor<S> {
    fn default() -> Self {
        Self::new(S::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mailbox_backpressures_and_accepted_work_survives_cancellation() {
        let actor = Actor::new(0usize);
        let (started, ready) = oneshot::channel();
        let (release, blocked) = oneshot::channel();
        let worker = actor.clone();
        let task = tokio::spawn(async move {
            worker
                .call_async(move |state| {
                    Box::pin(async move {
                        let _ = started.send(());
                        let _ = blocked.await;
                        *state += 1;
                    })
                })
                .await;
        });
        ready.await.unwrap();
        task.abort();
        for _ in 0..CAPACITY {
            actor
                .tx
                .try_send(Box::new(|state| Box::pin(async move { *state += 1 })))
                .unwrap_or_else(|_| panic!("mailbox has room"));
        }
        let pending = actor.call(|state| *state);
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut pending)
                .await
                .is_err()
        );
        release.send(()).unwrap();
        assert_eq!(pending.await, CAPACITY + 1);
    }
}
