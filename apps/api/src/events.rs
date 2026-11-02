//! In-process fan-out of committed-change notifications to event streams.
//! A single PostgreSQL listener receives the bot IDs that `db::events`
//! publishes with `pg_notify` and rebroadcasts them. Notifications are only
//! wake hints: streams also poll the journal every second.

use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::db::events::CHANNEL;

/// `Uuid::nil()` wakes every stream, e.g. after the listener reconnects.
#[derive(Clone)]
pub struct EventHub {
    tx: broadcast::Sender<Uuid>,
}

impl EventHub {
    /// A hub with no database listener; streams rely on polling.
    pub fn polling_only() -> Self {
        Self {
            tx: broadcast::channel(256).0,
        }
    }

    /// Start the listener task. It reconnects with backoff until `stop`.
    pub fn start(pool: PgPool, stop: CancellationToken) -> Self {
        let hub = Self::polling_only();
        let tx = hub.tx.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(200);
            loop {
                let session = async {
                    let mut listener = PgListener::connect_with(&pool).await?;
                    listener.listen(CHANNEL).await?;
                    // Anything missed while disconnected is found by polling.
                    let _ = tx.send(Uuid::nil());
                    loop {
                        let notification = listener.recv().await?;
                        if let Ok(bot_id) = notification.payload().parse() {
                            let _ = tx.send(bot_id);
                        }
                    }
                    #[allow(unreachable_code)]
                    Ok::<(), sqlx::Error>(())
                };
                tokio::select! {
                    _ = stop.cancelled() => return,
                    result = session => {
                        if let Err(err) = result {
                            tracing::warn!(error = %err, "event listener disconnected");
                        }
                    }
                }
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        });
        hub
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Uuid> {
        self.tx.subscribe()
    }
}
