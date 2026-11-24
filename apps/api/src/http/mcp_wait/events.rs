//! An event wait's subscription. Push comes first; polling takes over when
//! the event offers only that, or push keeps failing or is refused. A wait
//! counts as live only while a saved cursor lets a reconnect replay what it
//! misses; at every other moment its status calls check the job.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::super::AppState;
use crate::db::mcp_waits::{self, EventChange, EventSpec, Watched};
use crate::mcp::events::{
    Delivery, Descriptor, Endpoint, Failure, Occurrence, Polled, Pushed, SUGGESTED_HEARTBEAT,
    Subscription, matches,
};
use crate::mcp::{Library, PackageId};
use futures_util::future::BoxFuture;
use sqlx::SqlitePool;

const START_TIMEOUT: Duration = Duration::from_secs(20);
/// The part of [`START_TIMEOUT`] a first poll may take, so push still gets
/// a try.
const START_POLL_TIMEOUT: Duration = Duration::from_secs(8);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Push streams that end before delivering anything, after which polling
/// takes over when the event offers it.
const PUSH_ATTEMPTS: u32 = 3;
/// Task secrets change without closing the package's session, so a stream
/// reopens this often to pick them up.
const STREAM_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// Whether subscription task `task` can replay every event it misses, so its
/// wait's status calls can pause.
pub(super) struct Liveness {
    pub task: tokio::task::Id,
    pub live: bool,
}

fn subscription(event: &EventSpec) -> Subscription {
    Subscription {
        name: event.name.clone(),
        arguments: event.arguments.clone(),
    }
}

fn unreplayable(name: &str) -> Failure {
    Failure::Unsupported(format!(
        "{name} cannot be replayed, so polling it cannot see an event."
    ))
}

async fn endpoint(library: &Library, mcp_id: &str, rejected: bool) -> Result<Endpoint, Failure> {
    let id = PackageId::try_from(mcp_id.to_owned())
        .map_err(|message| Failure::Rejected(message.into()))?;
    Ok(library.events_endpoint(&id, rejected).await?)
}

/// The server's descriptor of `event`, once it confirms it offers events
/// and the type has a mode else.bot can use.
async fn descriptor(endpoint: &Endpoint, mcp_id: &str, name: &str) -> Result<Descriptor, Failure> {
    if !endpoint.offers_events().await? {
        return Err(Failure::Unsupported(format!(
            "{mcp_id} does not offer MCP events."
        )));
    }
    let descriptor = endpoint.descriptor(name).await?.ok_or_else(|| {
        Failure::Rejected(format!(
            "{mcp_id} has no usable event type named {name}; see events/list."
        ))
    })?;
    if descriptor.delivery.is_empty() {
        return Err(Failure::Unsupported(format!(
            "{name} offers only webhook delivery, which else.bot does not use."
        )));
    }
    Ok(descriptor)
}

/// Subscribe to `event` for a new wait and return the cursor its
/// subscription starts from. An error means the wait uses status calls
/// only.
pub(super) async fn start(
    library: &Library,
    mcp_id: &str,
    event: &EventSpec,
) -> Result<Option<String>, Failure> {
    let starting = async {
        match head(library, mcp_id, event, false).await {
            Err(Failure::Unauthorized) => head(library, mcp_id, event, true).await,
            started => started,
        }
    };
    tokio::time::timeout(START_TIMEOUT, starting)
        .await
        .unwrap_or_else(|_| Err(Failure::Transient("Subscribing timed out.".into())))
}

/// The server's current position in `event`'s stream: an empty poll, or a
/// push stream's confirmation when polling is not offered or fails.
async fn head(
    library: &Library,
    mcp_id: &str,
    event: &EventSpec,
    rejected: bool,
) -> Result<Option<String>, Failure> {
    let endpoint = endpoint(library, mcp_id, rejected).await?;
    let descriptor = descriptor(&endpoint, mcp_id, &event.name).await?;
    head_on(&endpoint, &descriptor, event).await
}

async fn head_on(
    endpoint: &Endpoint,
    descriptor: &Descriptor,
    event: &EventSpec,
) -> Result<Option<String>, Failure> {
    let subscription = subscription(event);
    let push = descriptor.delivery.contains(&Delivery::Push);
    if descriptor.delivery.contains(&Delivery::Poll) {
        let polled = tokio::time::timeout(
            START_POLL_TIMEOUT,
            endpoint.poll(&subscription, descriptor, None),
        )
        .await
        .unwrap_or_else(|_| Err(Failure::Transient("The first poll timed out.".into())));
        match polled {
            Ok(Polled {
                cursor: Some(cursor),
                ..
            }) => return Ok(Some(cursor)),
            Ok(_) if !push => return Err(unreplayable(&event.name)),
            Err(failure) if !push || matches!(failure, Failure::Paused | Failure::Unauthorized) => {
                return Err(failure);
            }
            _ => {}
        }
    }
    let mut stream = endpoint.stream(&subscription, descriptor, None).await?;
    loop {
        if let Pushed::Active { cursor, .. } = stream.next().await? {
            return Ok(cursor);
        }
    }
}

enum Ended {
    /// The wait settled or no longer watches its event.
    Done,
    Refused(String),
    /// It broke and may work again. `held` when it had worked, which
    /// resets the backoff.
    Broke {
        reason: String,
        held: bool,
    },
    /// Subscribe again at once from the saved cursor, which replays what
    /// the reopening misses.
    Reopen,
    /// The saved cursor no longer holds: start from the server's position
    /// now and check the status for what was skipped.
    Restart,
}

struct Watch {
    state: Arc<AppState>,
    wait: Watched,
    report: mpsc::Sender<Liveness>,
    push_failures: u32,
    /// The server refused the last token, so the next one is refreshed.
    rejected: bool,
}

impl Watch {
    async fn report(&self, live: bool) {
        let task = tokio::task::id();
        let _ = self.report.send(Liveness { task, live }).await;
    }

    async fn report_live(&self) {
        if self.wait.cursor.is_some() {
            self.report(true).await;
        }
    }

    fn ended(&mut self, failure: Failure, held: bool) -> Ended {
        match failure {
            Failure::Unsupported(reason) | Failure::Rejected(reason) => Ended::Refused(reason),
            Failure::SchemaChanged => Ended::Restart,
            failure => {
                self.rejected |= matches!(failure, Failure::Unauthorized);
                Ended::Broke {
                    reason: failure.message().to_owned(),
                    held,
                }
            }
        }
    }

    fn changed(
        &mut self,
        change: Result<EventChange, sqlx::Error>,
        cursor: Option<String>,
    ) -> Result<(), Ended> {
        match change {
            Ok(EventChange::Applied(_)) => {
                if cursor.is_some() {
                    self.wait.cursor = cursor;
                }
                Ok(())
            }
            Ok(EventChange::Ended) => Err(Ended::Done),
            Err(error) => Err(Ended::Broke {
                reason: format!("Saving the event position failed: {error}"),
                held: true,
            }),
        }
    }

    /// Run `write` inside the package's actor while `by`, the endpoint
    /// whose response caused it, still matches the package's configuration
    /// and sign-in. A stale endpoint reopens instead.
    async fn guarded<R: Send + 'static>(
        &mut self,
        by: &Endpoint,
        write: impl FnOnce(SqlitePool) -> BoxFuture<'static, Result<R, sqlx::Error>> + Send + 'static,
    ) -> Result<R, Ended> {
        let Ok(id) = PackageId::try_from(self.wait.mcp_id.clone()) else {
            return Err(Ended::Refused("The wait names no valid MCP.".into()));
        };
        let db = self.state.db.clone();
        match self
            .state
            .mcps
            .while_current(&id, by, move || write(db))
            .await
        {
            Ok(Some(Ok(written))) => Ok(written),
            Ok(Some(Err(error))) => Err(Ended::Broke {
                reason: format!("Saving the event wait failed: {error}"),
                held: true,
            }),
            Ok(None) => Err(Ended::Reopen),
            Err(error) => Err(self.unavailable(error.into())),
        }
    }

    /// The package could not be reached: it may be paused, replaced or
    /// signing in, so try again rather than drop the event.
    fn unavailable(&mut self, failure: Failure) -> Ended {
        self.rejected |= matches!(failure, Failure::Unauthorized);
        Ended::Broke {
            reason: failure.message().to_owned(),
            held: false,
        }
    }

    async fn advance(&mut self, cursor: Option<String>, by: &Endpoint) -> Result<(), Ended> {
        if cursor.is_none() || cursor == self.wait.cursor {
            return Ok(());
        }
        let (wait, saved) = (self.wait.id, cursor.clone());
        let change = self
            .guarded(by, move |db| {
                Box::pin(async move { mcp_waits::advance(&db, wait, saved.as_deref()).await })
            })
            .await?;
        self.changed(Ok(change), cursor)
    }

    async fn gap(&mut self, cursor: Option<String>, by: &Endpoint) -> Result<(), Ended> {
        let (wait, saved) = (self.wait.id, cursor.clone());
        let change = self
            .guarded(by, move |db| {
                Box::pin(
                    async move { mcp_waits::open_gap(&db, wait, saved.as_deref(), false).await },
                )
            })
            .await?;
        self.changed(Ok(change), cursor)
    }

    /// Drop the event after a refusal that came through `by`, unless `by`
    /// is stale, so an old sign-in cannot turn events off for a new one.
    async fn refuse(&mut self, by: &Endpoint, reason: String) -> Ended {
        let wait = self.wait.id;
        let dropped = self
            .guarded(by, move |db| {
                Box::pin(async move { mcp_waits::drop_event(&db, wait).await })
            })
            .await;
        match dropped {
            Ok(_) => {
                tracing::warn!(%wait, %reason, "an MCP event wait uses status calls only");
                Ended::Done
            }
            Err(ended) => ended,
        }
    }

    /// The event type's schema changed: rediscover it, take the server's
    /// position now, and open a gap for what was skipped, all through
    /// `endpoint`.
    async fn restart(&mut self, endpoint: &Endpoint) -> Ended {
        let restarting = async {
            let descriptor = descriptor(endpoint, &self.wait.mcp_id, &self.wait.event.name).await?;
            head_on(endpoint, &descriptor, &self.wait.event).await
        };
        let started = tokio::select! {
            biased;
            () = endpoint.revoked().cancelled() => return Ended::Reopen,
            started = tokio::time::timeout(START_TIMEOUT, restarting) => started
                .unwrap_or_else(|_| Err(Failure::Transient("Subscribing timed out.".into()))),
        };
        let cursor = match started {
            Ok(cursor) => cursor,
            Err(failure) => return self.ended(failure, false),
        };
        let (wait, saved) = (self.wait.id, cursor.clone());
        let change = self
            .guarded(endpoint, move |db| {
                Box::pin(
                    async move { mcp_waits::open_gap(&db, wait, saved.as_deref(), true).await },
                )
            })
            .await;
        match change {
            Ok(EventChange::Applied(_)) => {
                self.wait.cursor = cursor;
                Ended::Reopen
            }
            Ok(EventChange::Ended) => Ended::Done,
            Err(ended) => ended,
        }
    }

    /// Settle the wait on `event` if it is the one waited for, inside the
    /// package's actor and only while `received` still matches the package's
    /// configuration and sign-in.
    async fn offer(&mut self, event: Occurrence, received: &Endpoint) -> Result<(), Ended> {
        let spec = &self.wait.event;
        if event.name != spec.name || !matches(&event.data, &spec.matching) {
            return self.advance(event.cursor, received).await;
        }
        let (wait, raw) = (self.wait.id, event.raw.to_string());
        let settled = self
            .guarded(received, move |db| {
                Box::pin(async move { mcp_waits::settle_on_event(&db, wait, &raw).await })
            })
            .await?;
        if matches!(settled, EventChange::Applied(_)) {
            self.state.scheduler.wake();
        }
        Err(Ended::Done)
    }

    async fn subscribe(&mut self) -> Ended {
        let rejected = std::mem::take(&mut self.rejected);
        let endpoint = match endpoint(&self.state.mcps, &self.wait.mcp_id, rejected).await {
            Ok(endpoint) => endpoint,
            Err(failure) => return self.unavailable(failure),
        };
        let described = tokio::select! {
            biased;
            () = endpoint.revoked().cancelled() => return Ended::Reopen,
            described = descriptor(&endpoint, &self.wait.mcp_id, &self.wait.event.name) => described,
        };
        let descriptor = match described {
            Ok(descriptor) => descriptor,
            Err(failure) => {
                return match self.ended(failure, false) {
                    Ended::Refused(reason) => self.refuse(&endpoint, reason).await,
                    ended => ended,
                };
            }
        };
        let push = descriptor.delivery.contains(&Delivery::Push);
        let poll = descriptor.delivery.contains(&Delivery::Poll);
        let ended = if !push || (poll && self.push_failures >= PUSH_ATTEMPTS) {
            self.poll(&endpoint, &descriptor).await
        } else {
            match self.push(&endpoint, &descriptor).await {
                Ended::Refused(reason) if poll => {
                    self.push_failures = PUSH_ATTEMPTS;
                    return Ended::Broke { reason, held: true };
                }
                ended => ended,
            }
        };
        match ended {
            Ended::Refused(reason) => self.refuse(&endpoint, reason).await,
            Ended::Restart => self.restart(&endpoint).await,
            ended => ended,
        }
    }

    async fn push(&mut self, endpoint: &Endpoint, descriptor: &Descriptor) -> Ended {
        let subscription = subscription(&self.wait.event);
        let opened = tokio::select! {
            biased;
            () = endpoint.revoked().cancelled() => return Ended::Reopen,
            opened = endpoint.stream(&subscription, descriptor, self.wait.cursor.as_deref()) => opened,
        };
        let mut stream = match opened {
            Ok(stream) => stream,
            Err(failure) => {
                self.push_failures += 1;
                return self.ended(failure, false);
            }
        };
        let mut received_after_confirmation = false;
        // A stream silent for three heartbeats is dead (SEP-3415).
        let mut heartbeat = SUGGESTED_HEARTBEAT;
        let reopen = tokio::time::Instant::now() + STREAM_LIFETIME;
        loop {
            let next = tokio::select! {
                biased;
                () = endpoint.revoked().cancelled() => return Ended::Reopen,
                () = tokio::time::sleep_until(reopen) => return Ended::Reopen,
                next = tokio::time::timeout(heartbeat * 3, stream.next()) => next,
            };
            let next = match next {
                Ok(Ok(next)) => next,
                Ok(Err(failure)) => {
                    if !received_after_confirmation {
                        self.push_failures += 1;
                    }
                    return self.ended(failure, received_after_confirmation);
                }
                Err(_) => {
                    if !received_after_confirmation {
                        self.push_failures += 1;
                    }
                    return Ended::Broke {
                        reason: "The event stream went silent.".into(),
                        held: received_after_confirmation,
                    };
                }
            };
            if !received_after_confirmation && !matches!(next, Pushed::Active { .. }) {
                received_after_confirmation = true;
                self.push_failures = 0;
            }
            let step = match next {
                Pushed::Active {
                    cursor,
                    truncated,
                    heartbeat: granted,
                } => {
                    heartbeat = granted;
                    let saved = if truncated {
                        self.gap(cursor, endpoint).await
                    } else {
                        self.advance(cursor, endpoint).await
                    };
                    if saved.is_ok() {
                        self.report_live().await;
                    }
                    saved
                }
                Pushed::Event(event) => self.offer(event, endpoint).await,
                Pushed::Heartbeat { cursor } => self.advance(cursor, endpoint).await,
                Pushed::Error(error) => {
                    tracing::debug!(wait = %self.wait.id, %error, "an MCP event delivery failed");
                    Ok(())
                }
            };
            if let Err(ended) = step {
                return ended;
            }
        }
    }

    async fn poll(&mut self, endpoint: &Endpoint, descriptor: &Descriptor) -> Ended {
        let subscription = subscription(&self.wait.event);
        let mut held = false;
        loop {
            let polled = tokio::select! {
                biased;
                () = endpoint.revoked().cancelled() => return Ended::Reopen,
                polled = endpoint.poll(&subscription, descriptor, self.wait.cursor.as_deref()) => polled,
            };
            let polled = match polled {
                Ok(polled) => polled,
                Err(failure) => return self.ended(failure, held),
            };
            for event in polled.events {
                if let Err(ended) = self.offer(event, endpoint).await {
                    return ended;
                }
            }
            if polled.cursor.is_none() {
                return Ended::Refused(unreplayable(&self.wait.event.name).message().to_owned());
            }
            let saved = if polled.truncated {
                self.gap(polled.cursor, endpoint).await
            } else {
                self.advance(polled.cursor, endpoint).await
            };
            if let Err(ended) = saved {
                return ended;
            }
            if !held {
                held = true;
                self.report_live().await;
            }
            if !polled.has_more {
                tokio::select! {
                    biased;
                    () = endpoint.revoked().cancelled() => return Ended::Reopen,
                    () = tokio::time::sleep(polled.next_poll) => {}
                }
            }
        }
    }
}

/// Keep `wait`'s event subscription until the wait settles, stops watching,
/// or the server refuses the subscription for good, which drops the event
/// and leaves the wait to its status calls.
pub(super) async fn watch(state: Arc<AppState>, wait: Watched, report: mpsc::Sender<Liveness>) {
    let mut watch = Watch {
        state,
        wait,
        report,
        push_failures: 0,
        rejected: false,
    };
    let id = watch.wait.id;
    let mut backoff = MIN_BACKOFF;
    loop {
        let ended = watch.subscribe().await;
        watch.report(false).await;
        match ended {
            Ended::Done => return,
            Ended::Refused(reason) => {
                tracing::warn!(wait = %id, %reason, "an MCP event wait uses status calls only");
                if let Err(error) = mcp_waits::drop_event(&watch.state.db, id).await {
                    tracing::warn!(wait = %id, %error, "dropping an MCP event failed");
                }
                return;
            }
            Ended::Broke { reason, held } => {
                tracing::debug!(wait = %id, %reason, "an MCP event subscription broke");
                if held {
                    backoff = MIN_BACKOFF;
                }
                let jitter = Duration::from_millis(
                    u64::try_from(uuid::Uuid::new_v4().as_u128() % 1000).unwrap_or_default(),
                );
                tokio::time::sleep(backoff + jitter).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Ended::Reopen | Ended::Restart => backoff = MIN_BACKOFF,
        }
    }
}
