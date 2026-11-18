//! The owner's reads use their own signed-in session per account, apart
//! from the account worker, so opening an email never waits behind a
//! bot's command. The session stays open between reads until it idles
//! out, so a repeat read skips connecting and signing in.
//!
//! Each account's slot carries an epoch. `close` raises it before it
//! clears the session, and a read keeps its session only while the epoch
//! it started under is current, so a read in flight during a pause or a
//! credential change never leaves a session behind.

use super::types::{Fetched, MailError, MailReader, MessageRef};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::Instant;
use uuid::Uuid;

/// How long an unused session stays open.
pub(super) const IDLE: Duration = Duration::from_secs(4 * 60);

struct Warm {
    epoch: u64,
    session: Box<dyn MailReader>,
    used: Instant,
}

/// One account's kept session, and the single spare a read may sign in
/// while another read holds it.
struct Slot {
    warm: Mutex<Option<Warm>>,
    spare: Semaphore,
}

/// How a read reached the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Path {
    /// On the account's open session.
    Warm,
    /// On a new session, kept open for the next read.
    Cold,
    /// On a session for this read only, because another read holds the
    /// account's session.
    Spare,
}

impl Path {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warm => "warm",
            Self::Cold => "cold",
            Self::Spare => "spare",
        }
    }
}

#[derive(Default)]
pub(super) struct Readers {
    slots: crate::actor::Actor<HashMap<Uuid, (u64, Arc<Slot>)>>,
}

/// An error about the message itself, after which the session still works.
fn about_message(error: &MailError) -> bool {
    matches!(
        error.code,
        "message_not_found"
            | "stale_message_reference"
            | "invalid_message_uid"
            | "invalid_mail_request"
            | "invalid_email_message"
            | "mail_message_too_large"
    )
}

impl Readers {
    async fn slot(&self, account_id: Uuid) -> (u64, Arc<Slot>) {
        self.slots
            .call(move |slots| {
                slots
                    .entry(account_id)
                    .or_insert_with(|| {
                        let slot = Slot {
                            warm: Mutex::new(None),
                            spare: Semaphore::new(1),
                        };
                        (0, Arc::new(slot))
                    })
                    .clone()
            })
            .await
    }

    /// Whether `slot` is still the account's slot at `epoch`.
    async fn current(&self, account_id: Uuid, slot: &Arc<Slot>, epoch: u64) -> bool {
        let slot = slot.clone();
        self.slots
            .call(move |slots| {
                slots
                    .get(&account_id)
                    .is_some_and(|(e, s)| *e == epoch && Arc::ptr_eq(s, &slot))
            })
            .await
    }

    /// Ends the account's session. A read under way keeps no session, and
    /// `forget` also drops the slot of a removed account.
    pub async fn close(&self, account_id: Uuid, forget: bool) {
        let slot = self
            .slots
            .call(move |slots| {
                if forget {
                    return slots.remove(&account_id).map(|(_, slot)| slot);
                }
                slots.get_mut(&account_id).map(|(epoch, slot)| {
                    *epoch += 1;
                    slot.clone()
                })
            })
            .await;
        if let Some(slot) = slot {
            *slot.warm.lock().await = None;
        }
    }

    /// Reads `message` on the account's session. `open` signs in a new
    /// session when there is none, it idled out, or it failed; a warm
    /// session that fails is replaced once.
    pub async fn read<F, Fut>(
        &self,
        account_id: Uuid,
        message: &MessageRef,
        open: F,
    ) -> (Result<Fetched, MailError>, Path)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Box<dyn MailReader>, MailError>>,
    {
        let (epoch, slot) = self.slot(account_id).await;
        let mut guard = match slot.warm.try_lock() {
            Ok(guard) => guard,
            Err(_) => match slot.spare.try_acquire() {
                Ok(_spare) => {
                    let result = match open().await {
                        Ok(mut session) => session.read_message(message).await,
                        Err(error) => Err(error),
                    };
                    return (result, Path::Spare);
                }
                Err(_) => slot.warm.lock().await,
            },
        };
        let was_open = guard.is_some();
        let warm = guard
            .take()
            .filter(|w| w.epoch == epoch && w.used.elapsed() < IDLE);
        let (result, session, path) = match warm {
            Some(mut warm) => match warm.session.read_message(message).await {
                Ok(fetched) => (Ok(fetched), Some(warm.session), Path::Warm),
                Err(error) if about_message(&error) => (Err(error), Some(warm.session), Path::Warm),
                Err(_) => {
                    drop(warm);
                    let (result, session) = fresh(open, message).await;
                    (result, session, Path::Cold)
                }
            },
            None => {
                let (result, session) = fresh(open, message).await;
                (result, session, Path::Cold)
            }
        };
        if let Some(session) = session
            && self.current(account_id, &slot, epoch).await
        {
            *guard = Some(Warm {
                epoch,
                session,
                used: Instant::now(),
            });
            drop(guard);
            if !was_open {
                tokio::spawn(sweep(slot));
            }
        }
        (result, path)
    }
}

/// Closes the slot's session once it has gone `IDLE` unused.
async fn sweep(slot: Arc<Slot>) {
    let mut wait = IDLE;
    loop {
        tokio::time::sleep(wait).await;
        let mut warm = slot.warm.lock().await;
        match warm.as_ref().map(|w| w.used.elapsed()) {
            Some(idle) if idle < IDLE => wait = IDLE - idle,
            Some(_) => {
                *warm = None;
                return;
            }
            None => return,
        }
    }
}

async fn fresh<F, Fut>(
    open: F,
    message: &MessageRef,
) -> (Result<Fetched, MailError>, Option<Box<dyn MailReader>>)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Box<dyn MailReader>, MailError>>,
{
    let mut session = match open().await {
        Ok(session) => session,
        Err(error) => return (Err(error), None),
    };
    match session.read_message(message).await {
        Ok(fetched) => (Ok(fetched), Some(session)),
        Err(error) if about_message(&error) => (Err(error), Some(session)),
        Err(error) => (Err(error), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Notify;

    /// Counts sessions opened and closed. Each session answers `uid` 9,
    /// fails with a dropped connection once `broken` is set, and before
    /// answering `uid` 12 says it started and waits for `release`.
    #[derive(Default)]
    struct Server {
        opened: AtomicUsize,
        closed: AtomicUsize,
        broken: AtomicBool,
        started: Notify,
        release: Notify,
    }
    struct Session(Arc<Server>);

    impl Drop for Session {
        fn drop(&mut self) {
            self.0.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl MailReader for Session {
        async fn read_message(&mut self, message: &MessageRef) -> Result<Fetched, MailError> {
            if self.0.broken.swap(false, Ordering::SeqCst) {
                return Err(MailError::new("mail_transport_failed"));
            }
            match message.uid {
                9 => {}
                12 => {
                    self.0.started.notify_one();
                    self.0.release.notified().await;
                }
                _ => return Err(MailError::new("message_not_found")),
            }
            Ok(Fetched {
                raw: b"raw".as_slice().into(),
                received_at: None,
            })
        }
    }

    fn uid(uid: u32) -> MessageRef {
        MessageRef {
            folder: "INBOX".into(),
            uid_validity: 1,
            uid,
        }
    }

    async fn read(readers: &Readers, server: &Arc<Server>, message: u32) -> Path {
        let server = server.clone();
        let (_, path) = readers
            .read(Uuid::nil(), &uid(message), || async move {
                server.opened.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Session(server)) as Box<dyn MailReader>)
            })
            .await;
        path
    }

    fn count(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::SeqCst)
    }

    #[tokio::test(start_paused = true)]
    async fn repeat_reads_share_one_session_until_it_idles_out() {
        let (readers, server) = (Readers::default(), Arc::new(Server::default()));
        assert_eq!(read(&readers, &server, 9).await, Path::Cold);
        assert_eq!(read(&readers, &server, 9).await, Path::Warm);
        assert_eq!(
            read(&readers, &server, 10).await,
            Path::Warm,
            "a missing message keeps it"
        );
        tokio::time::advance(IDLE - Duration::from_secs(1)).await;
        assert_eq!(
            read(&readers, &server, 9).await,
            Path::Warm,
            "use restarts the idle time"
        );
        tokio::time::sleep(IDLE + Duration::from_secs(1)).await;
        assert_eq!(count(&server.closed), 1, "closed while nothing reads");
        assert_eq!(read(&readers, &server, 9).await, Path::Cold);
        assert_eq!(count(&server.opened), 2);
    }

    #[tokio::test]
    async fn a_dropped_session_closes_before_it_reconnects_within_the_read() {
        let (readers, server) = (Readers::default(), Arc::new(Server::default()));
        read(&readers, &server, 9).await;
        server.broken.store(true, Ordering::SeqCst);
        let replaced = server.clone();
        let (result, path) = readers
            .read(Uuid::nil(), &uid(9), || async move {
                assert_eq!(
                    count(&replaced.closed),
                    1,
                    "the broken session closed first"
                );
                replaced.opened.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Session(replaced)) as Box<dyn MailReader>)
            })
            .await;
        assert!(result.is_ok());
        assert_eq!(path, Path::Cold);
        assert_eq!(read(&readers, &server, 9).await, Path::Warm);
    }

    #[tokio::test]
    async fn a_read_in_flight_when_the_account_closes_keeps_no_session() {
        let (readers, server) = (Arc::new(Readers::default()), Arc::new(Server::default()));
        read(&readers, &server, 9).await;
        let slow = tokio::spawn({
            let (readers, server) = (readers.clone(), server.clone());
            async move { read(&readers, &server, 12).await }
        });
        server.started.notified().await;
        let closing = tokio::spawn({
            let readers = readers.clone();
            async move { readers.close(Uuid::nil(), false).await }
        });
        while readers.slots.call(|slots| slots[&Uuid::nil()].0).await == 0 {
            tokio::task::yield_now().await;
        }
        server.release.notify_one();
        assert_eq!(slow.await.unwrap(), Path::Warm);
        closing.await.unwrap();
        assert_eq!(read(&readers, &server, 9).await, Path::Cold);
        assert_eq!(count(&server.opened), 2);
    }

    #[tokio::test]
    async fn busy_reads_sign_in_one_spare_and_the_rest_wait() {
        let (readers, server) = (Arc::new(Readers::default()), Arc::new(Server::default()));
        let held = tokio::spawn({
            let (readers, server) = (readers.clone(), server.clone());
            async move { read(&readers, &server, 12).await }
        });
        server.started.notified().await;
        let spare = tokio::spawn({
            let (readers, server) = (readers.clone(), server.clone());
            async move { read(&readers, &server, 12).await }
        });
        server.started.notified().await;
        let waiting = tokio::spawn({
            let (readers, server) = (readers.clone(), server.clone());
            async move { read(&readers, &server, 9).await }
        });
        tokio::task::yield_now().await;
        assert_eq!(count(&server.opened), 2, "one kept session and one spare");
        server.release.notify_one();
        server.release.notify_one();
        let mut paths = [held.await.unwrap(), spare.await.unwrap()];
        paths.sort_by_key(|p| p.as_str());
        assert_eq!(paths, [Path::Cold, Path::Spare]);
        assert_eq!(waiting.await.unwrap(), Path::Warm);
        assert_eq!(count(&server.opened), 2);
    }
}
