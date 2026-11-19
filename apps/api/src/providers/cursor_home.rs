//! Cursor's runtime on the host: the Node.js that runs it, the pinned
//! `@cursor/sdk` installed under the API's data directory on first use, the
//! sidecar script that wraps it, and the API key kept in the Vault.
//!
//! The SDK's license does not let else.bot ship it, so the server installs it
//! with npm once it has a key, instead of the release bundling it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::Vault;
use super::host::Host;

/// The SDK version the sidecar is written against.
pub const SDK_VERSION: &str = "1.3.1";
/// The oldest Node.js the SDK runs on.
const MIN_NODE: (u32, u32) = (22, 13);
/// The Vault entry and variable that hold the account's API key.
pub const VAULT_ENTRY: &str = "provider-cursor";
pub const KEY_VAR: &str = "CURSOR_API_KEY";
const SIDECAR_FILE: &str = "else-cursor.mjs";
const SIDECAR: &str = include_str!("cursor/sidecar.mjs");
const INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub fn signed_out() -> String {
    format!(
        "Cursor isn't signed in on this server. In Settings › Vault, add an entry named \
         `{VAULT_ENTRY}` with the variable {KEY_VAR} set to an API key from \
         cursor.com/dashboard, or switch this bot to another model."
    )
}

/// Node.js that can run the SDK, or why there is none.
pub fn node(host: &Host, found: Result<Option<PathBuf>, String>) -> Result<PathBuf, String> {
    let node = found?.ok_or(
        "Cursor needs Node.js 22.13 or newer on this server, and `node` was not found on PATH. \
         Install Node.js, or set ELSE_NODE_BIN to it.",
    )?;
    let printed = host.probe(&node, &["--version"], &[]).map_err(|why| {
        format!(
            "Node.js does not run (`{} --version`: {why}).",
            node.display()
        )
    })?;
    match parse_version(&printed) {
        Some(version) if version >= MIN_NODE => Ok(node),
        _ => Err(format!(
            "Cursor needs Node.js 22.13 or newer on this server; {} is {printed}. Install a newer \
             Node.js, or set ELSE_NODE_BIN to one.",
            node.display()
        )),
    }
}

/// `v22.13.0` as `(22, 13)`.
fn parse_version(printed: &str) -> Option<(u32, u32)> {
    let mut parts = printed.trim().strip_prefix('v')?.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// The account's API key from the Vault, or the sign-in step.
pub async fn api_key(vault: Option<&Vault>) -> Result<zeroize::Zeroizing<String>, String> {
    let vault = vault.ok_or_else(signed_out)?;
    let vars =
        match crate::db::task_secrets::provider_env(&vault.pool, &vault.key, VAULT_ENTRY).await {
            Ok(vars) => vars,
            Err(crate::db::task_secrets::SecretError::Unknown(_)) => return Err(signed_out()),
            Err(err) => return Err(format!("Cursor's API key could not be read: {err}")),
        };
    vars.into_iter()
        .find(|(name, value)| name == KEY_VAR && !value.trim().is_empty())
        .map(|(_, value)| value)
        .ok_or_else(signed_out)
}

/// The bot's Cursor agents, kept in its private provider home instead of
/// the SDK's default under `~/.cursor`.
pub fn store_dir(home: &Path) -> std::io::Result<PathBuf> {
    let store = home.join("agents");
    std::fs::create_dir_all(&store)?;
    Ok(store)
}

/// The sidecar's environment: the SDK's runtime data in the bot's home.
pub fn env(home: &Path) -> Vec<(String, String)> {
    vec![(
        "CURSOR_DATA_DIR".into(),
        home.join("data").to_string_lossy().into_owned(),
    )]
}

/// Where this SDK version and the sidecar beside it live.
pub fn sdk_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("cursor-sdk").join(SDK_VERSION)
}

fn installed(dir: &Path) -> bool {
    let manifest = dir.join("node_modules/@cursor/sdk/package.json");
    std::fs::read(manifest)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|manifest| manifest["version"] == SDK_VERSION)
}

/// The sidecar script, with the pinned SDK installed beside it. A missing
/// SDK is installed with the npm next to `node` (else on `PATH`) into a
/// new directory renamed into place, so a failed or concurrent install
/// never leaves a partial one. Callers serialize installs.
pub async fn prepare(host: &Host, node: &Path, data_dir: &Path) -> Result<PathBuf, String> {
    let dir = sdk_dir(data_dir);
    if !installed(&dir) {
        install(host, node, &dir).await?;
    }
    let script = dir.join(SIDECAR_FILE);
    if std::fs::read(&script).ok().as_deref() != Some(SIDECAR.as_bytes()) {
        super::cli::files::replace_private(&dir, SIDECAR_FILE, SIDECAR.as_bytes())
            .map_err(|e| format!("The Cursor sidecar could not be written: {e}"))?;
    }
    Ok(script)
}

async fn install(host: &Host, node: &Path, dir: &Path) -> Result<(), String> {
    let manual = format!(
        "Run `npm install --prefix {} @cursor/sdk@{SDK_VERSION}` on the server, or switch this bot \
         to another model.",
        dir.display()
    );
    let failed = |why: String| format!("The Cursor SDK could not be installed: {why}. {manual}");
    let npm = node
        .parent()
        .map(|bin| bin.join("npm"))
        .filter(|npm| npm.is_file())
        .map_or_else(|| super::executable(None, "npm"), |npm| Ok(Some(npm)))
        .map_err(&failed)?
        .ok_or_else(|| failed("npm was not found".into()))?;
    let parent = dir
        .parent()
        .ok_or_else(|| failed("no parent directory".into()))?;
    std::fs::create_dir_all(parent).map_err(|e| failed(e.to_string()))?;
    let staging = parent.join(format!(".{SDK_VERSION}.{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&staging).map_err(|e| failed(e.to_string()))?;
    tracing::info!(dir = %dir.display(), "installing the Cursor SDK");
    let prefix = staging.to_string_lossy().into_owned();
    let package = format!("@cursor/sdk@{SDK_VERSION}");
    // npm finds `node` through PATH; put the one Cursor runs first.
    let path = match (node.parent(), std::env::var("PATH")) {
        (Some(bin), Ok(path)) => format!("{}:{path}", bin.display()),
        (Some(bin), Err(_)) => bin.display().to_string(),
        (None, path) => path.unwrap_or_default(),
    };
    let args = [
        "install",
        "--prefix",
        &prefix,
        "--no-audit",
        "--no-fund",
        "--no-save",
        "--omit=dev",
        "--loglevel=error",
        &package,
    ];
    let result = tokio::time::timeout(
        INSTALL_TIMEOUT,
        host.output(&npm, &args, &[("PATH".into(), path)], 4 * 1024 * 1024),
    )
    .await
    .unwrap_or_else(|_| Err("npm did not finish within 10 minutes".into()));
    let renamed = result.and_then(|_| {
        if !installed(&staging) {
            return Err("npm finished without installing it".into());
        }
        // Another install may have finished first; keep that one.
        match std::fs::rename(&staging, dir) {
            Ok(()) => Ok(()),
            Err(_) if installed(dir) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    });
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    renamed.map_err(failed)
}
