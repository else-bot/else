use std::fs;
use std::path::{Path, PathBuf};

use super::cli::files::{GRANT_ENV, link_login, replace_private};
use super::host::BotDirs;
use crate::app::tools::SERVER_NAME;

/// Grok's built-in host tools: the name given to `--tools`, and the name its
/// inventory and tool calls report.
pub const HOST_TOOLS: &[(&str, &str)] = &[
    ("run_terminal_cmd", "run_terminal_command"),
    ("read_file", "read_file"),
    ("write", "write"),
    ("search_replace", "search_replace"),
    ("list_dir", "list_dir"),
    ("grep", "grep"),
    ("get_task_output", "get_command_or_subagent_output"),
    ("kill_task", "kill_command_or_subagent"),
];
/// The turn's packet, given as Grok's prompt file.
pub const PROMPT_FILE: &str = "prompt.txt";
pub const MCP_TOOLS: &[&str] = &["search_tool", "use_tool"];

pub fn host_home() -> Option<PathBuf> {
    std::env::var_os("GROK_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".grok")))
}

/// A stable, non-secret identity of the host login (its users and teams),
/// used only inside session fingerprints.
pub fn host_account(login: &Path) -> String {
    let Ok(text) = fs::read_to_string(login.join("auth.json")) else {
        return String::new();
    };
    let Ok(serde_json::Value::Object(entries)) = serde_json::from_str(&text) else {
        return String::new();
    };
    let mut accounts: Vec<String> = entries
        .values()
        .map(|entry| {
            format!(
                "{}:{}",
                entry["user_id"].as_str().unwrap_or_default(),
                entry["team_id"].as_str().unwrap_or_default()
            )
        })
        .collect();
    accounts.sort();
    accounts.join(",")
}

pub fn env(home: &Path) -> Vec<(String, String)> {
    let mut env = vec![("GROK_HOME".into(), home.to_string_lossy().into_owned())];
    for (key, value) in [
        ("GROK_DISABLE_AUTOUPDATER", "1"),
        ("GROK_SUBAGENTS", "0"),
        ("GROK_WORKFLOWS", "0"),
        ("GROK_MEMORY", "0"),
        ("GROK_BACKEND_SEARCH", "0"),
        ("GROK_CAMPAIGNS", "0"),
        ("GROK_DISABLE_EXPERIMENTS", "1"),
    ] {
        env.push((key.into(), value.into()));
    }
    env
}

const CONFIG: &str = r#"
[cli]
auto_update = false
use_leader = false
[features]
managed_config = false
campaigns = false
[compat.claude]
agents = false
hooks = false
mcps = false
rules = false
skills = false
[compat.cursor]
agents = false
hooks = false
mcps = false
rules = false
skills = false
[compat.codex]
hooks = false
skills = false
[subagents]
enabled = false
[workflows]
enabled = false
[managed_mcps]
enabled = false
gateway_tools_enabled = false
[memory]
enabled = false
[memory_v2]
enabled = false
[session]
load_envrc = false
[relay]
enabled = false
[marketplace]
official_marketplace_auto_installed = true
default_skills_installs_purged = true
"#;

pub fn prepare(
    dirs: &BotDirs,
    login: &Path,
    tool_url: Option<&str>,
    prompt: &str,
    host_tools: bool,
) -> std::io::Result<()> {
    link_login(&dirs.provider_home, "auth.json", &login.join("auth.json"))?;
    let mut config = CONFIG.to_owned();
    config.push_str(&format!(
        "\n[permission]\nallow = [\"MCPTool({SERVER_NAME}__*)\"]\n"
    ));
    if !host_tools {
        config.push_str("deny = [\"Read(**)\"]\n");
    }
    if let Some(url) = tool_url {
        config.push_str(&format!(
            "\n[mcp_servers.{SERVER_NAME}]\nurl = {}\nstartup_timeout_sec = 30\n\
             [mcp_servers.{SERVER_NAME}.headers]\nAuthorization = \"Bearer ${{{GRANT_ENV}}}\"\n",
            serde_json::to_string(url)?,
        ));
    }
    replace_private(&dirs.provider_home, "config.toml", config.as_bytes())?;
    replace_private(&dirs.provider_home, PROMPT_FILE, prompt.as_bytes())?;
    let allowed = tool_url.map_or_else(
        || "[]".into(),
        |url| format!("[{{serverUrl = {}}}]", serde_json::to_string(url).unwrap()),
    );
    replace_private(
        &dirs.provider_home,
        "managed_config.toml",
        format!("allow_managed_hooks_only = true\nallowedMcpServers = {allowed}\n").as_bytes(),
    )
}

pub fn session_exists(home: &Path, project: &Path, id: &str) -> bool {
    if uuid::Uuid::parse_str(id).is_err() {
        return false;
    }
    let Ok(project) = project.canonicalize() else {
        return false;
    };
    const ESCAPE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    let encoded =
        percent_encoding::utf8_percent_encode(&project.to_string_lossy(), ESCAPE).to_string();
    let sessions = home.join("sessions");
    if sessions
        .join(encoded)
        .join(id)
        .join("chat_history.jsonl")
        .is_file()
    {
        return true;
    }
    fs::read_dir(sessions).is_ok_and(|groups| {
        groups.filter_map(Result::ok).any(|group| {
            fs::read_to_string(group.path().join(".cwd"))
                .is_ok_and(|cwd| Path::new(cwd.trim_end()) == project)
                && group.path().join(id).join("chat_history.jsonl").is_file()
        })
    })
}
