//! else.bot API library. The `else-api` binary composes these modules; the
//! integration tests drive them directly against SQLite.

/// This build's version: the newest release it contains (see `build_support/release_version.rs`).
pub const VERSION: &str = env!("ELSE_VERSION");

mod actor;
pub mod app;
pub mod backup;
pub mod computers;
pub mod config;
pub mod connectors;
pub mod credentials;
pub mod db;
pub mod events;
pub mod faults;
pub mod grants;
pub mod handoff;
pub mod http;
pub mod maintenance;
pub mod mcp;
pub mod memory_worker;
pub mod providers;
pub mod runtime_lock;
pub mod scheduler;
mod server_updates;
pub mod views;

pub mod socials;
