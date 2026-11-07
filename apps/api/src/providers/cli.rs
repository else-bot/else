//! What every provider CLI attempt shares: process ownership and cleanup,
//! bounded output framing, private files, session bookkeeping, and the stop
//! lifecycle. Protocols, permissions, and homes stay with each adapter.
//!
//! A new CLI provider supplies its [`super::definitions::Definition`], a home
//! module that writes its configuration and enforces its tool policy, and a
//! protocol that validates its events ([`attempt::Protocol`]). A CLI that
//! streams events for one turn runs through [`stream::StreamAttempt`]; one
//! with its own transport implements [`attempt::Channel`] and is driven by
//! [`attempt::drive`]. Either way [`session::PreparedTurn`] owns the packet
//! and the session advance, and [`attempt::conclude`] the cleanup.

pub mod attempt;
pub mod files;
pub mod lines;
pub mod process;
pub mod session;
pub mod stream;
