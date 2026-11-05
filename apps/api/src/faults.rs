//! Named fault points for recovery tests.
//!
//! An armed point makes the operation that reaches it stop right there, as
//! if the API process died at that moment: the operation returns without
//! doing anything further, leaving SQLite and the runtime exactly as a
//! crash would. Tests then start a fresh manager (a restarted API, whose
//! state lives only in SQLite and the runtime) and check recovery.
//! Points are inert unless a test arms them; production code never does.

use std::sync::atomic::{AtomicUsize, Ordering};

static POINTS: [(&str, AtomicUsize); 8] = [
    ("backup.snapshot", AtomicUsize::new(0)),
    ("backup.before_complete", AtomicUsize::new(0)),
    ("replication.after_part", AtomicUsize::new(0)),
    ("job.after_insert", AtomicUsize::new(0)),
    ("job.after_launch", AtomicUsize::new(0)),
    ("computer.after_start_decision", AtomicUsize::new(0)),
    ("lease.after_renew", AtomicUsize::new(0)),
    ("computer.after_stop_decision", AtomicUsize::new(0)),
];

/// Make the next operation reaching `point` stop there.
pub fn arm(point: &'static str) {
    let (_, count) = POINTS
        .iter()
        .find(|(name, _)| *name == point)
        .expect("known fault point");
    count.fetch_add(1, Ordering::Relaxed);
}

/// Whether to stop at `point` (consumes the arming).
pub fn hit(point: &str) -> bool {
    let Some((_, count)) = POINTS.iter().find(|(name, _)| *name == point) else {
        return false;
    };
    if count
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
        .is_err()
    {
        return false;
    }
    tracing::warn!(point, "fault point reached: stopping as if the API died");
    true
}
