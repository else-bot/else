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
//! native sub-agents. A turn started by an incoming connector event, and a
//! memory summary, get no host tools: shell off, a read-only sandbox, and a
//! model catalog without native tools. The event keeps else.bot's MCP
//! tools; the summary gets none.
//!
//! Codex registers `apply_patch` from the model's catalog entry whatever the
//! feature flags say, so the shell flag alone does not remove file edits.
//! Those launches export the installed CLI's own catalog (`codex debug
//! models`, under the launch's environment), keep only the selected model
//! with its native tools removed, and start Codex on that file through the
//! supported `model_catalog_json` override. A static catalog is never
//! refreshed over.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::cli::files::{GRANT_ENV, link_login, replace_private};
use super::host::{BotDirs, Host, LaunchSpec};

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

/// Input tokens at which Codex compacts a thread. Its default sits near the
/// 475k window, so long bot threads reread 200k to 450k tokens on every
/// model request; replaying production rollouts at this limit cut input by
/// about a fifth with one compaction per 14 threads. else.bot resends its
/// memory, skills, and context blocks after a compaction.
pub const AUTO_COMPACT_TOKENS: u32 = 200_000;

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
         model_auto_compact_token_limit = {AUTO_COMPACT_TOKENS}\n\
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
    // Codex reaches its shell, file edits, and MCP tools only through its
    // code-mode host; a summary has none of them to call.
    let code_mode = host_tools || tool_url.is_some();
    text.push_str(&format!("code_mode_host = {code_mode}\n"));
    if !host_tools {
        text.push_str("\n[tools]\nupdate_plan = { enabled = false }\nexperimental_request_user_input = { enabled = false }\n");
    }
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

const CATALOG_FILE: &str = "no-native-tools-catalog.json";
const EXPORT_TIMEOUT: Duration = Duration::from_secs(30);
const EXPORT_LIMIT: usize = 8 << 20;

/// Reduce the installed catalog to one model without native host tools.
/// Missing models or required capability fields fail closed.
pub fn no_native_catalog(exported: &[u8], model: &str) -> Result<Vec<u8>, String> {
    let catalog: Value = serde_json::from_slice(exported)
        .map_err(|e| format!("the Codex model catalog is not JSON: {e}"))?;
    let models = catalog["models"]
        .as_array()
        .ok_or("the Codex model catalog lists no models")?;
    let [entry] = models
        .iter()
        .filter(|m| m["slug"] == model)
        .collect::<Vec<_>>()[..]
    else {
        return Err(format!(
            "the Codex model catalog does not list {model} once"
        ));
    };
    let mut entry = entry.clone();
    let fields = entry
        .as_object_mut()
        .ok_or("the Codex model catalog entry is not an object")?;
    for (field, without) in [
        ("apply_patch_tool_type", Value::Null),
        ("shell_type", json!("disabled")),
        ("experimental_supported_tools", json!([])),
    ] {
        let Some(value) = fields.get_mut(field) else {
            return Err(format!(
                "the Codex catalog entry for {model} has no {field}"
            ));
        };
        *value = without;
    }
    // Restricted launches cannot search deferred MCP schemas, so declare them up front.
    fields.insert("supports_search_tool".into(), json!(false));
    Ok(serde_json::to_vec(&json!({ "models": [entry] })).expect("JSON values serialize"))
}

/// Export the installed CLI's catalog under `launch`'s environment, write
/// `model`'s entry without native tools into the provider home, and return
/// the arguments that start Codex on it. The home's `config.toml` must
/// already be prepared and must not name a catalog itself.
pub async fn no_native_tools(
    host: &Host,
    launch: &LaunchSpec,
    model: &str,
) -> Result<Vec<OsString>, String> {
    let export = LaunchSpec {
        args: vec!["debug".into(), "models".into()],
        ..launch.clone()
    };
    let exported = host
        .bounded_output(&export, EXPORT_TIMEOUT, EXPORT_LIMIT)
        .await
        .map_err(|e| format!("`codex debug models` failed: {e}"))?;
    let catalog = no_native_catalog(&exported, model)?;
    let home = &launch.dirs.provider_home;
    replace_private(home, CATALOG_FILE, &catalog).map_err(|e| e.to_string())?;
    let path = home.join(CATALOG_FILE);
    let path = path.to_str().ok_or("the provider home path is not UTF-8")?;
    let path = serde_json::to_string(path).expect("strings serialize");
    let arguments = vec!["-c".into(), format!("model_catalog_json={path}").into()];
    let mut verify = launch.clone();
    verify.args = arguments.clone();
    verify.args.extend(["debug".into(), "models".into()]);
    let loaded = host
        .bounded_output(&verify, EXPORT_TIMEOUT, EXPORT_LIMIT)
        .await?;
    let loaded: Value = serde_json::from_slice(&loaded)
        .map_err(|_| "Codex did not return the loaded model catalog")?;
    let expected: Value = serde_json::from_slice(&catalog).expect("generated catalog is JSON");
    if loaded != expected {
        return Err("Codex did not load the catalog without native tools".into());
    }
    Ok(arguments)
}

/// Prepare the bot's Codex home: rewrite the config and point `auth.json`
/// at the host login (anything else at that path is replaced).
pub fn prepare(
    dirs: &BotDirs,
    host_auth: &Path,
    tool_url: Option<&str>,
    host_tools: bool,
) -> std::io::Result<()> {
    replace_private(
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
    fn code_mode_runs_whenever_the_launch_has_a_tool_to_call() {
        let url = Some("http://127.0.0.1:7878/tools/v1/mcp");
        for (tool_url, host_tools, code_mode) in [
            (None, true, true),
            (url, true, true),
            (url, false, true),
            (None, false, false),
        ] {
            let text = config_toml(tool_url, host_tools);
            assert!(
                text.contains(&format!("code_mode_host = {code_mode}\n")),
                "{tool_url:?} {host_tools}"
            );
            assert!(text.contains("[agents]\nenabled = false"));
            assert!(text.contains("multi_agent = false\nmulti_agent_v2 = false\n"));
        }
    }

    #[test]
    fn elsebot_tools_without_host_tools_keep_code_mode_and_drop_the_shell() {
        let text = config_toml(Some("http://127.0.0.1:7878/tools/v1/mcp"), false);
        assert!(text.contains("shell_tool = false\n"));
        assert!(text.contains("sandbox_mode = \"read-only\""));
        assert!(text.contains("approval_policy = \"never\""));
        assert!(text.contains("code_mode_host = true\n"));
        assert!(text.contains("[mcp_servers.elsebot]"));
        assert!(text.contains("web_search = \"disabled\""));
        for feature in DISABLED_FEATURES {
            assert!(text.contains(&format!("{feature} = false\n")), "{feature}");
        }
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
        fs::write(home.join("auth.json"), "REFRESHED").unwrap();
        assert_eq!(fs::read_to_string(&login).unwrap(), "REFRESHED");
    }

    #[test]
    fn the_no_native_catalog_keeps_only_the_model_without_its_native_tools() {
        let entry = |slug: &str| {
            json!({
                "slug": slug,
                "context_window": 272000,
                "apply_patch_tool_type": "freeform",
                "shell_type": "unified_exec",
                "experimental_supported_tools": ["clock"],
            })
        };
        let exported = json!({ "models": [entry("a"), entry("b")] }).to_string();
        let catalog: Value =
            serde_json::from_slice(&no_native_catalog(exported.as_bytes(), "b").unwrap()).unwrap();
        assert_eq!(
            catalog,
            json!({ "models": [{
                "slug": "b",
                "context_window": 272000,
                "apply_patch_tool_type": null,
                "shell_type": "disabled",
                "experimental_supported_tools": [],
                "supports_search_tool":false,
            }] })
        );
        let mut missing = entry("b");
        missing
            .as_object_mut()
            .unwrap()
            .remove("apply_patch_tool_type");
        for bad in [
            "not json".to_owned(),
            json!({ "data": [entry("b")] }).to_string(),
            json!({ "models": [entry("a")] }).to_string(),
            json!({ "models": [entry("b"), entry("b")] }).to_string(),
            json!({ "models": ["b"] }).to_string(),
            json!({ "models": [missing] }).to_string(),
        ] {
            assert!(no_native_catalog(bad.as_bytes(), "b").is_err(), "{bad}");
        }
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
