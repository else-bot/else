use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::host::{BotDirs, link_login};

pub const GRANT_ENV: &str = "ELSE_TASK_GRANT";
pub const HOST_TOOLS: &[&str] = &[
    "run_terminal_cmd",
    "read_file",
    "write",
    "search_replace",
    "list_dir",
    "grep",
    "get_task_output",
    "kill_task",
];
pub const HOST_TOOL_NAMES: &[&str] = &[
    "run_terminal_command",
    "read_file",
    "write",
    "search_replace",
    "list_dir",
    "grep",
    "get_command_or_subagent_output",
    "kill_command_or_subagent",
];
pub const MCP_TOOLS: &[&str] = &["search_tool", "use_tool"];

pub fn host_home() -> Option<PathBuf> {
    std::env::var_os("GROK_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".grok")))
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
    config.push_str("\n[permission]\nallow = [\"MCPTool(elsebot__*)\"]\n");
    if !host_tools {
        config.push_str("deny = [\"Read(**)\"]\n");
    }
    if let Some(url) = tool_url {
        config.push_str(&format!(
            "\n[mcp_servers.elsebot]\nurl = {}\nstartup_timeout_sec = 30\n\
             [mcp_servers.elsebot.headers]\nAuthorization = \"Bearer ${{{GRANT_ENV}}}\"\n",
            serde_json::to_string(url)?,
        ));
    }
    replace(&dirs.provider_home, "config.toml", &config)?;
    replace(&dirs.provider_home, "prompt.txt", prompt)?;
    let allowed = tool_url.map_or_else(
        || "[]".into(),
        |url| format!("[{{serverUrl = {}}}]", serde_json::to_string(url).unwrap()),
    );
    replace(
        &dirs.provider_home,
        "managed_config.toml",
        &format!("allow_managed_hooks_only = true\nallowedMcpServers = {allowed}\n"),
    )
}

fn replace(dir: &Path, name: &str, contents: &str) -> std::io::Result<()> {
    if !fs::symlink_metadata(dir)?.is_dir() {
        return Err(std::io::Error::other("provider home is not a directory"));
    }
    let temp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(contents.as_bytes())?;
    fs::rename(temp, dir.join(name))
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
