//! The offline else-api integration tests, linked into one binary so an API
//! change relinks one test executable instead of one per file. Live,
//! replication, and fault-injection tests (`backup`, `recovery`) stay separate
//! targets in `tests/`: `else_api::faults` is process-global, so an armed point
//! would stop another module's operation when plain `cargo test` runs modules
//! on threads of one process.

#[path = "../common/mod.rs"]
mod common;

mod admission_profiles;
mod assistant_backup;
mod assistant_images;
mod attachments;
mod backup_settings;
mod bot_councils;
mod bot_descriptions;
mod bot_profiles;
mod bot_renaming;
mod bot_rules;
mod bots;
mod cancel;
mod categories;
mod cli_attempts;
mod computer_handoffs;
mod computer_job_waits;
mod computers;
mod connector_store;
mod default_skills;
mod delegation;
mod delegation_files;
mod desktop_stream;
mod else_status;
mod email_api;
mod github_oauth_build;
mod guest_scripts;
mod handoff;
mod host;
mod installation;
mod mcp_oauth;
mod mcp_waits;
mod memory;
mod memory_summary;
mod memory_tree;
mod onboarding;
mod outgoing_drafts;
mod owner_questions;
mod reads;
mod reset;
mod routine_checks;
mod routine_grants;
mod routine_models;
mod routines;
mod runtime_lock;
mod screens;
mod secret_requests;
mod server_name;
mod shared_mcp;
mod skills;
mod social_sessions;
mod social_triggers;
mod socials;
mod sqlite;
mod task_permissions;
mod task_secrets;
mod tool_permissions;
mod turns;
mod update_gate;
mod visual_preview;
mod webhooks;
mod ws;
