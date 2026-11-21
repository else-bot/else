//! The seam between the scheduler and the features whose state machines
//! advance on every tick: routines, watchers, webhooks, and mail and social
//! deliveries. Each feature logs its own failures. Their order matters
//! where they share limits, so it is spelled out here.

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use crate::computers::ComputerManager;
pub use crate::db::routines::Check;
use crate::providers::Registry;

/// What one pass runs with. The scheduler holds a maintenance admission
/// for the whole pass.
pub struct Tick<'a> {
    pub pool: &'a SqlitePool,
    pub registry: &'a Registry,
    /// Where checks run; without computers no check is reserved.
    pub computers: Option<&'a ComputerManager>,
    pub now: DateTime<Utc>,
    /// Launch a reserved check job.
    pub launch: &'a (dyn Fn(Check) + Send + Sync),
}

/// What the scheduler settles before each claim pass, after deferred
/// messages: the review of ended routine runs writes into conversations
/// their next turn reads.
pub(super) async fn before_claim(tick: &Tick<'_>) {
    crate::db::routines::pump_review(tick).await;
}

/// Advance every feature once. Routine check settling and watchers go
/// first, in the order that shares the bots' job slots and turn queues as
/// before; the remaining admissions run concurrently, as they did before.
pub(super) async fn all(tick: &Tick<'_>) {
    crate::db::routines::pump_settle(tick).await;
    crate::db::watchers::pump(tick).await;
    tokio::join!(
        crate::db::routines::pump_admit(tick),
        crate::db::webhooks::pump(tick),
        crate::connectors::delivery::pump(tick),
        crate::socials::delivery::pump(tick),
    );
}
