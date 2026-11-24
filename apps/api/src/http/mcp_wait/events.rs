//! An event wait's subscription: push first, polling when the server
//! offers only that or push keeps failing. The status call covers every
//! moment the subscription is not live, so no wait is lost to a dropped
//! stream, a restart, or a server that stops offering the event.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

use super::super::AppState;
use crate::db::mcp_waits::{self, EventChange, EventSpec, Watched};
use crate::mcp::events::{
    Delivery, Descriptor, Endpoint, Failure, Occurrence, Pushed, SUGGESTED_HEARTBEAT, Subscription,
    matches,
};
use crate::mcp::{Library, PackageId};

const START_TIMEOUT: Duration = Duration::from_secs(20);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Push streams that fail before they open, after which polling takes over
/// when the event offers it.
const PUSH_ATTEMPTS: u32 = 3;

/// Whether a wait's subscription can see every event, so its status calls
/// can pause.
pub(super) enum Liveness {
    Live(Uuid),
    Down(Uuid),
}

fn subscription(event: &EventSpec) -> Subscription {
    Subscription {
        name: event.name.clone(),
        arguments: event.arguments.clone(),
    }
}

async fn endpoint(library: &Library, mcp_id: &str) -> Result<Endpoint, Failure> {
    let id = PackageId::try_from(mcp_id.to_owned())
        .map_err(|message| Failure::Rejected(message.into()))?;
    Ok(library.events_endpoint(&id).await?)
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
            "{mcp_id} has no event type named {name}; see events/list."
        ))
    })?;
    if descriptor.delivery.is_empty() {
        return Err(Failure::Unsupported(format!(
            "{name} offers only webhook delivery, which else.bot does not use."
        )));
    }
    Ok(descriptor)
}

/// Subscribe to `event` for a new wait and return where its subscription
/// starts, before the first status call. An error means the wait uses
/// status calls only.
pub(super) async fn start(
    library: &Library,
    mcp_id: &str,
    event: &EventSpec,
) -> Result<Option<String>, Failure> {
    let starting = async {
        let endpoint = endpoint(library, mcp_id).await?;
        let descriptor = descriptor(&endpoint, mcp_id, &event.name).await?;
        let subscription = subscription(event);
        if descriptor.delivery.contains(&Delivery::Poll) {
            let polled = endpoint.poll(&subscription, &descriptor, None).await?;
            if polled.cursor.is_some() || descriptor.delivery.contains(&Delivery::Push) {
                return Ok(polled.cursor);
            }
            return Err(Failure::Unsupported(format!(
                "{} cannot be replayed, so polling it cannot see an event.",
                event.name
            )));
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

/// How one subscription ended.
enum Ended {
    /// The wait settled or no longer watches its event.
    Done,
    /// The server refused it for good.
    Refused(String),
    /// It broke and may work again. `opened` when it had been live.
    Broke { reason: String, opened: bool },
}

impl From<Failure> for Ended {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Unsupported(reason) | Failure::Rejected(reason) => Self::Refused(reason),
            Failure::Transient(reason) => Self::Broke {
                reason,
                opened: false,
            },
            Failure::Paused => Self::Broke {
                reason: Failure::Paused.message().into(),
                opened: false,
            },
        }
    }
}

/// One wait's subscription state across reconnects.
struct Watch {
    state: Arc<AppState>,
    wait: Watched,
    report: mpsc::Sender<Liveness>,
    push_failures: u32,
}

impl Watch {
    fn id(&self) -> Uuid {
        self.wait.status.id
    }

    async fn report(&self, liveness: Liveness) {
        let _ = self.report.send(liveness).await;
    }

    /// Save a new position. `Ended` stops the subscription.
    async fn advance(&mut self, cursor: Option<String>) -> Result<(), Ended> {
        if cursor == self.wait.cursor {
            return Ok(());
        }
        match mcp_waits::advance(&self.state.db, self.id(), cursor.as_deref()).await {
            Ok(EventChange::Applied) => {
                self.wait.cursor = cursor;
                Ok(())
            }
            Ok(EventChange::Ended) => Err(Ended::Done),
            Err(error) => Err(Ended::Broke {
                reason: format!("Saving the event position failed: {error}"),
                opened: true,
            }),
        }
    }

    /// Settle the wait on `event` if it is the one waited for.
    async fn offer(&mut self, event: Occurrence) -> Result<(), Ended> {
        let spec = &self.wait.event;
        if event.name != spec.name || !matches(&event.data, &spec.matching) {
            return self.advance(event.cursor).await;
        }
        match mcp_waits::settle_on_event(&self.state.db, self.id(), &event.raw.to_string()).await {
            Ok(EventChange::Applied) => {
                self.state.scheduler.wake();
                Err(Ended::Done)
            }
            Ok(EventChange::Ended) => Err(Ended::Done),
            Err(error) => Err(Ended::Broke {
                reason: format!("Settling the wait failed: {error}"),
                opened: true,
            }),
        }
    }

    /// The server skipped events, so the job may have stopped unseen: call
    /// the status tool once.
    async fn gap(&self) {
        super::check_and_record(self.state.clone(), self.wait.status.clone()).await;
    }

    /// Mark the subscription live once it can replay what it misses.
    async fn opened(&self) {
        if self.wait.cursor.is_some() {
            self.report(Liveness::Live(self.id())).await;
        }
    }

    async fn subscribe(&mut self) -> Ended {
        let mcp_id = self.wait.status.mcp_id.clone();
        let endpoint = match endpoint(&self.state.mcps, &mcp_id).await {
            Ok(endpoint) => endpoint,
            Err(failure) => return failure.into(),
        };
        let descriptor = match descriptor(&endpoint, &mcp_id, &self.wait.event.name).await {
            Ok(descriptor) => descriptor,
            Err(failure) => return failure.into(),
        };
        let push = descriptor.delivery.contains(&Delivery::Push);
        let poll = descriptor.delivery.contains(&Delivery::Poll);
        if push && (self.push_failures < PUSH_ATTEMPTS || !poll) {
            self.push(&endpoint, &descriptor).await
        } else {
            self.poll(&descriptor).await
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
                return failure.into();
            }
        };
        let mut opened = false;
        // A stream silent for three heartbeats is dead (SEP-3415).
        let mut heartbeat = SUGGESTED_HEARTBEAT;
        loop {
            let next = match tokio::time::timeout(heartbeat * 3, stream.next()).await {
                Ok(Ok(next)) => next,
                Ok(Err(failure)) => {
                    if !opened {
                        self.push_failures += 1;
                    }
                    return match Ended::from(failure) {
                        Ended::Broke { reason, .. } => Ended::Broke { reason, opened },
                        ended => ended,
                    };
                }
                Err(_) => {
                    return Ended::Broke {
                        reason: "The event stream went silent.".into(),
                        opened,
                    };
                }
            };
            let step = match next {
                Pushed::Active {
                    cursor,
                    truncated,
                    heartbeat: granted,
                } => {
                    heartbeat = granted;
                    opened = true;
                    self.push_failures = 0;
                    let saved = self.advance(cursor).await;
                    if saved.is_ok() {
                        self.opened().await;
                        if truncated {
                            self.gap().await;
                        }
                    }
                    saved
                }
                Pushed::Event(event) => self.offer(event).await,
                Pushed::Heartbeat { cursor } => self.advance(cursor).await,
                Pushed::Error(error) => {
                    tracing::debug!(wait = %self.id(), %error, "an MCP event delivery failed");
                    Ok(())
                }
            };
            if let Err(ended) = step {
                return ended;
            }
        }
    }

    async fn poll(&mut self, descriptor: &Descriptor) -> Ended {
        let mcp_id = self.wait.status.mcp_id.clone();
        let subscription = subscription(&self.wait.event);
        let mut opened = false;
        loop {
            // A fresh endpoint each time, so a refreshed sign-in applies.
            let polled = match endpoint(&self.state.mcps, &mcp_id).await {
                Ok(endpoint) => {
                    endpoint
                        .poll(&subscription, descriptor, self.wait.cursor.as_deref())
                        .await
                }
                Err(failure) => Err(failure),
            };
            let polled = match polled {
                Ok(polled) => polled,
                Err(failure) => {
                    return match Ended::from(failure) {
                        Ended::Broke { reason, .. } => Ended::Broke { reason, opened },
                        ended => ended,
                    };
                }
            };
            for event in polled.events {
                if let Err(ended) = self.offer(event).await {
                    return ended;
                }
            }
            if polled.cursor.is_none() {
                return Ended::Refused(format!(
                    "{} cannot be replayed, so polling it cannot see an event.",
                    self.wait.event.name
                ));
            }
            if let Err(ended) = self.advance(polled.cursor).await {
                return ended;
            }
            if !opened {
                opened = true;
                self.opened().await;
            }
            if polled.truncated {
                self.gap().await;
            }
            if !polled.has_more {
                tokio::time::sleep(polled.next_poll).await;
            }
        }
    }
}

/// Keep wait `wait`'s event subscription until the wait settles, stops
/// watching, or the server refuses the subscription for good, which drops
/// the event and leaves the wait to its status calls.
pub(super) async fn watch(state: Arc<AppState>, wait: Watched, report: mpsc::Sender<Liveness>) {
    let mut watch = Watch {
        state,
        wait,
        report,
        push_failures: 0,
    };
    let mut backoff = MIN_BACKOFF;
    loop {
        let ended = watch.subscribe().await;
        watch.report(Liveness::Down(watch.id())).await;
        match ended {
            Ended::Done => return,
            Ended::Refused(reason) => {
                tracing::warn!(wait = %watch.id(), %reason, "an MCP event wait uses status calls only");
                if let Err(error) = mcp_waits::drop_event(&watch.state.db, watch.id()).await {
                    tracing::warn!(wait = %watch.id(), %error, "dropping an MCP event failed");
                }
                return;
            }
            Ended::Broke { reason, opened } => {
                tracing::debug!(wait = %watch.id(), %reason, "an MCP event subscription broke");
                if opened {
                    backoff = MIN_BACKOFF;
                }
                let jitter = Duration::from_millis(
                    u64::try_from(Uuid::new_v4().as_u128() % 1000).unwrap_or_default(),
                );
                tokio::time::sleep(backoff + jitter).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}
