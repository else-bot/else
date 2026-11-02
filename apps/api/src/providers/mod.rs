//! Provider adapters and the registry the API exposes through capabilities.

pub mod codex;
pub mod codex_home;
pub mod codex_rpc;
pub mod host;
pub mod synthetic;

use std::sync::Arc;

use crate::app::providers::{Availability, Provider};
use crate::config::Config;

/// Providers enabled by configuration, in default-selection order. Codex is
/// always listed; it reports unavailable (with a reason) when bubblewrap,
/// the CLI, or its host login is missing.
pub fn registry(config: &Config) -> Registry {
    let mut providers: Vec<Arc<dyn Provider>> = Vec::new();
    let sandbox = host::HostSandbox::detect(&config.data_dir).ok();
    let program = config.codex_bin.clone().or_else(|| find_on_path("codex"));
    let auth = codex_home::host_auth_file(
        std::env::var("CODEX_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    );
    providers.push(Arc::new(codex::Codex::detect(sandbox, program, auth)));
    if config.synthetic_provider {
        providers.push(Arc::new(synthetic::Synthetic::new(
            std::time::Duration::from_millis(40),
        )));
    }
    Registry::new(providers)
}

/// An executable on `PATH`, resolved to its real file.
fn find_on_path(name: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|dir| std::path::Path::new(dir).join(name))
        .find(|path| path.is_file())
        .and_then(|path| std::fs::canonicalize(path).ok())
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
