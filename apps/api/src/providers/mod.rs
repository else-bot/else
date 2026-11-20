pub mod attachments;
pub mod claude;
pub mod claude_home;
pub mod cli;
pub mod codex;
pub mod codex_home;
pub mod codex_rpc;
pub(crate) mod context;
pub mod cursor;
pub mod cursor_home;
mod default_skills;
pub mod definitions;
pub mod executable;
#[cfg(test)]
mod fingerprints;
pub mod grok;
pub mod grok_home;
pub mod host;
pub mod models;
pub mod output_files;
pub mod prompt;
pub mod synthetic;

use std::sync::Arc;

use executable::{Program, executable, executable_as};

use crate::app::providers::{Availability, Provider};
use crate::config::Config;
use crate::memory_worker::Summarizer;

/// What a CLI definition's constructor gets: the configuration (its
/// executable override), the host, and the else.bot tool endpoint.
pub struct Wiring<'a> {
    pub config: &'a Config,
    pub host: host::Host,
    pub tool_url: String,
}

/// A constructed CLI provider, and the same instance as the memory
/// summarizer when it is one.
pub struct Built {
    pub provider: Arc<dyn Provider>,
    pub summarizer: Option<Arc<dyn Summarizer>>,
}

/// The registry, one provider per [`definitions::DEFINITIONS`] entry in its
/// order and then the optional synthetic one, and the no-tools summarizer
/// for memory.
pub async fn build(config: &Config) -> (Registry, Option<Arc<dyn Summarizer>>) {
    let mut tool_addr = config.bind;
    if tool_addr.ip().is_unspecified() {
        tool_addr.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
    }
    let wiring = Wiring {
        config,
        host: host::Host::new(&config.data_dir, &config.bots_dir),
        tool_url: format!("http://{tool_addr}/tools/v1/mcp"),
    };
    let mut providers: Vec<Arc<dyn Provider>> = Vec::new();
    let mut summarizer = None;
    let built = futures_util::future::join_all(
        definitions::DEFINITIONS
            .iter()
            .map(|definition| (definition.build)(&wiring)),
    )
    .await;
    for built in built {
        summarizer = summarizer.or(built.summarizer);
        providers.push(built.provider);
    }
    if config.synthetic_provider {
        providers.push(Arc::new(synthetic::Synthetic::new(
            std::time::Duration::from_millis(40),
        )));
    }
    (Registry::new(providers), summarizer)
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
mod registration_tests {
    use super::*;
    use crate::app::providers::Profile;

    struct Env(Vec<(&'static str, String)>);

    impl crate::config::Source for Env {
        fn get(&self, name: &str) -> Option<String> {
            self.0
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        }
    }

    /// Runtime construction, its order, and offline restore validation all
    /// come from the same `DEFINITIONS` entries. Every CLI is configured at a
    /// missing path, so nothing logs in or calls a model.
    #[tokio::test]
    async fn the_registry_is_built_from_the_definitions_that_restore_validates() {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name).display().to_string();
        let config = Config::load(&Env(vec![
            ("HOME", path("home")),
            ("ELSE_DATA_DIR", path("data")),
            ("ELSE_BOTS_DIR", path("bots")),
            ("ELSE_CODEX_BIN", path("missing/codex")),
            ("ELSE_CLAUDE_BIN", path("missing/claude")),
            ("ELSE_GROK_BIN", path("missing/grok")),
            ("ELSE_CURSOR_BIN", path("missing/cursor-agent")),
            ("ELSE_SYNTHETIC_PROVIDER", "true".into()),
        ]))
        .unwrap();
        let (registry, summarizer) = build(&config).await;

        let ids: Vec<&str> = registry.all().iter().map(|p| p.id()).collect();
        let mut expected: Vec<&str> = definitions::DEFINITIONS.iter().map(|d| d.id).collect();
        assert_eq!(expected, ["codex", "claude", "grok", "cursor"]);
        expected.push(synthetic::ID);
        assert_eq!(ids, expected);

        let rejected = Profile {
            model: "unknown".into(),
            reasoning_effort: "high".into(),
            service_tier: "default".into(),
        };
        for (definition, provider) in definitions::DEFINITIONS.iter().zip(registry.all()) {
            assert_eq!(provider.label(), definition.label);
            let catalog = provider.catalog();
            let mut shown = catalog.ids();
            shown.sort_unstable();
            let mut bundled = definition.fallback.to_vec();
            bundled.sort_unstable();
            assert_eq!(
                shown, bundled,
                "the bundled list until the CLI lists its own"
            );
            let setting = format!("ELSE_{}_BIN", definition.id.to_uppercase());
            assert!(
                matches!(provider.availability(), Availability::Unavailable(why) if why.contains(&setting)),
                "{setting} is named in {:?}",
                provider.availability()
            );
            assert!(provider.validate_profile(&rejected).is_err());
        }
        assert_eq!(registry.default_id(), Some(synthetic::ID));

        let summarizer = summarizer.expect("Codex summarizes memory");
        let codex = registry.get(codex::ID).unwrap();
        assert_eq!(
            Arc::as_ptr(&summarizer).cast::<()>(),
            Arc::as_ptr(codex).cast::<()>(),
            "the summarizer is the registered Codex"
        );
    }

    /// Each fake CLI waits until both have started `--version`, so
    /// probes run one after another would log an end before the next start.
    #[tokio::test]
    async fn startup_probes_run_concurrently() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name).display().to_string();
        let log = dir.path().join("log");
        let script = format!(
            "#!/bin/sh\n\
             echo \"start $(basename \"$0\") $*\" >> {log}\n\
             i=0\n\
             while [ \"$(grep -c -- '--version' {log})\" -lt 2 ] && [ $i -lt 200 ]; do sleep 0.1; i=$((i+1)); done\n\
             echo \"end $(basename \"$0\")\" >> {log}\n\
             echo 2099.1.1\n",
            log = log.display()
        );
        for name in ["claude", "cursor-agent"] {
            let program = dir.path().join(name);
            std::fs::write(&program, &script).unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let config = Config::load(&Env(vec![
            ("HOME", path("home")),
            ("ELSE_DATA_DIR", path("data")),
            ("ELSE_BOTS_DIR", path("bots")),
            ("ELSE_CODEX_BIN", path("missing/codex")),
            ("ELSE_CLAUDE_BIN", path("claude")),
            ("ELSE_GROK_BIN", path("missing/grok")),
            ("ELSE_CURSOR_BIN", path("cursor-agent")),
        ]))
        .unwrap();
        build(&config).await;
        let log = std::fs::read_to_string(&log).unwrap();
        let mut first: Vec<&str> = log.lines().take(2).collect();
        first.sort_unstable();
        assert_eq!(
            first,
            ["start claude --version", "start cursor-agent --version"],
            "every CLI started its probe before any finished:\n{log}"
        );
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
