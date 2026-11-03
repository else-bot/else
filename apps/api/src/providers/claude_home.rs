//! Claude Code's configuration for bots.
//!
//! Bots run Claude Code with the host's own configuration directory, the way
//! `claude` runs in the owner's terminal, so the owner's subscription login
//! works wherever Claude Code keeps it (`.credentials.json` on Linux, the
//! Keychain on macOS, whose entry is keyed to the configuration directory).
//! Native sessions live there too, under `projects/` in a directory named
//! for the bot's project directory, so each bot resumes only its own; the
//! adapter always names the session to resume. Settings and the MCP
//! configuration are rewritten before every launch, and the CLI is started
//! with `--setting-sources ""` and `--strict-mcp-config`, so user, project,
//! and local settings, hooks, plugins' MCP servers, and anything a session
//! wrote are ignored.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::host::BotDirs;

/// Environment variable carrying the turn's task grant; the MCP
/// configuration references it, so the grant is never written to disk.
pub const GRANT_ENV: &str = "ELSE_TASK_GRANT";
pub const SETTINGS_FILE: &str = "else-settings.json";
pub const MCP_FILE: &str = "elsebot-mcp.json";

/// The host's Claude Code configuration directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    pub dir: PathBuf,
    /// Given as `CLAUDE_CONFIG_DIR` to the API, so passed on to the CLI;
    /// otherwise the CLI's default (`~/.claude`) is left implicit, as the
    /// Keychain entry depends on it.
    pub explicit: bool,
}

impl HostConfig {
    /// The environment the CLI needs to use this directory.
    pub fn env(&self) -> Vec<(String, String)> {
        if self.explicit {
            vec![(
                "CLAUDE_CONFIG_DIR".to_owned(),
                self.dir.to_string_lossy().into_owned(),
            )]
        } else {
            Vec::new()
        }
    }
}

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn host_config(config_dir: Option<&str>, home: Option<&str>) -> Option<HostConfig> {
    match (config_dir, home) {
        (Some(dir), _) if !dir.is_empty() => Some(HostConfig {
            dir: PathBuf::from(dir),
            explicit: true,
        }),
        (_, Some(home)) if !home.is_empty() => Some(HostConfig {
            dir: Path::new(home).join(".claude"),
            explicit: false,
        }),
        _ => None,
    }
}

/// Whether `claude auth status` (JSON) reports a login.
pub fn logged_in(status: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(status)
        .is_ok_and(|v| v["loggedIn"].as_bool() == Some(true))
}

/// A stable, non-secret identity of the host login (its account and
/// organization IDs), used only inside session fingerprints.
pub fn host_account(config_dir: Option<&str>, home: Option<&str>) -> String {
    let path = match (config_dir, home) {
        (Some(dir), _) if !dir.is_empty() => Path::new(dir).join(".claude.json"),
        (_, Some(home)) if !home.is_empty() => Path::new(home).join(".claude.json"),
        _ => return String::new(),
    };
    let Ok(text) = fs::read_to_string(path) else {
        return String::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return String::new();
    };
    let account = &value["oauthAccount"];
    format!(
        "{}:{}",
        account["accountUuid"].as_str().unwrap_or_default(),
        account["organizationUuid"].as_str().unwrap_or_default()
    )
}

/// Settings forced for every session: no hooks, no auto-memory, no
/// co-author trailers, and native agents, web, and skills denied even if the
/// tool set were widened; the host tools too unless `host_tools`.
pub fn settings_json(host_tools: bool) -> String {
    let mut deny = DENIED_TOOLS.to_vec();
    if !host_tools {
        deny.extend(HOST_TOOLS);
    }
    json!({
        "disableAllHooks": true,
        "autoMemoryEnabled": false,
        "includeCoAuthoredBy": false,
        "enableAllProjectMcpServers": false,
        "permissions": {
            "defaultMode": "dontAsk",
            "deny": deny,
        },
    })
    .to_string()
}

/// Claude Code's built-in shell and file tools, offered to a turn so the bot
/// can work on the host (the names `system/init` reports for them).
pub const HOST_TOOLS: &[&str] = &[
    "Bash",
    "Read",
    "Write",
    "Edit",
    "NotebookEdit",
    "Glob",
    "Grep",
];

/// Built-in tools that must never run, denied in settings as well as left
/// out of the tool set.
pub const DENIED_TOOLS: &[&str] = &[
    "Agent",
    "Task",
    "TaskStop",
    "BashOutput",
    "KillShell",
    "MultiEdit",
    "WebFetch",
    "WebSearch",
    "Skill",
    "SlashCommand",
];

/// else.bot's MCP server, with the grant taken from the environment.
pub fn mcp_json(tool_url: &str) -> String {
    json!({
        "mcpServers": {
            crate::http::mcp::SERVER_NAME: {
                "type": "http",
                "url": tool_url,
                "headers": { "Authorization": format!("Bearer ${{{GRANT_ENV}}}") },
            }
        }
    })
    .to_string()
}

/// The tool names Claude reports for else.bot's tools.
pub fn tool_names() -> Vec<String> {
    crate::http::mcp::TOOL_NAMES
        .iter()
        .map(|t| format!("mcp__{}__{t}", crate::http::mcp::SERVER_NAME))
        .collect()
}

/// Replace `dir/name` atomically without following anything the sandbox
/// may have planted there.
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

/// Prepare the bot's private Claude files (passed to the CLI by path):
/// rewrite the locked settings and MCP configuration (removed when there
/// are no tools).
pub fn prepare(dirs: &BotDirs, tool_url: Option<&str>, host_tools: bool) -> std::io::Result<()> {
    replace_file(
        &dirs.provider_home,
        SETTINGS_FILE,
        settings_json(host_tools).as_bytes(),
    )?;
    match tool_url {
        Some(url) => replace_file(&dirs.provider_home, MCP_FILE, mcp_json(url).as_bytes())?,
        None => match fs::remove_file(dirs.provider_home.join(MCP_FILE)) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err),
            _ => {}
        },
    }
    Ok(())
}

/// Whether the host configuration holds the native session's transcript.
/// Sessions live under `projects/` in a directory named for the CLI's
/// working directory, the bot's project by its resolved path, every
/// character but ASCII letters and digits replaced by `-` (`/a/b.c` gives
/// `-a-b-c`).
pub fn session_exists(config_dir: &Path, project: &Path, native_id: &str) -> bool {
    let project = fs::canonicalize(project).unwrap_or_else(|_| project.to_owned());
    let project: String = project
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    uuid::Uuid::parse_str(native_id).is_ok()
        && fs::symlink_metadata(
            config_dir
                .join("projects")
                .join(project)
                .join(format!("{native_id}.jsonl")),
        )
        .is_ok_and(|m| m.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_config_references_the_grant_without_containing_it() {
        let text = mcp_json("http://127.0.0.1:7878/tools/v1/mcp");
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let server = &value["mcpServers"]["elsebot"];
        assert_eq!(server["type"], "http");
        assert_eq!(
            server["headers"]["Authorization"],
            "Bearer ${ELSE_TASK_GRANT}"
        );
        assert!(!text.contains("tg_"));
    }

    #[test]
    fn settings_deny_native_agents_and_host_tools_unless_offered() {
        let value: serde_json::Value = serde_json::from_str(&settings_json(true)).unwrap();
        let deny: Vec<&str> = value["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        for tool in ["Agent", "Task", "WebFetch"] {
            assert!(deny.contains(&tool), "{tool}");
        }
        for tool in HOST_TOOLS {
            assert!(!deny.contains(tool), "{tool}");
        }
        let value: serde_json::Value = serde_json::from_str(&settings_json(false)).unwrap();
        let deny: Vec<&str> = value["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        for tool in ["Agent", "Task", "Bash", "Read", "WebFetch"] {
            assert!(deny.contains(&tool), "{tool}");
        }
        assert_eq!(value["disableAllHooks"], true);
    }

    #[test]
    fn prepare_never_writes_through_planted_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("claude-home");
        fs::create_dir(&home).unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "HOST FILE").unwrap();
        for name in [SETTINGS_FILE, MCP_FILE] {
            std::os::unix::fs::symlink(&outside, home.join(name)).unwrap();
        }
        let dirs = BotDirs {
            root: dir.path().to_owned(),
            project: dir.path().join("project"),
            provider_home: home.clone(),
        };
        prepare(&dirs, None, true).unwrap();
        assert_eq!(fs::read_to_string(&outside).unwrap(), "HOST FILE");
        assert!(
            fs::symlink_metadata(home.join(MCP_FILE)).is_err(),
            "no tools, no MCP file"
        );
        assert_eq!(
            fs::read_to_string(home.join(SETTINGS_FILE)).unwrap(),
            settings_json(true)
        );
    }

    #[test]
    fn sessions_are_found_by_the_bots_resolved_project_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = root.join("claude");
        let project = root.join("bots").join("b0t.1");
        fs::create_dir_all(&project).unwrap();
        let linked = root.join("linked");
        std::os::unix::fs::symlink(root.join("bots"), &linked).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        assert!(!session_exists(&config, &project, &id));
        let encoded: String = project
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        assert!(encoded.ends_with("-bots-b0t-1"));
        let sessions = config.join("projects").join(&encoded);
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join(format!("{id}.jsonl")), "{}").unwrap();
        assert!(session_exists(&config, &project, &id));
        // The CLI names the directory for the resolved path.
        assert!(session_exists(&config, &linked.join("b0t.1"), &id));
        assert!(!session_exists(&config, &root.join("bots/other"), &id));
        assert!(!session_exists(&config, &project, "../../etc/passwd"));
    }

    #[test]
    fn the_host_configuration_is_explicit_only_when_given() {
        let implicit = host_config(None, Some("/home/x")).unwrap();
        assert_eq!(implicit.dir, PathBuf::from("/home/x/.claude"));
        assert!(
            implicit.env().is_empty(),
            "the Keychain entry depends on it"
        );
        let explicit = host_config(Some("/srv/claude"), Some("/home/x")).unwrap();
        assert_eq!(
            explicit.env(),
            vec![("CLAUDE_CONFIG_DIR".to_owned(), "/srv/claude".to_owned())]
        );
        assert!(logged_in(
            r#"{"loggedIn": true, "authMethod": "claude.ai"}"#
        ));
        assert!(!logged_in(r#"{"loggedIn": false}"#));
        assert!(!logged_in("not json"));
    }
}
