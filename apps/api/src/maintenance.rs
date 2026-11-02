//! The maintenance gate for coordinated backups (POC-022). While closed,
//! the scheduler claims no turn and the HTTP layer refuses requests that
//! would change state or wake a computer (except Stop work and stopping a
//! computer), so the database, the bot computers, and the conversation
//! records are captured at one application checkpoint.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Default)]
pub struct Gate {
    closed: AtomicBool,
}

/// Reopens the gate when dropped, however the backup ends.
pub struct Closed(Arc<Gate>);

impl Drop for Closed {
    fn drop(&mut self) {
        self.0.closed.store(false, Ordering::SeqCst);
    }
}

impl Gate {
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Close the gate; `None` if it is already closed.
    pub fn close(self: &Arc<Self>) -> Option<Closed> {
        self.closed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| Closed(self.clone()))
    }
}

/// Requests allowed while the gate is closed: reads (except opening a
/// desktop, which wakes a computer), Stop work, and stopping a computer.
pub fn allowed(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;
    if matches!(*method, Method::GET | Method::HEAD) {
        return !path.ends_with("/desktop");
    }
    *method == Method::POST && (path.ends_with("/cancel") || path.ends_with("/computer/stop"))
}
