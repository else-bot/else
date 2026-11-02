//! Per-bot Claude Code configuration directory and locked settings.
//!
//! Each bot gets its own `CLAUDE_CONFIG_DIR` (sessions, caches, and state
//! stay separate, so one bot cannot resume another's session). The owner's
//! subscription login is shared by bind-mounting the host
//! `.credentials.json` over an empty placeholder: never copied, and token
//! refreshes write through to the one file. Settings and the MCP
//! configuration are rewritten before every launch, and the CLI is started
//! with `--setting-sources ""` and `--strict-mcp-config`, so user, project,
//! and local settings, hooks, plugins' MCP servers, and anything a session
//! wrote are ignored.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::host::{Bind, BotDirs, SANDBOX_PROVIDER_HOME};

/// Environment variable carrying the turn's task grant; the MCP
/// configuration references it, so the grant is never written to disk.
pub const GRANT_ENV: &str = "ELSE_TASK_GRANT";
pub const SETTINGS_FILE: &str = "else-settings.json";
pub const MCP_FILE: &str = "elsebot-mcp.json";

/// Where the host's Claude Code login lives: `$CLAUDE_CONFIG_DIR`, else
/// `~/.claude`.
pub fn host_credentials_file(config_dir: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let dir = match (config_dir, home) {
        (Some(dir), _) if !dir.is_empty() => PathBuf::from(dir),
        (_, Some(home)) if !home.is_empty() => Path::new(home).join(".claude"),
        _ => return None,
    };
    Some(dir.join(".credentials.json"))
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
/// co-author trailers, and native agents and host tools denied even if the
/// tool set were widened.
pub fn settings_json() -> String {
    json!({
        "disableAllHooks": true,
        "autoMemoryEnabled": false,
        "includeCoAuthoredBy": false,
        "enableAllProjectMcpServers": false,
        "permissions": {
            "defaultMode": "dontAsk",
            "deny": DENIED_TOOLS,
        },
    })
    .to_string()
}

/// Built-in tools that must never run, denied in settings as well as left
/// out of the tool set.
pub const DENIED_TOOLS: &[&str] = &[
    "Agent",
    "Task",
    "Bash",
    "BashOutput",
    "KillShell",
    "Read",
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "Glob",
    "Grep",
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

/// Prepare the bot's Claude home: rewrite the locked settings and MCP
/// configuration (removed when there are no tools) and reset the empty
/// mount point for the login. Returns the bind for the login file.
pub fn prepare(
    dirs: &BotDirs,
    host_credentials: &Path,
    tool_url: Option<&str>,
) -> std::io::Result<Bind> {
    replace_file(
        &dirs.provider_home,
        SETTINGS_FILE,
        settings_json().as_bytes(),
    )?;
    match tool_url {
        Some(url) => replace_file(&dirs.provider_home, MCP_FILE, mcp_json(url).as_bytes())?,
        None => match fs::remove_file(dirs.provider_home.join(MCP_FILE)) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err),
            _ => {}
        },
    }
    let path = dirs.provider_home.join(".credentials.json");
    let placeholder = fs::symlink_metadata(&path);
    if !placeholder.is_ok_and(|m| m.is_file() && m.len() == 0) {
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            fs::remove_dir_all(&path)?;
        }
        replace_file(&dirs.provider_home, ".credentials.json", b"")?;
    }
    Ok(Bind {
        source: host_credentials.to_owned(),
        target: format!("{SANDBOX_PROVIDER_HOME}/.credentials.json"),
        writable: true,
    })
}

/// Whether the bot's home holds the native session's transcript (the CLI
/// runs in `/work`, so sessions live under `projects/-work`).
pub fn session_exists(dirs: &BotDirs, native_id: &str) -> bool {
    uuid::Uuid::parse_str(native_id).is_ok()
        && fs::symlink_metadata(
            dirs.provider_home
                .join("projects")
                .join("-work")
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
    fn settings_deny_native_agents_and_host_tools() {
        let value: serde_json::Value = serde_json::from_str(&settings_json()).unwrap();
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
        for name in [SETTINGS_FILE, MCP_FILE, ".credentials.json"] {
            std::os::unix::fs::symlink(&outside, home.join(name)).unwrap();
        }
        let dirs = BotDirs {
            root: dir.path().to_owned(),
            work: dir.path().join("work"),
            provider_home: home.clone(),
        };
        prepare(&dirs, Path::new("/nonexistent/.credentials.json"), None).unwrap();
        assert_eq!(fs::read_to_string(&outside).unwrap(), "HOST FILE");
        assert!(
            fs::symlink_metadata(home.join(MCP_FILE)).is_err(),
            "no tools, no MCP file"
        );
        assert_eq!(
            fs::read_to_string(home.join(SETTINGS_FILE)).unwrap(),
            settings_json()
        );
        let creds = fs::symlink_metadata(home.join(".credentials.json")).unwrap();
        assert!(creds.is_file() && creds.len() == 0);
    }

    #[test]
    fn sessions_are_found_only_in_the_bots_own_home() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = BotDirs {
            root: dir.path().to_owned(),
            work: dir.path().join("work"),
            provider_home: dir.path().join("home"),
        };
        let id = uuid::Uuid::new_v4().to_string();
        assert!(!session_exists(&dirs, &id));
        fs::create_dir_all(dirs.provider_home.join("projects/-work")).unwrap();
        fs::write(
            dirs.provider_home
                .join(format!("projects/-work/{id}.jsonl")),
            "{}",
        )
        .unwrap();
        assert!(session_exists(&dirs, &id));
        assert!(!session_exists(&dirs, "../../etc/passwd"));
    }
}
