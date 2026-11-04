//! Bounded in-process wake hints for committed SQLite changes.
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

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
    pub fn start(pool: SqlitePool, stop: CancellationToken) -> Self {
        let hub = Self::polling_only();
        let tx = hub.tx.clone();
        tokio::spawn(async move {
            let mut version = None;
            let mut conn = None;
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            loop {
                tokio::select! { _ = stop.cancelled() => return, _ = interval.tick() => {} }
                if conn.is_none() {
                    conn = pool.acquire().await.ok();
                }
                if let Some(db) = conn.as_mut() {
                    match sqlx::query_scalar::<_, i64>("PRAGMA data_version")
                        .fetch_one(&mut **db)
                        .await
                    {
                        Ok(current) => {
                            if version != Some(current) {
                                let _ = tx.send(Uuid::nil());
                            }
                            version = Some(current);
                        }
                        Err(_) => {
                            conn = None;
                            version = None;
                        }
                    }
                }
            }
        });
        hub
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Uuid> {
        self.tx.subscribe()
    }
}
