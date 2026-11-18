//! The owner's reads use their own signed-in session per account, apart
//! from the account worker, so opening an email never waits behind a
//! bot's command. The session stays open between reads until it idles
//! out, so a repeat read skips connecting and signing in.

use super::types::{Fetched, MailError, MailReader, MessageRef};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;
use uuid::Uuid;

/// How long an unused session stays open.
pub(super) const IDLE: Duration = Duration::from_secs(4 * 60);

struct Warm {
    generation: i64,
    session: Box<dyn MailReader>,
    used: Instant,
}

type Slot = Arc<Mutex<Option<Warm>>>;

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
    slots: crate::actor::Actor<HashMap<Uuid, Slot>>,
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
    async fn slot(&self, account_id: Uuid) -> Slot {
        self.slots
            .call(move |slots| slots.entry(account_id).or_default().clone())
            .await
    }

    /// Closes the account's session, after any read on it finishes.
    pub async fn close(&self, account_id: Uuid) {
        let slot = self
            .slots
            .call(move |slots| slots.get(&account_id).cloned())
            .await;
        if let Some(slot) = slot {
            *slot.lock().await = None;
        }
    }

    /// Reads `message` on the account's session for `generation`. `open`
    /// signs in a new session when there is none, it idled out, or it
    /// failed; a warm session that fails is replaced once.
    pub async fn read<F, Fut>(
        &self,
        account_id: Uuid,
        generation: i64,
        message: &MessageRef,
        open: F,
    ) -> (Result<Fetched, MailError>, Path)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Box<dyn MailReader>, MailError>>,
    {
        let slot = self.slot(account_id).await;
        let Ok(mut guard) = slot.clone().try_lock_owned() else {
            let result = match open().await {
                Ok(mut session) => session.read_message(message).await,
                Err(error) => Err(error),
            };
            return (result, Path::Spare);
        };
        let warm = guard
            .take()
            .filter(|w| w.generation == generation && w.used.elapsed() < IDLE);
        let (result, session, path) = match warm {
            Some(mut warm) => match warm.session.read_message(message).await {
                Ok(fetched) => (Ok(fetched), Some(warm.session), Path::Warm),
                Err(error) if about_message(&error) => (Err(error), Some(warm.session), Path::Warm),
                Err(_) => {
                    let (result, session) = fresh(open, message).await;
                    (result, session, Path::Cold)
                }
            },
            None => {
                let (result, session) = fresh(open, message).await;
                (result, session, Path::Cold)
            }
        };
        if let Some(session) = session {
            *guard = Some(Warm {
                generation,
                session,
                used: Instant::now(),
            });
            drop(guard);
            tokio::spawn(async move {
                tokio::time::sleep(IDLE).await;
                let mut warm = slot.lock().await;
                if warm.as_ref().is_some_and(|w| w.used.elapsed() >= IDLE) {
                    *warm = None;
                }
            });
        }
        (result, path)
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts sessions opened. Each session answers `uid` 9, fails with a
    /// dropped connection once `broken` is set, and waits for `release`
    /// before answering `uid` 12.
    #[derive(Default)]
    struct Server {
        opened: AtomicUsize,
        broken: std::sync::atomic::AtomicBool,
        release: tokio::sync::Notify,
    }
    struct Session(Arc<Server>);

    #[async_trait::async_trait]
    impl MailReader for Session {
        async fn read_message(&mut self, message: &MessageRef) -> Result<Fetched, MailError> {
            if self.0.broken.swap(false, Ordering::SeqCst) {
                return Err(MailError::new("mail_transport_failed"));
            }
            match message.uid {
                9 => {}
                12 => self.0.release.notified().await,
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

    async fn read(
        readers: &Readers,
        server: &Arc<Server>,
        generation: i64,
        message: u32,
    ) -> (Result<Fetched, MailError>, Path) {
        let server = server.clone();
        readers
            .read(Uuid::nil(), generation, &uid(message), || async move {
                server.opened.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Session(server)) as Box<dyn MailReader>)
            })
            .await
    }

    fn opened(server: &Server) -> usize {
        server.opened.load(Ordering::SeqCst)
    }

    #[tokio::test(start_paused = true)]
    async fn repeat_reads_share_one_session_until_it_idles_out() {
        let (readers, server) = (Readers::default(), Arc::new(Server::default()));
        assert_eq!(read(&readers, &server, 1, 9).await.1, Path::Cold);
        assert_eq!(read(&readers, &server, 1, 9).await.1, Path::Warm);
        let (missing, path) = read(&readers, &server, 1, 10).await;
        assert_eq!(missing.err().map(|e| e.code), Some("message_not_found"));
        assert_eq!(path, Path::Warm, "a missing message keeps the session");
        assert_eq!(opened(&server), 1);
        tokio::time::advance(IDLE).await;
        assert_eq!(read(&readers, &server, 1, 9).await.1, Path::Cold);
        assert_eq!(
            read(&readers, &server, 2, 9).await.1,
            Path::Cold,
            "new generation"
        );
        readers.close(Uuid::nil()).await;
        assert_eq!(read(&readers, &server, 2, 9).await.1, Path::Cold);
        assert_eq!(opened(&server), 4);
    }

    #[tokio::test]
    async fn a_dropped_session_reconnects_within_the_same_read() {
        let (readers, server) = (Readers::default(), Arc::new(Server::default()));
        read(&readers, &server, 1, 9).await.0.unwrap();
        server.broken.store(true, Ordering::SeqCst);
        let (result, path) = read(&readers, &server, 1, 9).await;
        assert!(result.is_ok());
        assert_eq!(path, Path::Cold);
        assert_eq!(opened(&server), 2);
    }

    #[tokio::test]
    async fn a_read_while_the_session_is_busy_signs_in_a_spare() {
        let (readers, server) = (Arc::new(Readers::default()), Arc::new(Server::default()));
        let slow = tokio::spawn({
            let (readers, server) = (readers.clone(), server.clone());
            async move { read(&readers, &server, 1, 12).await.1 }
        });
        while opened(&server) == 0 {
            tokio::task::yield_now().await;
        }
        assert_eq!(read(&readers, &server, 1, 9).await.1, Path::Spare);
        server.release.notify_one();
        assert_eq!(slow.await.unwrap(), Path::Cold);
        assert_eq!(read(&readers, &server, 1, 9).await.1, Path::Warm);
    }
}
