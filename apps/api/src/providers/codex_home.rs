//! Per-bot Codex home and its configuration.
//!
//! Each bot gets its own `CODEX_HOME` (sessions, caches, and settings stay
//! separate). The owner's supported ChatGPT login is shared by a symlink to
//! the host `auth.json`: it is never copied, and token refreshes write
//! through to the one file. The configuration is rewritten on every launch so
//! nothing a session does can change it. A turn gets Codex's shell and file
//! tools with full host access (`danger-full-access`, never asking for
//! approval), else.bot's MCP tools, and nothing else: no web search,
//! browser, apps, plugins, hooks, other MCP servers, image generation, or
//! native sub-agents. A memory summary gets no tools at all and a read-only
//! sandbox.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::host::{BotDirs, link_login};

/// Feature flags forced off for every bot session. Native sub-agents
/// (`multi_agent*`) are disabled by policy, not only by prompt. `shell_tool`
/// is set separately: on for turns, off for summaries. Codex 0.166 keeps
/// `unified_exec` (the execution backend) on regardless of configuration;
/// with `shell_tool` off and a read-only, never-approve sandbox it exposes
/// no command tool, which the summary adapter verifies from live events.
pub const DISABLED_FEATURES: &[&str] = &[
    "multi_agent",
    "multi_agent_v2",
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
];

/// Environment variable carrying the turn's task grant to Codex, which
/// sends it as the bearer token to else.bot's MCP endpoint.
pub const GRANT_ENV: &str = "ELSE_TASK_GRANT";

/// The sandbox mode for a session: full host access with host tools,
/// otherwise read-only.
pub fn sandbox_mode(host_tools: bool) -> &'static str {
    if host_tools {
        "danger-full-access"
    } else {
        "read-only"
    }
}

/// The configuration for one launch. `host_tools` gives a turn Codex's own
/// shell and file tools on the host; a summary runs without.
pub fn config_toml(tool_url: Option<&str>, host_tools: bool) -> String {
    let mut text = format!(
        "# Written by else.bot before every launch; edits are overwritten.\n\
         approval_policy = \"never\"\n\
         sandbox_mode = \"{sandbox}\"\n\
         web_search = \"disabled\"\n\
         project_doc_max_bytes = 0\n\
         check_for_update_on_startup = false\n\
         \n\
         [agents]\n\
         enabled = false\n\
         \n\
         [features]\n\
         shell_tool = {host_tools}\n",
        sandbox = sandbox_mode(host_tools),
    );
    for feature in DISABLED_FEATURES {
        text.push_str(&format!("{feature} = false\n"));
    }
    // Codex reaches MCP tools only through its code-mode host; without
    // else.bot tools there is nothing for it to call.
    text.push_str(&format!("code_mode_host = {}\n", tool_url.is_some()));
    text.push_str("\n[mcp_servers]\n");
    if let Some(url) = tool_url {
        let tools: Vec<String> = crate::app::tools::TOOL_NAMES
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect();
        text.push_str(&format!(
            "\n[mcp_servers.{name}]\n\
             url = \"{url}\"\n\
             bearer_token_env_var = \"{GRANT_ENV}\"\n\
             enabled_tools = [{tools}]\n\
             required = true\n\
             default_tools_approval_mode = \"approve\"\n\
             startup_timeout_sec = 15\n\
             tool_timeout_sec = 360\n",
            name = crate::app::tools::SERVER_NAME,
            tools = tools.join(", "),
        ));
    }
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

/// Replace `dir/name` atomically without following anything the sandbox
/// may have planted there: the new file is created exclusively (never through
/// a symlink) and renamed over the old entry, which replaces a symlink itself
/// rather than its target.
fn replace_file(dir: &Path, name: &str, contents: &[u8]) -> std::io::Result<()> {
    if !fs::symlink_metadata(dir)?.is_dir() {
        return Err(std::io::Error::other("provider home is not a directory"));
    }
    let temp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&temp, dir.join(name))
}

/// Prepare the bot's Codex home: rewrite the config and point `auth.json`
/// at the host login (anything else at that path is replaced).
pub fn prepare(
    dirs: &BotDirs,
    host_auth: &Path,
    tool_url: Option<&str>,
    host_tools: bool,
) -> std::io::Result<()> {
    replace_file(
        &dirs.provider_home,
        "config.toml",
        config_toml(tool_url, host_tools).as_bytes(),
    )?;
    let path = dirs.provider_home.join("auth.json");
    if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
        fs::remove_dir_all(&path)?;
    }
    link_login(&dirs.provider_home, "auth.json", host_auth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_disables_native_agents_and_tools() {
        let text = config_toml(None, false);
        assert!(text.contains("[agents]\nenabled = false"));
        for flag in [
            "multi_agent = false",
            "multi_agent_v2 = false",
            "shell_tool = false",
        ] {
            assert!(text.contains(flag), "{flag}");
        }
        assert!(text.contains("sandbox_mode = \"read-only\""));
        assert!(text.contains("web_search = \"disabled\""));
    }

    #[test]
    fn host_tools_add_the_shell_with_full_access_and_nothing_else() {
        let text = config_toml(None, true);
        assert!(text.contains("shell_tool = true"));
        assert!(text.contains("sandbox_mode = \"danger-full-access\""));
        assert!(text.contains("approval_policy = \"never\""));
        for flag in ["multi_agent = false", "multi_agent_v2 = false"] {
            assert!(text.contains(flag), "{flag}");
        }
        assert!(text.contains("web_search = \"disabled\""));
    }

    #[test]
    fn config_offers_only_elsebot_tools_with_the_grant_from_the_environment() {
        assert!(!config_toml(None, true).contains("[mcp_servers."));
        let text = config_toml(Some("http://127.0.0.1:7878/tools/v1/mcp"), true);
        assert!(
            text.contains("[mcp_servers.elsebot]\nurl = \"http://127.0.0.1:7878/tools/v1/mcp\"")
        );
        assert!(text.contains("bearer_token_env_var = \"ELSE_TASK_GRANT\""));
        let expected: Vec<String> = crate::app::tools::TOOL_NAMES
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect();
        assert!(text.contains(&format!("enabled_tools = [{}]", expected.join(", "))));
        assert!(text.contains("\"memory_search\""));
        // The grant itself is never written to disk.
        assert!(!text.contains("tg_"));
    }

    #[test]
    fn prepare_never_writes_through_planted_symlinks_and_links_the_login() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex-home");
        fs::create_dir(&home).unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "HOST FILE").unwrap();
        std::os::unix::fs::symlink(&outside, home.join("config.toml")).unwrap();
        std::os::unix::fs::symlink(&outside, home.join("auth.json")).unwrap();
        let dirs = BotDirs {
            root: dir.path().to_owned(),
            project: dir.path().join("project"),
            provider_home: home.clone(),
        };
        let login = dir.path().join("host-auth.json");
        fs::write(&login, "LOGIN").unwrap();
        prepare(&dirs, &login, None, true).unwrap();
        assert_eq!(fs::read_to_string(&outside).unwrap(), "HOST FILE");
        assert!(
            fs::symlink_metadata(home.join("config.toml"))
                .unwrap()
                .is_file()
        );
        assert_eq!(
            fs::read_to_string(home.join("config.toml")).unwrap(),
            config_toml(None, true)
        );
        assert_eq!(fs::read_link(home.join("auth.json")).unwrap(), login);
        // A refresh writes through to the host login.
        fs::write(home.join("auth.json"), "REFRESHED").unwrap();
        assert_eq!(fs::read_to_string(&login).unwrap(), "REFRESHED");
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
