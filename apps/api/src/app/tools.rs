//! The tool inventory shared by provider configuration and MCP transport.
//! Adapters verify this exact allowlist before running a turn.

pub const SERVER_NAME: &str = "elsebot";
/// The tools this server offers. Adapters allow exactly these.
pub const TOOL_NAMES: &[&str] = &[
    "memory_bots",
    "memory_search",
    "memory_read",
    "memory_view",
    "memory_zoom",
    "memory_date",
    "delegate",
    "create_bot",
    "computer_run",
    "computer_job_start",
    "computer_job",
    "computer_job_cancel",
    "computer_write_file",
    "computer_read_file",
    "computer_fetch",
    "computer_browser",
    "computer_secrets",
    "computer_screenshot",
    "computer_input",
];
