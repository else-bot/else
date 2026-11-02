//! else.bot API library. The `else-api` binary composes these modules; the
//! integration tests drive them directly against PostgreSQL.

pub mod app;
pub mod config;
pub mod credentials;
pub mod db;
pub mod events;
pub mod http;
pub mod providers;
pub mod runtime_lock;
pub mod scheduler;
pub mod views;
