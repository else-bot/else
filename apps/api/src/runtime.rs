//! Builds the API's services in dependency order and owns their background
//! tasks. `main` and the integration-test harness both start the API
//! through [`start`], so a startup change reaches both.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::computers::ComputerManager;
use crate::config::Config;
use crate::credentials::{OwnerToken, SecretKey};
use crate::http::AppState;
use crate::scheduler::Scheduler;

/// Bot computers, or why they did not start (reported by `else_status`).
pub enum Computers {
    Ready(Arc<ComputerManager>),
    Unavailable(String),
}

impl Computers {
    pub fn manager(&self) -> Option<&Arc<ComputerManager>> {
        match self {
            Self::Ready(manager) => Some(manager),
            Self::Unavailable(_) => None,
        }
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        match self {
            Self::Ready(_) => None,
            Self::Unavailable(reason) => Some(reason),
        }
    }
}

/// Detect the computer runtime and reconcile recorded computer states with
/// it. A missing runtime is `Unavailable`; a failed reconcile is an error.
pub async fn open_computers(
    config: &Config,
    pool: &SqlitePool,
    key: Option<Arc<SecretKey>>,
) -> anyhow::Result<Computers> {
    let msb = match crate::computers::runtime::Msb::detect(&config.data_dir, config.msb_bin.clone())
    {
        Ok(msb) => msb,
        Err(err) => {
            tracing::warn!(error = %err, "bot computers unavailable");
            return Ok(Computers::Unavailable(err.to_string()));
        }
    };
    let mut manager = ComputerManager::new(pool.clone(), Arc::new(msb), config.computers.clone());
    if let Some(key) = key {
        manager = manager.with_secret_key(key);
    }
    manager.reconcile_startup().await?;
    Ok(Computers::Ready(Arc::new(manager)))
}

/// What [`start`] builds the API from. Tests substitute the provider
/// registry, transports, skill sources, and computer runtime.
pub struct Parts {
    pub owner_token: OwnerToken,
    pub secret_key: Arc<SecretKey>,
    pub pool: SqlitePool,
    pub registry: crate::providers::Registry,
    pub memory: Memory,
    pub computers: ComputerSource,
    pub mail: Arc<dyn crate::connectors::types::MailTransport>,
    pub social: Arc<dyn crate::socials::providers::Transport>,
    pub skill_sources: crate::skills::Sources,
    /// How long an interrupted provider may take to stop.
    pub cancel_grace: Duration,
    pub backups: Backups,
}

/// How bot memory is compacted.
pub enum Memory {
    /// Without a summarizer: deterministic compaction only.
    Deterministic,
    Summarized(Arc<dyn crate::memory_worker::Summarizer>),
    /// No background compactor: the caller drives compaction itself.
    Manual,
}

/// Where bot computers come from.
pub enum ComputerSource {
    /// Detect the runtime once recovery has run, as [`open_computers`].
    Detect,
    Given(Computers),
}

/// Whether to start the backup service.
pub enum Backups {
    Start,
    /// Leave it off, with the reason `else_status` reports.
    Off(String),
}

impl Parts {
    /// The production parts for an installation.
    pub async fn production(
        config: &Config,
        owner_token: OwnerToken,
        secret_key: Arc<SecretKey>,
        pool: SqlitePool,
    ) -> Self {
        let (registry, summarizer) = crate::providers::build(config).await;
        Self {
            owner_token,
            secret_key,
            pool,
            registry,
            memory: match summarizer {
                Some(summarizer) => Memory::Summarized(summarizer),
                None => Memory::Deterministic,
            },
            computers: ComputerSource::Detect,
            mail: Arc::new(crate::connectors::pimalaya::Pimalaya),
            social: Arc::new(crate::socials::providers::Native),
            skill_sources: crate::skills::Sources::default(),
            cancel_grace: crate::scheduler::DEFAULT_CANCEL_GRACE,
            backups: Backups::Start,
        }
    }
}

/// The background tasks of a started API. They run until `shutdown` is
/// cancelled, which dropping this does; [`Background::stop`] also waits
/// for them.
pub struct Background {
    tasks: TaskTracker,
    shutdown: CancellationToken,
    computers: Option<Arc<ComputerManager>>,
}

impl Background {
    fn spawn(&self, name: &'static str, task: impl Future<Output = ()> + Send + 'static) {
        let task = tokio::spawn(task);
        self.tasks.spawn(async move {
            if let Err(err) = task.await
                && err.is_panic()
            {
                tracing::error!(task = name, error = %err, "background task panicked");
            }
        });
    }

    /// Cancel the background loops, stop computer upgrades, and wait until
    /// every task has ended.
    pub async fn stop(self) {
        self.shutdown.cancel();
        if let Some(computers) = &self.computers {
            computers.stop_upgrades().await;
        }
        self.tasks.close();
        self.tasks.wait().await;
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Recover what an earlier process left, build the services, and start
/// their loops. The caller holds the runtime lock, has migrated the
/// database and bound the listener, and publishes the lock once this
/// returns.
pub async fn start(
    config: &Config,
    parts: Parts,
    shutdown: CancellationToken,
) -> anyhow::Result<(Arc<AppState>, Background)> {
    let Parts {
        owner_token,
        secret_key,
        pool,
        registry,
        memory,
        computers,
        mail,
        social,
        skill_sources,
        cancel_grace,
        backups,
    } = parts;
    let mcp_root = config.data_dir.join("mcp");
    let swept =
        crate::providers::host::Host::new(&config.data_dir, &config.bots_dir).sweep_orphans()?;
    if swept > 0 {
        tracing::warn!(
            swept,
            "terminated provider processes left by an earlier API"
        );
    }
    crate::mcp::cleanup(&mcp_root)?;
    let (requeued, uncertain) = crate::db::turns::lifecycle::recover_after_restart(&pool).await?;
    crate::connectors::store::recover(&pool).await?;
    crate::socials::store::recover(&pool).await?;
    if requeued + uncertain > 0 {
        tracing::info!(requeued, uncertain, "reconciled unfinished turns");
    }

    let computers = match computers {
        ComputerSource::Detect => open_computers(config, &pool, Some(secret_key.clone())).await?,
        ComputerSource::Given(computers) => computers,
    };
    let background = Background {
        tasks: TaskTracker::new(),
        shutdown: shutdown.clone(),
        computers: computers.manager().cloned(),
    };
    let model_cache = crate::providers::models::cache_path(&config.data_dir);
    crate::providers::models::restore(&registry, &model_cache);
    let model_cache = crate::providers::models::Cache::new(model_cache);
    background.spawn(
        "model refresh",
        crate::providers::models::run(registry.clone(), model_cache.clone(), shutdown.clone()),
    );

    let gate = Arc::new(crate::maintenance::Gate::default());
    let grants = Arc::new(crate::grants::Grants::default());
    let scheduler = Scheduler::new(
        crate::scheduler::Services {
            pool: pool.clone(),
            registry: registry.clone(),
            gate: gate.clone(),
            grants: grants.clone(),
            mcp_root: mcp_root.clone(),
            computers: computers.manager().cloned(),
        },
        config.max_active_turns,
        cancel_grace,
    );
    background.spawn(
        "scheduler",
        scheduler
            .clone()
            .run(shutdown.clone(), config.shutdown_grace),
    );
    let compactor = match memory {
        Memory::Deterministic => Some(crate::memory_worker::Compactor::deterministic(pool.clone())),
        Memory::Summarized(summarizer) => Some(crate::memory_worker::Compactor::with_summaries(
            pool.clone(),
            summarizer,
            scheduler.clone(),
        )),
        Memory::Manual => None,
    };
    if let Some(compactor) = compactor {
        background.spawn(
            "compactor",
            compactor.with_gate(gate.clone()).run(shutdown.clone()),
        );
    }
    let events = crate::events::EventHub::start(pool.clone(), shutdown.clone());

    if let Some(computers) = computers.manager() {
        background.spawn("computers", computers.clone().run(shutdown.clone()));
    }
    let backups = match backups {
        Backups::Start => match crate::backup::service::Backups::start(
            config,
            pool.clone(),
            secret_key.clone(),
            crate::backup::capture::Coordinator {
                turns: Some(crate::backup::capture::Turns {
                    scheduler: scheduler.clone(),
                    gate: gate.clone(),
                }),
                computers: computers.manager().cloned(),
                drain: Duration::from_secs(300),
                exclusive: false,
                projects_dir: Some(config.bots_dir.clone()),
                mcp_dir: Some(mcp_root.clone()),
            },
            shutdown.clone(),
        )
        .await
        {
            Ok((backups, task)) => {
                background.spawn("backups", async move {
                    if let Err(err) = task.await
                        && err.is_panic()
                    {
                        std::panic::resume_unwind(err.into_panic());
                    }
                });
                Ok(backups)
            }
            Err(error) => {
                tracing::warn!(error = %error, "backup service unavailable; API continues without backups");
                Err(error.to_string())
            }
        },
        Backups::Off(reason) => Err(reason),
    };

    let connectors = Arc::new(crate::connectors::ConnectorManager::new(
        pool.clone(),
        secret_key.clone(),
        scheduler.clone(),
        gate.clone(),
        mail,
    ));
    connectors.start_workers().await;
    background.spawn("connectors", connectors.clone().run(shutdown.clone()));
    let socials = Arc::new(crate::socials::Manager::new(
        pool.clone(),
        secret_key.clone(),
        scheduler.clone(),
        social,
    ));
    background.spawn("socials", socials.clone().run(shutdown.clone()));
    let mcps = crate::mcp::Library::new(mcp_root, gate.clone())
        .with_oauth_key(Some(secret_key.clone()))
        .with_oauth_client(config.github_oauth.clone())
        .with_task_secrets(crate::mcp::TaskSecrets::new(
            pool.clone(),
            Some(secret_key.clone()),
        ));

    let state = Arc::new(AppState {
        owner_token,
        db: pool,
        scheduler,
        registry,
        model_cache,
        gate,
        grants,
        events,
        server_instance_id: Uuid::new_v4(),
        auth_failures: Default::default(),
        computers,
        secret_key,
        backups,
        mcps,
        skill_sources,
        connectors,
        socials,
        update_hold: Default::default(),
        server_updates: crate::actor::Actor::new(crate::server_updates::Installation::discover(
            config,
        )),
        desktop_streams: Default::default(),
    });
    background.spawn(
        "MCP waits",
        crate::http::mcp_wait::run(state.clone(), shutdown),
    );
    Ok((state, background))
}

/// An API over a fresh installed database under `home`, with no
/// providers, computers or backups.
#[cfg(test)]
pub(crate) async fn start_in(home: &std::path::Path) -> (Config, Arc<AppState>, Background) {
    let config = Config::load(&crate::config::tests::MapSource::new(&[(
        "HOME",
        home.to_str().unwrap(),
    )]))
    .unwrap();
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let pool = crate::db::connect(&config.database_url).await.unwrap();
    crate::db::migrate(&pool).await.unwrap();
    crate::db::installation::setup(&pool).await.unwrap();
    let parts = Parts {
        owner_token: OwnerToken::parse(crate::http::tests::TOKEN).unwrap(),
        secret_key: Arc::new(
            SecretKey::parse("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap(),
        ),
        pool,
        registry: crate::providers::Registry::new(vec![]),
        memory: Memory::Deterministic,
        computers: ComputerSource::Given(Computers::Unavailable(
            "unit tests run no computers".into(),
        )),
        mail: Arc::new(crate::connectors::pimalaya::Pimalaya),
        social: Arc::new(crate::socials::providers::Native),
        skill_sources: crate::skills::Sources::default(),
        cancel_grace: crate::scheduler::DEFAULT_CANCEL_GRACE,
        backups: Backups::Off("unit tests run no backups".into()),
    };
    let (state, background) = start(&config, parts, CancellationToken::new())
        .await
        .unwrap();
    (config, state, background)
}
