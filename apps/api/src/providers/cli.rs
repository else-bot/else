//! What every provider CLI attempt shares: process ownership and cleanup,
//! bounded output framing, private files, session bookkeeping, and the stop
//! lifecycle. Protocols, permissions, and homes stay with each adapter.

pub mod attempt;
pub mod files;
pub mod lines;
pub mod process;
pub mod session;
pub mod stream;
