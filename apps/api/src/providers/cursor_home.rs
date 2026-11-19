//! Cursor CLI's files for bots.
//!
//! The CLI reads rules, commands, skills, hooks, plugins and MCP servers
//! from `~/.cursor`, and permissions and hooks from `~/.claude`, through the
//! home directory with no override. Each bot therefore runs it with its
//! provider home as `HOME`, so none of the owner's personal configuration
//! loads. The only thing shared is the host login: on Linux, and on macOS
//! with the file credential store, the bot's `auth.json` is a link to the
//! host's, so token refreshes write through; the macOS keychain is per user
//! and needs no link. Chats live in the bot's own `.cursor/chats`.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::cli::files::{GRANT_ENV, link_login, replace_private};
use super::host::BotDirs;
use crate::app::tools::SERVER_NAME;

pub const SIGNED_OUT: &str = "Cursor CLI isn't signed in on this server. Run `cursor-agent login` \
                              there, or switch this bot to another model.";
pub const NOT_INSTALLED: &str = "Cursor CLI (`cursor-agent`) was not found on this server. \
                                 Install it with `curl https://cursor.com/install -fsS | bash`, \
                                 or set ELSE_CURSOR_BIN.";
/// Where Cursor's installer links the CLI, under the home directory.
pub const INSTALLED: &str = ".local/bin/cursor-agent";
/// The oldest CLI whose flags and stream this adapter was verified against.
/// Its versions start with their release date.
pub const MIN_VERSION: &str = "2026.08.11";

/// The host's login file, when the CLI keeps one: `$XDG_CONFIG_HOME/cursor`
/// or `~/.config/cursor` on Linux, `~/.cursor` on macOS when
/// `AGENT_CLI_CREDENTIAL_STORE=file`. `None` on macOS by default, where the
/// login is in the keychain.
pub fn host_login(
    macos: bool,
    home: Option<&str>,
    xdg_config: Option<&str>,
    store: Option<&str>,
) -> Option<PathBuf> {
    let home = home.filter(|h| !h.is_empty()).map(Path::new);
    if macos {
        return (store == Some("file"))
            .then(|| home.map(|h| h.join(".cursor/auth.json")))
            .flatten();
    }
    match xdg_config.filter(|x| !x.is_empty()) {
        Some(config) => Some(Path::new(config).join("cursor/auth.json")),
        None => home.map(|h| h.join(".config/cursor/auth.json")),
    }
}

/// How the host's CLI finds its login, kept for the bots' runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    pub macos: bool,
    /// `AGENT_CLI_CREDENTIAL_STORE`, passed on unchanged.
    pub store: Option<String>,
    /// The host's login file, linked into each bot's home.
    pub file: Option<PathBuf>,
}

impl Login {
    pub fn host() -> Self {
        let var = |name| std::env::var(name).ok();
        let store = var("AGENT_CLI_CREDENTIAL_STORE");
        let macos = cfg!(target_os = "macos");
        Self {
            macos,
            file: host_login(
                macos,
                var("HOME").as_deref(),
                var("XDG_CONFIG_HOME").as_deref(),
                store.as_deref(),
            ),
            store,
        }
    }
}

/// Where the CLI looks for its login under a bot's `HOME`.
fn bot_login(home: &Path, macos: bool) -> PathBuf {
    if macos {
        home.join(".cursor/auth.json")
    } else {
        home.join(".config/cursor/auth.json")
    }
}

pub fn signed_out(login: Option<&Path>) -> bool {
    login.is_some_and(|login| !login.is_file())
}

/// A stable, non-secret identity of the host login (the user its access
/// token names), used only inside session fingerprints.
pub fn host_account(login: &Path) -> String {
    use base64::Engine;
    let Ok(text) = fs::read_to_string(login) else {
        return String::new();
    };
    let Ok(auth) = serde_json::from_str::<serde_json::Value>(&text) else {
        return String::new();
    };
    auth["accessToken"]
        .as_str()
        .and_then(|token| token.split('.').nth(1))
        .and_then(|claims| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(claims.trim_end_matches('='))
                .ok()
        })
        .and_then(|claims| serde_json::from_slice::<serde_json::Value>(&claims).ok())
        .and_then(|claims| claims["sub"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Whether the CLI refused for want of a login, as its stderr reports it.
pub fn refused_login(stderr: &str) -> bool {
    stderr.contains("Authentication required")
        || stderr.contains("Not authenticated")
        || stderr.contains("agent login")
}

/// Whether `version` (`2026.08.11-e8db854`) is at least [`MIN_VERSION`].
pub fn supported(version: &str) -> bool {
    let date = version.trim().get(..MIN_VERSION.len()).unwrap_or_default();
    date.len() == MIN_VERSION.len()
        && date.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && date >= MIN_VERSION
}

/// The CLI's environment for a bot: its provider home as `HOME` and every
/// Cursor directory inside it.
pub fn env(home: &Path, login: &Login) -> Vec<(String, String)> {
    let path = |p: &Path| p.to_string_lossy().into_owned();
    let mut env = vec![
        ("HOME".into(), path(home)),
        ("XDG_CONFIG_HOME".into(), path(&home.join(".config"))),
        ("XDG_DATA_HOME".into(), path(&home.join(".local/share"))),
        ("XDG_CACHE_HOME".into(), path(&home.join(".cache"))),
        ("XDG_STATE_HOME".into(), path(&home.join(".local/state"))),
        ("CURSOR_CONFIG_DIR".into(), path(&home.join(".cursor"))),
        ("CURSOR_DATA_DIR".into(), path(&home.join(".cursor"))),
        ("NO_COLOR".into(), "1".into()),
        ("NO_OPEN_BROWSER".into(), "1".into()),
        ("DIRENV_DISABLE".into(), "1".into()),
    ];
    if let Some(store) = &login.store {
        env.push(("AGENT_CLI_CREDENTIAL_STORE".into(), store.clone()));
    }
    env
}

/// Owner turns run tools without asking (`unrestricted`, which unlike
/// `--force` leaves web search and fetch unapproved); other turns run only
/// else.bot's. No sandbox, as with the other providers' host tools.
fn cli_config(host_tools: bool) -> serde_json::Value {
    let (deny, mode): (&[&str], _) = if host_tools {
        (&[], "unrestricted")
    } else {
        (&["Shell(*)", "Read(**)", "Write(**)"], "allowlist")
    };
    json!({
        "version": 1,
        "permissions": {
            "allow": [format!("Mcp({SERVER_NAME}:*)")],
            "deny": deny,
        },
        "approvalMode": mode,
        "sandbox": { "mode": "disabled" },
        "autoAcceptWebSearch": false,
        "attribution": { "attributeCommitsToAgent": false, "attributePRsToAgent": false },
    })
}

/// Write the bot's Cursor configuration before a launch: the login link,
/// the CLI settings, and else.bot's MCP server as the only user-level one.
/// The grant stays in the environment; the server's header names it.
pub fn prepare(
    dirs: &BotDirs,
    login: &Login,
    tool_url: Option<&str>,
    host_tools: bool,
) -> std::io::Result<()> {
    let home = &dirs.provider_home;
    let cursor = home.join(".cursor");
    fs::create_dir_all(&cursor)?;
    if let Some(file) = &login.file {
        let linked = bot_login(home, login.macos);
        let dir = linked.parent().expect("the login has a directory");
        fs::create_dir_all(dir)?;
        link_login(dir, "auth.json", file)?;
    }
    let config = serde_json::to_vec_pretty(&cli_config(host_tools))?;
    replace_private(&cursor, "cli-config.json", &config)?;
    let servers = match tool_url {
        Some(url) => json!({
            SERVER_NAME: {
                "url": url,
                "headers": { "Authorization": format!("Bearer ${{{GRANT_ENV}}}") },
            },
        }),
        None => json!({}),
    };
    let mcp = serde_json::to_vec_pretty(&json!({ "mcpServers": servers }))?;
    replace_private(&cursor, "mcp.json", &mcp)
}

/// Where a turn without host tools runs: a directory only else.bot writes,
/// so no project hooks, plugins, rules or MCP servers load. The bot's
/// project is its computer's `~/work`, which a guest can write.
pub fn private_workspace(home: &Path) -> std::io::Result<PathBuf> {
    let dir = home.join("workspace");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Whether the bot's chat `id` exists for this project directory: Cursor
/// keeps it in `chats/<md5 of the directory>/<id>/store.db`.
pub fn session_exists(home: &Path, project: &Path, id: &str) -> bool {
    if uuid::Uuid::parse_str(id).is_err() {
        return false;
    }
    let Ok(project) = project.canonicalize() else {
        return false;
    };
    let digest = md5_hex(project.to_string_lossy().as_bytes());
    home.join(".cursor/chats")
        .join(digest)
        .join(id)
        .join("store.db")
        .is_file()
}

fn md5_hex(bytes: &[u8]) -> String {
    use md5::Digest;
    hex::encode(md5::Md5::digest(bytes))
}
