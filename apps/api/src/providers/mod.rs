pub mod attachments;
pub mod claude;
pub mod claude_home;
pub mod codex;
pub mod codex_home;
pub mod codex_rpc;
pub mod host;
pub(crate) mod output_images;
pub mod prompt;
pub mod synthetic;

use std::sync::Arc;

use crate::app::providers::{Availability, Provider};
use crate::config::Config;

/// Providers enabled by configuration, in default-selection order. Codex and
/// Claude are always listed; each reports unavailable (with a reason) when its
/// CLI or host login is missing.
pub fn registry(config: &Config) -> Registry {
    build(config).0
}

/// The registry and the no-tools summarizer for memory (the same Codex).
pub fn build(config: &Config) -> (Registry, Option<Arc<dyn crate::memory_worker::Summarizer>>) {
    let mut providers: Vec<Arc<dyn Provider>> = Vec::new();
    let host = host::Host::new(&config.data_dir, &config.bots_dir);
    let program = executable(config.codex_bin.as_deref(), "codex");
    let auth = codex_home::host_auth_file(
        std::env::var("CODEX_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    );
    let mut tool_addr = config.bind;
    if tool_addr.ip().is_unspecified() {
        tool_addr.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
    }
    let tool_url = format!("http://{tool_addr}/tools/v1/mcp");
    let codex = Arc::new(
        codex::Codex::detect(Some(host.clone()), program, auth).with_tools(tool_url.clone()),
    );
    providers.push(codex.clone());
    let config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
    let home = std::env::var("HOME").ok();
    let claude = claude::Claude::detect(
        Some(host),
        executable(config.claude_bin.as_deref(), "claude"),
        claude_home::host_config(config_dir.as_deref(), home.as_deref()),
        claude_home::host_account(config_dir.as_deref(), home.as_deref()),
    )
    .with_tools(tool_url);
    providers.push(Arc::new(claude));
    if config.synthetic_provider {
        providers.push(Arc::new(synthetic::Synthetic::new(
            std::time::Duration::from_millis(40),
        )));
    }
    let summarizer: Arc<dyn crate::memory_worker::Summarizer> = codex;
    (Registry::new(providers), Some(summarizer))
}

/// A provider CLI: the configured path, else `name` on `PATH`, resolved
/// to the native executable. Version-manager shims (such as mise's, which
/// are links to the `mise` binary) do not run with the passed environment, so a shim is
/// resolved through `mise which` (with this process's environment, so the
/// version mise selects now). A configured path that cannot be resolved is
/// kept, and the provider reports why it does not run.
fn executable(configured: Option<&std::path::Path>, name: &str) -> Option<std::path::PathBuf> {
    match configured {
        Some(path) => Some(resolve_native(path, name).unwrap_or_else(|| path.to_path_buf())),
        None => {
            let found = std::env::var_os("PATH")?
                .to_str()?
                .split(':')
                .map(|dir| std::path::Path::new(dir).join(name))
                .find(|path| path.is_file())?;
            resolve_native(&found, name)
        }
    }
}

fn resolve_native(path: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    let found = std::fs::canonicalize(path).ok()?;
    if found.file_name().is_some_and(|f| f == "mise") {
        let out = std::process::Command::new(&found)
            .args(["which", name])
            .output()
            .ok()
            .filter(|out| out.status.success())?;
        let path = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        return std::fs::canonicalize(path).ok().filter(|p| p.is_file());
    }
    Some(found)
}

/// How long an outgoing provider may take to produce a checkpoint.
pub const CHECKPOINT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);
/// Byte cap for a checkpoint's text.
pub const CHECKPOINT_BYTES: usize = 8 * 1024;

/// A checkpoint is used only whole and within its budget; never cut.
pub fn bounded_checkpoint(text: String, max: usize) -> Result<String, String> {
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Err("the checkpoint was empty".into());
    }
    if text.len() > max {
        return Err(format!(
            "the checkpoint was {} bytes, over its {max}-byte budget",
            text.len()
        ));
    }
    Ok(text)
}

#[derive(Clone)]
pub struct Registry {
    providers: Vec<Arc<dyn Provider>>,
}

impl Registry {
    pub fn new(providers: Vec<Arc<dyn Provider>>) -> Self {
        Self { providers }
    }

    pub fn get(&self, id: &str) -> Option<&Arc<dyn Provider>> {
        self.providers.iter().find(|p| p.id() == id)
    }

    pub fn all(&self) -> &[Arc<dyn Provider>] {
        &self.providers
    }

    /// The first available provider in registration order.
    pub fn default_id(&self) -> Option<&'static str> {
        self.providers
            .iter()
            .find(|p| p.availability() == Availability::Available)
            .map(|p| p.id())
    }
}

#[cfg(test)]
mod executable_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_configured_mise_shim_is_resolved_to_the_native_executable() {
        let dir = tempfile::tempdir().unwrap();
        let native = dir.path().join("installs/codex/1.0/bin/codex");
        std::fs::create_dir_all(native.parent().unwrap()).unwrap();
        std::fs::write(&native, "native").unwrap();
        let mise = dir.path().join("bin/mise");
        std::fs::create_dir_all(mise.parent().unwrap()).unwrap();
        std::fs::write(
            &mise,
            format!(
                "#!/bin/sh\n[ \"$1 $2\" = \"which codex\" ] && echo {}\n",
                native.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&mise, std::fs::Permissions::from_mode(0o755)).unwrap();
        let shim = dir.path().join("shims/codex");
        std::fs::create_dir_all(shim.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&mise, &shim).unwrap();

        let native = std::fs::canonicalize(&native).unwrap();
        assert_eq!(executable(Some(&shim), "codex"), Some(native.clone()));
        assert_eq!(executable(Some(&native), "codex"), Some(native));
        let missing = dir.path().join("nope/codex");
        assert_eq!(executable(Some(&missing), "codex"), Some(missing));
    }
}

#[cfg(test)]
pub(crate) mod recording {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use crate::app::providers::{Fenced, Selection, SessionState, TurnSink};

    #[derive(Default)]
    pub struct Recorded {
        pub sessions: Mutex<Vec<SessionState>>,
    }

    impl Recorded {
        pub fn last_session(&self) -> SessionState {
            self.sessions.lock().unwrap().last().cloned().unwrap()
        }
    }

    #[async_trait]
    impl TurnSink for Recorded {
        async fn packet(&self, _: &Selection, _: &str) -> Result<(), Fenced> {
            Ok(())
        }
        async fn sending(&self) -> Result<(), Fenced> {
            Ok(())
        }
        async fn running(&self) -> Result<(), Fenced> {
            Ok(())
        }
        async fn text(&self, _: &str) -> Result<(), Fenced> {
            Ok(())
        }
        async fn end_message(&self) -> Result<(), Fenced> {
            Ok(())
        }
        async fn session(&self, state: &SessionState) -> Result<(), Fenced> {
            self.sessions.lock().unwrap().push(state.clone());
            Ok(())
        }
        async fn reported(&self, _: &serde_json::Value, _: &str) -> Result<(), Fenced> {
            Ok(())
        }
    }

    pub fn received(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
