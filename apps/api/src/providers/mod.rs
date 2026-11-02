//! Provider adapters and the registry the API exposes through capabilities.

pub mod codex_home;
pub mod codex_rpc;
pub mod host;
pub mod synthetic;

use std::sync::Arc;

use crate::app::providers::{Availability, Provider};
use crate::config::Config;

/// Providers enabled by configuration, in default-selection order.
pub fn registry(config: &Config) -> Registry {
    let mut providers: Vec<Arc<dyn Provider>> = Vec::new();
    if config.synthetic_provider {
        providers.push(Arc::new(synthetic::Synthetic::new(
            std::time::Duration::from_millis(40),
        )));
    }
    Registry::new(providers)
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
