//! Named fault points for recovery tests (POC-019).
//!
//! An armed point makes the operation that reaches it stop right there, as
//! if the API process died at that moment: the operation returns without
//! doing anything further, leaving PostgreSQL and the runtime exactly as a
//! crash would. Tests then start a fresh manager (a restarted API, whose
//! state lives only in PostgreSQL and the runtime) and check recovery.
//! Points are inert unless a test arms them; production code never does.

use std::sync::Mutex;

static ARMED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// Make the next operation reaching `point` stop there.
pub fn arm(point: &'static str) {
    ARMED.lock().unwrap_or_else(|p| p.into_inner()).push(point);
}

/// Whether to stop at `point` (consumes the arming).
pub fn hit(point: &str) -> bool {
    let mut armed = ARMED.lock().unwrap_or_else(|p| p.into_inner());
    match armed.iter().position(|p| *p == point) {
        Some(index) => {
            armed.remove(index);
            tracing::warn!(point, "fault point reached: stopping as if the API died");
            true
        }
        None => false,
    }
}
