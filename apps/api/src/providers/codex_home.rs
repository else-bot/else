//! Per-bot Codex home and locked configuration.
//!
//! Each bot gets its own `CODEX_HOME` (sessions, caches, and settings stay
//! separate). The owner's supported ChatGPT login is shared by bind-mounting
//! the host `auth.json` into that home: it is never copied, and token
//! refreshes write through to the one file. The configuration is rewritten on
//! every launch so nothing a session does can widen it: no shell or exec
//! tools, no web search, browser, apps, plugins, hooks, MCP servers, image
//! generation, or native sub-agents, read-only sandbox, never ask for
//! approval.

use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::host::{Bind, BotDirs, SANDBOX_PROVIDER_HOME};

/// Feature flags forced off for every bot session. Native sub-agents
/// (`multi_agent*`) are disabled by policy, not only by prompt. Codex
/// 0.166 keeps `unified_exec` (the execution backend) on regardless of
/// configuration; with `shell_tool` off and a read-only, never-approve
/// sandbox it exposes no command tool, which the adapter verifies from live
/// turn events and rejects if seen.
pub const DISABLED_FEATURES: &[&str] = &[
    "multi_agent",
    "multi_agent_v2",
    "shell_tool",
    "shell_snapshot",
    "apps",
    "plugins",
    "remote_plugin",
    "plugin_sharing",
    "hooks",
    "browser_use",
    "browser_use_external",
    "in_app_browser",
    "in_app_local_automation",
    "computer_use",
    "image_generation",
    "goals",
    "memories",
    "sleep_tool",
    "view_image",
    "tool_suggest",
    "skill_search",
    "skill_mcp_dependency_install",
    "workspace_dependencies",
    "code_mode_host",
];

pub fn config_toml() -> String {
    let mut text = String::from(
        "# Written by else.bot before every launch; edits are overwritten.\n\
         approval_policy = \"never\"\n\
         sandbox_mode = \"read-only\"\n\
         web_search = \"disabled\"\n\
         project_doc_max_bytes = 0\n\
         check_for_update_on_startup = false\n\
         \n\
         [agents]\n\
         enabled = false\n\
         \n\
         [features]\n",
    );
    for feature in DISABLED_FEATURES {
        text.push_str(&format!("{feature} = false\n"));
    }
    text.push_str("\n[mcp_servers]\n");
    text
}

/// Where the host's Codex login lives: `$CODEX_HOME/auth.json`, else
/// `~/.codex/auth.json`.
pub fn host_auth_file(codex_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let dir = match (codex_home, home) {
        (Some(dir), _) if !dir.is_empty() => PathBuf::from(dir),
        (_, Some(home)) if !home.is_empty() => Path::new(home).join(".codex"),
        _ => return None,
    };
    Some(dir.join("auth.json"))
}

/// Prepare the bot's Codex home: rewrite the locked config and create the
/// empty mount point for the login. Returns the bind for the login file.
pub fn prepare(dirs: &BotDirs, host_auth: &Path) -> std::io::Result<Bind> {
    let config = dirs.provider_home.join("config.toml");
    fs::write(&config, config_toml())?;
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600))?;
    let mount_point = dirs.provider_home.join("auth.json");
    // An empty placeholder the login is mounted over; never a copy.
    if fs::symlink_metadata(&mount_point).is_err() {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&mount_point)?;
    }
    Ok(Bind {
        source: host_auth.to_owned(),
        target: format!("{SANDBOX_PROVIDER_HOME}/auth.json"),
        writable: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_disables_native_agents_and_tools() {
        let text = config_toml();
        assert!(text.contains("[agents]\nenabled = false"));
        for flag in [
            "multi_agent = false",
            "multi_agent_v2 = false",
            "shell_tool = false",
        ] {
            assert!(text.contains(flag), "{flag}");
        }
        assert!(text.contains("web_search = \"disabled\""));
    }

    #[test]
    fn auth_file_location() {
        assert_eq!(
            host_auth_file(None, Some("/home/x")).unwrap(),
            PathBuf::from("/home/x/.codex/auth.json")
        );
        assert_eq!(
            host_auth_file(Some("/srv/codex"), Some("/home/x")).unwrap(),
            PathBuf::from("/srv/codex/auth.json")
        );
    }
}
