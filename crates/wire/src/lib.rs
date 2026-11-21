//! The v1 wire types the API sends and the desktop reads: HTTP response
//! bodies and `/v1/ws` stream events. The server builds them from its
//! records, so a missing or renamed field fails to compile on both sides.
//!
//! The desktop must keep reading older and newer servers. Fields a server
//! may omit keep `#[serde(default)]`, no type denies unknown fields, and
//! states a newer server may extend decode through `wire_state!` or an
//! `Unknown` variant. A type the desktop also sends stays strict and its
//! received fields fall back instead, so the desktop never sends a value it
//! could not read.

#[macro_use]
mod state;

pub mod backups;
pub mod bots;
pub mod computers;
pub mod conversation;
pub mod delegations;
pub mod email;
pub mod items;
pub mod maintenance;
pub mod mcp;
pub mod notices;
pub mod outgoing;
pub mod questions;
pub mod routines;
pub mod rules;
pub mod secrets;
pub mod settings;
pub mod skills;
pub mod socials;
pub mod turns;
pub mod usage;
pub mod waits;
