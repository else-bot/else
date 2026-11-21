//! The features whose state machines advance on every tick: routines,
//! watchers, webhooks, and mail and social deliveries. Their order matters
//! where they share limits, so it is spelled out here.

use std::fmt::Display;

use super::Scheduler;
use crate::connector_core::inbound;
use crate::connectors::delivery::Mail;
use crate::db::routines::{self, Check};
use crate::db::{watchers, webhooks};
use crate::socials::delivery::Social;

/// Pending mail or social pairs admitted per pass.
const DELIVERY_BATCH: i64 = 32;

/// Advance every feature once, launching each check it reserves. The
/// caller holds a maintenance admission for the whole pass. Routines go
/// first: the review of ended runs queues their retries, then finished
/// checks settle, so on a nearly full queue a retry takes a bot's last slot
/// ahead of a watcher wake. Watchers go next because they share the bots'
/// job slots and turn queues; the remaining admissions run concurrently.
pub(super) async fn all(scheduler: &Scheduler, launch: &(dyn Fn(Check) + Send + Sync)) {
    let (pool, registry) = (&scheduler.pool, &scheduler.registry);
    let computers = scheduler.computers.as_deref();
    let now = chrono::Utc::now();
    logged(
        "reviewing ended routine runs",
        routines::review_ended(pool, registry, now).await,
    );
    logged(
        "settling routine checks",
        routines::settle_checks(pool, registry, routines::SETTLE_BATCH).await,
    );
    logged(
        "settling watcher checks",
        watchers::settle_checks(pool, now, watchers::BATCH).await,
    );
    logged(
        "admitting due watchers",
        watchers::admit_due(pool, computers, now, watchers::BATCH, launch).await,
    );
    logged(
        "queueing watcher wakes",
        watchers::deliver_wakes(pool, registry, now, watchers::BATCH).await,
    );
    tokio::join!(
        async {
            let admitted =
                routines::admit_due(pool, registry, computers, now, routines::ADMIT_BATCH);
            if let Some(admitted) = logged("admitting due routines", admitted.await) {
                admitted.checks.into_iter().for_each(launch);
            }
        },
        async {
            let admitted = webhooks::admit_requested(pool, registry, now, webhooks::ADMIT_BATCH);
            logged("admitting webhook wakes", admitted.await);
        },
        async {
            let admitted = inbound::admit_pending::<Mail>(pool, registry, DELIVERY_BATCH);
            logged("admitting email deliveries", admitted.await);
        },
        async {
            let admitted = inbound::admit_pending::<Social>(pool, registry, DELIVERY_BATCH);
            logged("admitting social deliveries", admitted.await);
        },
    );
}

/// Log a failed step and go on; the next pass retries it.
fn logged<T>(step: &str, result: Result<T, impl Display>) -> Option<T> {
    result
        .inspect_err(|err| tracing::warn!(error = %err, "{step} failed"))
        .ok()
}
