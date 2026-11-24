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

const START_TIMEOUT: Duration = Duration::from_secs(20);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Push streams that end before delivering anything, after which polling
/// takes over when the event offers it.
const PUSH_ATTEMPTS: u32 = 3;
/// How long a push stream stays open before it reopens with the package's
/// current configuration and sign-in.
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
        let endpoint = endpoint(library, mcp_id, false).await?;
        let descriptor = descriptor(&endpoint, mcp_id, &event.name).await?;
        let subscription = subscription(event);
        let push = descriptor.delivery.contains(&Delivery::Push);
        if descriptor.delivery.contains(&Delivery::Poll) {
            match endpoint.poll(&subscription, &descriptor, None).await {
                Ok(Polled {
                    cursor: Some(cursor),
                    ..
                }) => return Ok(Some(cursor)),
                Ok(_) if !push => return Err(unreplayable(&event.name)),
                Err(failure) if !push || matches!(failure, Failure::Paused) => {
                    return Err(failure);
                }
                _ => {}
            }
        }
        let mut stream = endpoint.stream(&subscription, &descriptor, None).await?;
        loop {
            if let Pushed::Active { cursor, .. } = stream.next().await? {
                return Ok(cursor);
            }
        }
    };
    tokio::time::timeout(START_TIMEOUT, starting)
        .await
        .unwrap_or_else(|_| Err(Failure::Transient("Subscribing timed out.".into())))
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
}

struct Watch {
    state: Arc<AppState>,
    wait: Watched,
    report: mpsc::Sender<Liveness>,
    /// At [`PUSH_ATTEMPTS`], push is off and polling takes over.
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

    async fn advance(&mut self, cursor: Option<String>) -> Result<(), Ended> {
        if cursor.is_none() || cursor == self.wait.cursor {
            return Ok(());
        }
        let change = mcp_waits::advance(&self.state.db, self.wait.id, cursor.as_deref()).await;
        self.changed(change, cursor)
    }

    /// The server skipped events before `cursor`, so the job may have
    /// stopped unseen: the wait's status calls resume until one sees it
    /// running.
    async fn gap(&mut self, cursor: Option<String>) -> Result<(), Ended> {
        let change = mcp_waits::open_gap(&self.state.db, self.wait.id, cursor.as_deref()).await;
        self.changed(change, cursor)
    }

    /// Settle the wait on `event` if it is the one waited for. `opened` is
    /// the endpoint a push stream was opened with, which must still match
    /// the package's current one.
    async fn offer(&mut self, event: Occurrence, opened: Option<&Endpoint>) -> Result<(), Ended> {
        let spec = &self.wait.event;
        if event.name != spec.name || !matches(&event.data, &spec.matching) {
            return self.advance(event.cursor).await;
        }
        if let Some(opened) = opened {
            match endpoint(&self.state.mcps, &self.wait.mcp_id, false).await {
                Ok(current) if current.same_target(opened) => {}
                Ok(_) => return Err(Ended::Reopen),
                Err(failure) => return Err(self.ended(failure, true)),
            }
        }
        let settled =
            mcp_waits::settle_on_event(&self.state.db, self.wait.id, &event.raw.to_string()).await;
        match settled {
            Ok(EventChange::Applied(_)) => {
                self.state.scheduler.wake();
                Err(Ended::Done)
            }
            Ok(EventChange::Ended) => Err(Ended::Done),
            Err(error) => Err(Ended::Broke {
                reason: format!("Settling the wait failed: {error}"),
                held: true,
            }),
        }
    }

    async fn subscribe(&mut self) -> Ended {
        let rejected = std::mem::take(&mut self.rejected);
        let endpoint = match endpoint(&self.state.mcps, &self.wait.mcp_id, rejected).await {
            Ok(endpoint) => endpoint,
            Err(failure) => return self.ended(failure, false),
        };
        let descriptor = match descriptor(&endpoint, &self.wait.mcp_id, &self.wait.event.name).await
        {
            Ok(descriptor) => descriptor,
            Err(failure) => return self.ended(failure, false),
        };
        let push = descriptor.delivery.contains(&Delivery::Push);
        let poll = descriptor.delivery.contains(&Delivery::Poll);
        if !push || (poll && self.push_failures >= PUSH_ATTEMPTS) {
            return self.poll(&descriptor).await;
        }
        match self.push(&endpoint, &descriptor).await {
            Ended::Refused(reason) if poll => {
                self.push_failures = PUSH_ATTEMPTS;
                Ended::Broke { reason, held: true }
            }
            ended => ended,
        }
    }

    async fn push(&mut self, endpoint: &Endpoint, descriptor: &Descriptor) -> Ended {
        let subscription = subscription(&self.wait.event);
        let mut stream = match endpoint
            .stream(&subscription, descriptor, self.wait.cursor.as_deref())
            .await
        {
            Ok(stream) => stream,
            Err(failure) => {
                self.push_failures += 1;
                return self.ended(failure, false);
            }
        };
        // Held once a message follows the confirmation, so a stream that
        // confirms and closes counts as a failure.
        let mut held = false;
        // A stream silent for three heartbeats is dead (SEP-3415).
        let mut heartbeat = SUGGESTED_HEARTBEAT;
        let reopen = tokio::time::Instant::now() + STREAM_LIFETIME;
        loop {
            let next = tokio::select! {
                next = tokio::time::timeout(heartbeat * 3, stream.next()) => next,
                () = tokio::time::sleep_until(reopen) => return Ended::Reopen,
            };
            let next = match next {
                Ok(Ok(next)) => next,
                Ok(Err(failure)) => {
                    if !held {
                        self.push_failures += 1;
                    }
                    return self.ended(failure, held);
                }
                Err(_) => {
                    if !held {
                        self.push_failures += 1;
                    }
                    return Ended::Broke {
                        reason: "The event stream went silent.".into(),
                        held,
                    };
                }
            };
            if !held && !matches!(next, Pushed::Active { .. }) {
                held = true;
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
                        self.gap(cursor).await
                    } else {
                        self.advance(cursor).await
                    };
                    if saved.is_ok() {
                        self.report_live().await;
                    }
                    saved
                }
                Pushed::Event(event) => self.offer(event, Some(endpoint)).await,
                Pushed::Heartbeat { cursor } => self.advance(cursor).await,
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

    async fn poll(&mut self, descriptor: &Descriptor) -> Ended {
        let subscription = subscription(&self.wait.event);
        let mut held = false;
        loop {
            // A fresh endpoint each time, so a new sign-in or configuration
            // applies.
            let rejected = std::mem::take(&mut self.rejected);
            let polled = match endpoint(&self.state.mcps, &self.wait.mcp_id, rejected).await {
                Ok(endpoint) => {
                    endpoint
                        .poll(&subscription, descriptor, self.wait.cursor.as_deref())
                        .await
                }
                Err(failure) => Err(failure),
            };
            let polled = match polled {
                Ok(polled) => polled,
                Err(failure) => return self.ended(failure, held),
            };
            for event in polled.events {
                if let Err(ended) = self.offer(event, None).await {
                    return ended;
                }
            }
            if polled.cursor.is_none() {
                return Ended::Refused(unreplayable(&self.wait.event.name).message().to_owned());
            }
            let saved = if polled.truncated {
                self.gap(polled.cursor).await
            } else {
                self.advance(polled.cursor).await
            };
            if let Err(ended) = saved {
                return ended;
            }
            if !held {
                held = true;
                self.report_live().await;
            }
            if !polled.has_more {
                tokio::time::sleep(polled.next_poll).await;
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
        if matches!(ended, Ended::Reopen) {
            backoff = MIN_BACKOFF;
            continue;
        }
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
            Ended::Reopen => {}
        }
    }
}
