use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// `Uuid::nil()` wakes every stream, e.g. after a commit or a reconnect.
#[derive(Clone)]
pub struct EventHub {
    tx: broadcast::Sender<Uuid>,
}

impl EventHub {
    /// A hub with no database watcher; streams rely on polling.
    pub fn polling_only() -> Self {
        Self {
            tx: broadcast::channel(256).0,
        }
    }

    /// Start the watcher task, which polls `PRAGMA data_version` until `stop`
    /// and wakes every stream when it changes.
    pub fn start(pool: SqlitePool, stop: CancellationToken) -> Self {
        let hub = Self::polling_only();
        let tx = hub.tx.clone();
        tokio::spawn(async move {
            let mut conn = None;
            let read = async move || {
                let mut db = match conn.take() {
                    Some(db) => db,
                    None => pool.acquire().await?,
                };
                let version = sqlx::query_scalar::<_, i64>("PRAGMA data_version")
                    .fetch_one(&mut *db)
                    .await?;
                conn = Some(db);
                Ok(version)
            };
            watch(read, tx, stop).await;
        });
        hub
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Uuid> {
        self.tx.subscribe()
    }
}

/// Poll every `POLL` while reads succeed. After consecutive failures, wait
/// twice as long each time up to `MAX_RETRY`, so an outage is not hammered.
async fn watch(
    mut read: impl AsyncFnMut() -> sqlx::Result<i64>,
    tx: broadcast::Sender<Uuid>,
    stop: CancellationToken,
) {
    const POLL: Duration = Duration::from_millis(100);
    const MAX_RETRY: Duration = Duration::from_secs(5);
    let mut version = None;
    let mut delay = POLL;
    loop {
        match read().await {
            Ok(current) => {
                if version != Some(current) {
                    let _ = tx.send(Uuid::nil());
                }
                version = Some(current);
                delay = POLL;
            }
            Err(_) => {
                version = None;
                delay = (delay * 2).min(MAX_RETRY);
            }
        }
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::Instant;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn failed_reads_back_off_until_a_read_succeeds() {
        let mut script = [
            Err(()),
            Err(()),
            Err(()),
            Ok(1),
            Ok(1),
            Ok(2),
            Err(()),
            Err(()),
            Err(()),
            Err(()),
            Err(()),
            Err(()),
            Err(()),
        ]
        .into_iter();
        let start = Instant::now();
        let mut reads = Vec::new();
        let (tx, mut wakes) = broadcast::channel(8);
        let stop = CancellationToken::new();
        let read = async || {
            reads.push(start.elapsed().as_millis());
            match script.next() {
                Some(result) => result.map_err(|()| sqlx::Error::PoolTimedOut),
                None => {
                    stop.cancel();
                    Ok(2)
                }
            }
        };
        watch(read, tx, stop.clone()).await;

        let gaps: Vec<_> = reads.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(
            gaps,
            [
                200, 400, 800, 100, 100, 100, 200, 400, 800, 1600, 3200, 5000, 5000
            ],
            "doubling retry capped at 5 s, 100 ms poll after a success",
        );
        let mut woken = 0;
        while wakes.try_recv().is_ok() {
            woken += 1;
        }
        assert_eq!(
            woken, 3,
            "a wake for each new data version, including after the outage"
        );
    }
}
