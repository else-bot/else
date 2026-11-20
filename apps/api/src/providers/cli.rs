//! What every provider CLI attempt shares: process ownership and cleanup,
//! bounded output framing, private files, session bookkeeping, and the stop
//! lifecycle. Protocols, permissions, and homes stay with each adapter.
//!
//! A new CLI provider registers one [`super::definitions::Definition`] (its
//! profile rules and the constructor the registry calls), a home module
//! that writes its configuration and enforces its tool policy, and a
//! protocol that validates its events ([`attempt::Protocol`]). A CLI that
//! streams events for one turn implements [`turn::Adapter`], and
//! [`turn::Cli`] runs its turns: the order of the sink's lifecycle calls,
//! the session choice, attachments, the task grant, and delivery live there
//! once. Its protocol reads fields through [`json::Fields`], and a
//! stream-json dialect shares [`stream_json`]'s policy checks. A CLI with
//! its own transport implements [`attempt::Channel`] and is driven by
//! [`attempt::drive`]. Either way [`session::PreparedTurn`] owns the packet
//! and the session advance, and [`attempt::conclude`] the cleanup.

pub mod attempt;
pub mod files;
pub mod json;
pub mod lines;
pub mod process;
pub mod session;
pub mod stream;
pub mod stream_json;
pub mod turn;
