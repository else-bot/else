//! else.bot API executable: command parsing and process composition only.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use else_api::config::{Config, ProcessEnv};
use else_api::scheduler::Scheduler;
use else_api::{credentials, db, http};
use tokio_util::sync::CancellationToken;

const USAGE: &str = "usage: else-api [serve|setup|migrate|backup]\n       \
else-api restore <set-dir> <new-database> [<new-data-dir>]\n       \
else-api restore-remote <set-id> <new-database> [<new-data-dir>]\n\n\
commands:\n  \
serve    apply pending migrations and run the API (default)\n  \
setup    provision missing credentials, apply migrations, and create the main\n           \
bot and default provider profiles once; never replaces existing state\n  \
migrate  apply pending versioned migrations only\n  \
backup   capture one encrypted database backup set now\n  \
restore  verify a backup set and restore it into a new database on the same\n           \
server; never changes an existing database or file\n  \
restore-remote  download a completed set from the configured bucket, then restore it\n";

fn main() -> ExitCode {
    // A local `.env` is a development convenience. Existing process variables
    // take precedence, and a missing file is not an error. Any other problem
    // fails startup rather than silently ignoring settings. Line contents are
    // not printed because they may hold credentials.
    if let Err(err) = dotenvy::dotenv() {
        if !err.not_found() {
            let detail = match &err {
                dotenvy::Error::LineParse(_, index) => format!("parse error near byte {index}"),
                dotenvy::Error::Io(io) => io.to_string(),
                _ => "unreadable".to_owned(),
            };
            eprintln!("error: invalid .env file: {detail}");
            return ExitCode::FAILURE;
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("serve");
    let arity_ok = match command {
        "restore" | "restore-remote" => args.len() == 3 || args.len() == 4,
        _ => args.len() <= 1,
    };
    if !arity_ok {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let result = match command {
        "serve" => run_serve(),
        "backup" => run_backup(),
        "restore" => run_restore(&args[1], &args[2], args.get(3).map(String::as_str)),
        "restore-remote" => run_restore_remote(&args[1], &args[2], args.get(3).map(String::as_str)),
        "setup" => run_setup(),
        "migrate" => run_migrate(),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("unknown command `{command}`\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn load_config() -> anyhow::Result<Config> {
    Config::load(&ProcessEnv).context("invalid configuration")
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting async runtime")
}

async fn database(config: &Config) -> anyhow::Result<sqlx::PgPool> {
    let url = config.require_database().context("invalid configuration")?;
    let pool = db::connect(url).await?;
    db::migrate(&pool).await?;
    Ok(pool)
}

fn backup_parts(
    config: &else_api::config::Config,
) -> anyhow::Result<(else_api::backup::PgParams, Arc<credentials::SecretKey>)> {
    let url = config.require_database().context("invalid configuration")?;
    let params = else_api::backup::PgParams::parse(url.expose())?;
    // A missing key is an error; backups never generate one.
    let key = credentials::load_secret_key(&config.secret_key)?;
    Ok((params, Arc::new(key)))
}

/// Capture one set now. The API must be stopped (its singleton lock is
/// taken here), so the set covers the database and every bot computer at one
/// checkpoint; a running API backs up on its own schedule.
fn run_backup() -> anyhow::Result<()> {
    let config = load_config()?;
    let (params, key) = backup_parts(&config)?;
    runtime()?.block_on(async {
        // Lock before migrating: a running (possibly older) API's database
        // is never changed by this command.
        let url = config.require_database().context("invalid configuration")?;
        let pool = db::connect(url).await?;
        let _lock = else_api::runtime_lock::acquire(&config.data_dir, &pool)
            .await
            .context("the API is running; it backs up on its schedule (or stop it first)")?;
        db::migrate(&pool).await?;
        let computers = match else_api::computers::runtime::Msb::detect(
            &config.data_dir,
            config.msb_bin.clone(),
        ) {
            Ok(msb) => {
                let manager = Arc::new(else_api::computers::ComputerManager::new(
                    pool.clone(),
                    Arc::new(msb),
                    config.computers.clone(),
                ));
                manager.reconcile_startup().await?;
                Some(manager)
            }
            Err(err) => {
                tracing::warn!(error = %err, "bot computers unavailable; database-only set");
                None
            }
        };
        let mut worker = else_api::backup::worker::BackupWorker::new(
            pool.clone(),
            params,
            key,
            config.backup.clone(),
        )
        .with_coordinator(else_api::backup::capture::Coordinator {
            scheduler: None,
            computers,
            drain: std::time::Duration::from_secs(0),
            exclusive: true,
        });
        if let Some(replicator) = replicator(&config, &pool)? {
            worker = worker.with_replicator(replicator);
        }
        let summary = worker.run_once().await?;
        println!("backup set: {}", summary.path.display());
        println!("coverage: {}", summary.manifest.coverage.note);
        match &summary.remote {
            Some(Ok(stats)) => println!("remote: {} sets replicated", stats.completed.len()),
            Some(Err(err)) => println!("remote: failed ({err}); the local set is complete"),
            None => {}
        }
        Ok(())
    })
}

fn replicator(
    config: &else_api::config::Config,
    pool: &sqlx::PgPool,
) -> anyhow::Result<Option<else_api::backup::replicate::Replicator>> {
    let Some(s3) = config.s3.clone() else {
        return Ok(None);
    };
    Ok(Some(else_api::backup::replicate::Replicator::new(
        else_api::backup::s3::S3::new(s3)?,
        pool.clone(),
        config.backup.keep,
    )))
}

/// Download a completed remote set, then restore it like a local one.
fn run_restore_remote(
    set_id: &str,
    target: &str,
    new_data_dir: Option<&str>,
) -> anyhow::Result<()> {
    let config = load_config()?;
    let (_, key) = backup_parts(&config)?;
    let downloads = config.data_dir.join("restore-download");
    // Only the bucket is needed to download: the source database may not
    // exist on a replacement server.
    let s3 = else_api::backup::s3::S3::new(
        config
            .s3
            .clone()
            .context("no bucket is configured (set ELSE_S3_*)")?,
    )?;
    let set_dir = runtime()?.block_on(else_api::backup::replicate::download_set(
        &s3,
        set_id,
        &downloads,
        &key,
        config.backup.max_bytes,
    ))?;
    let result = run_restore(&set_dir.display().to_string(), target, new_data_dir);
    let _ = std::fs::remove_dir_all(&set_dir);
    result
}

/// Restore a verified set into a new database (and, for bot computers, a
/// new installation directory with its own runtime home). The API is not
/// started and nothing existing is changed; point the API at the new
/// database and directory explicitly.
fn run_restore(set_dir: &str, target: &str, new_data_dir: Option<&str>) -> anyhow::Result<()> {
    let config = load_config()?;
    let (params, key) = backup_parts(&config)?;
    let new_dir = new_data_dir.map(std::path::PathBuf::from);
    if let Some(dir) = &new_dir {
        anyhow::ensure!(
            dir.is_absolute(),
            "the new installation directory must be absolute"
        );
        // An exclusive create: a second restore to the same path fails here
        // and never touches this one's files.
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .with_context(|| {
                format!(
                    "{} cannot be created as new; nothing was changed",
                    dir.display()
                )
            })?;
        // Credentials first, so a failure here leaves no restored database.
        let written = credentials::write_secret_key(&dir.join("secret-key"), &key)
            .and_then(|_| credentials::provision_owner_token(&dir.join("owner-token")).map(|_| ()));
        if let Err(err) = written {
            let _ = std::fs::remove_dir_all(dir);
            return Err(err.into());
        }
    }
    let result = runtime()?.block_on(async {
        let runtime: Option<Arc<dyn else_api::computers::ComputerRuntime>> = match &new_dir {
            Some(dir) => Some(Arc::new(else_api::computers::runtime::Msb::detect(
                dir,
                config.msb_bin.clone(),
            )?)),
            None => None,
        };
        let report =
            else_api::backup::restore::restore(else_api::backup::restore::RestoreRequest {
                set_dir: std::path::Path::new(set_dir),
                key: &key,
                server: &params,
                target_database: target,
                pg_restore: &config.backup.pg_restore,
                staging_parent: &config.data_dir.join("restore-staging"),
                max_bytes: config.backup.max_bytes,
                computers: match (&runtime, &new_dir) {
                    (Some(runtime), Some(dir)) => Some(else_api::backup::restore::ComputerTarget {
                        runtime: runtime.clone(),
                        data_dir: dir,
                    }),
                    _ => None,
                },
            })
            .await?;
        println!("restored set {} into database {target}", report.set_id);
        if let Some(dir) = &new_dir {
            println!("new installation directory: {}", dir.display());
        }
        println!("migration version: {}", report.migration_version);
        println!("coverage: {}", report.manifest.coverage.note);
        anyhow::Ok(())
    });
    if let Err(err) = &result {
        let leftover = err
            .downcast_ref::<else_api::backup::BackupError>()
            .is_some_and(|e| matches!(e, else_api::backup::BackupError::Leftover(_)));
        if let Some(dir) = new_dir.as_ref().filter(|_| !leftover) {
            // Only the directory this restore created, and only when
            // nothing it started may still run.
            let _ = std::fs::remove_dir_all(dir);
        }
    }
    result
}

fn run_migrate() -> anyhow::Result<()> {
    let config = load_config()?;
    runtime()?.block_on(async {
        database(&config).await?;
        println!("migrations: up to date");
        Ok(())
    })
}

/// Explicit installation setup. Creates only what is missing and reports
/// paths, never credential values.
fn run_setup() -> anyhow::Result<()> {
    use credentials::Provisioned;
    let config = load_config()?;
    // Validate everything setup will need before writing any file.
    let url = config.require_database().context("invalid configuration")?;
    url.expose()
        .parse::<sqlx::postgres::PgConnectOptions>()
        .map_err(|_| db::DbError::InvalidUrl)?;
    credentials::ensure_private_dir(&config.data_dir)?;
    let describe = |what: &str, result: Provisioned, location: String| {
        let state = match result {
            Provisioned::Created => "created",
            Provisioned::Existing => "kept existing",
            Provisioned::FromEnvironment => "validated from environment",
        };
        println!("{what}: {state} ({location})");
    };
    let token = credentials::provision_owner_token(&config.owner_token_file)?;
    describe(
        "owner token",
        token,
        config.owner_token_file.display().to_string(),
    );
    let key = credentials::provision_secret_key(&config.secret_key)?;
    let location = match &config.secret_key {
        credentials::SecretKeySource::Env(_) => "SECRET_KEY".to_owned(),
        credentials::SecretKeySource::File(path) => path.display().to_string(),
    };
    describe("secret key", key, location);

    runtime()?.block_on(async {
        let pool = database(&config).await?;
        let report = db::installation::setup(&pool).await?;
        for provider in &report.seeded_profiles {
            println!("provider profile {provider}: created default");
        }
        match report.main_bot {
            db::installation::MainBot::Created(id) => println!("main bot: created ({id})"),
            db::installation::MainBot::AlreadyInstalled(Some(id)) => {
                println!("main bot: installation already set up; default bot {id} kept")
            }
            db::installation::MainBot::AlreadyInstalled(None) => {
                println!("main bot: installation already set up; no default bot (kept empty)")
            }
        }
        Ok(())
    })
}

fn run_serve() -> anyhow::Result<()> {
    let config = load_config()?;
    // Credentials load before the listener opens. Neither is logged.
    let owner_token = credentials::load_owner_token(&config.owner_token_file)?;
    let secret_key = Arc::new(credentials::load_secret_key(&config.secret_key)?);
    runtime()?.block_on(async {
        let pool = database(&config).await?;
        db::installation::check_installed(&pool).await?;
        // Take the singleton locks and bind before touching runtime state: a
        // second instance must not reconcile or dispatch another's work.
        let mut lock = else_api::runtime_lock::acquire(&config.data_dir, &pool).await?;
        let lock_lost = CancellationToken::new();
        lock.watch(lock_lost.clone());
        let listener = TcpListener::bind(config.bind)
            .await
            .with_context(|| format!("binding {}", config.bind))?;
        // Terminate provider sandboxes left by an earlier API process before
        // anything is reconciled or dispatched.
        match else_api::providers::host::HostSandbox::detect(&config.data_dir) {
            Ok(sandbox) => {
                let swept = sandbox.sweep_orphans()?;
                if swept > 0 {
                    tracing::warn!(
                        swept,
                        "terminated provider processes left by an earlier API"
                    );
                }
            }
            Err(err) => tracing::warn!(error = %err, "host provider sandbox unavailable"),
        }
        let (registry, summarizer) = else_api::providers::build(&config);
        // Reconcile interrupted work before the scheduler admits anything.
        let (requeued, uncertain) = db::turns::recover_after_restart(&pool).await?;
        if requeued + uncertain > 0 {
            tracing::info!(requeued, uncertain, "reconciled unfinished turns");
        }
        let scheduler = Scheduler::new(pool.clone(), registry, config.max_active_turns);
        let stop_scheduler = CancellationToken::new();
        let scheduler_task = tokio::spawn(
            scheduler
                .clone()
                .run(stop_scheduler.clone(), config.shutdown_grace),
        );
        // Losing the singleton lock stops dispatch at once.
        let stop_on_loss = stop_scheduler.clone();
        let lost = lock_lost.clone();
        tokio::spawn(async move {
            lost.cancelled().await;
            stop_on_loss.cancel();
        });
        // Builds memory tree nodes; stops with the scheduler.
        let compactor = match summarizer {
            Some(summarizer) => else_api::memory_worker::Compactor::with_summaries(
                pool.clone(),
                summarizer,
                scheduler.clone(),
            ),
            None => else_api::memory_worker::Compactor::deterministic(pool.clone()),
        };
        let compactor = tokio::spawn(
            compactor
                .with_gate(scheduler.gate().clone())
                .run(stop_scheduler.clone()),
        );
        let events = else_api::events::EventHub::start(pool.clone(), stop_scheduler.clone());
        // Bot computers: inspect the runtime (never waking or recreating
        // anything), then maintain leases and idle computers.
        let mut state = http::AppState::new(owner_token, pool.clone(), scheduler, events);
        let mut computer_task = None;
        match else_api::computers::runtime::Msb::detect(&config.data_dir, config.msb_bin.clone()) {
            Ok(msb) => {
                let manager = Arc::new(else_api::computers::ComputerManager::new(
                    pool.clone(),
                    Arc::new(msb),
                    config.computers.clone(),
                ));
                manager.reconcile_startup().await?;
                computer_task = Some(tokio::spawn(manager.clone().run(stop_scheduler.clone())));
                state = state.with_computers(manager);
            }
            Err(err) => tracing::warn!(error = %err, "bot computers unavailable"),
        }
        // The serialized backup worker (scheduled local sets).
        let backup_task = match config
            .database_url
            .as_ref()
            .map(|url| else_api::backup::PgParams::parse(url.expose()))
        {
            Some(Ok(params)) if !config.backup.interval.is_zero() => {
                let mut worker = else_api::backup::worker::BackupWorker::new(
                    pool.clone(),
                    params,
                    secret_key.clone(),
                    config.backup.clone(),
                )
                .with_coordinator(else_api::backup::capture::Coordinator {
                    scheduler: Some(state.scheduler.clone()),
                    computers: state.computers.clone(),
                    drain: std::time::Duration::from_secs(300),
                    exclusive: false,
                });
                match replicator(&config, &pool) {
                    Ok(Some(replicator)) => worker = worker.with_replicator(replicator),
                    Ok(None) => {}
                    Err(err) => tracing::warn!(error = %err, "backup replication disabled"),
                }
                let worker = Arc::new(worker);
                Some(tokio::spawn(worker.run(stop_scheduler.clone())))
            }
            Some(Err(err)) => {
                tracing::warn!(error = %err, "backups disabled");
                None
            }
            _ => None,
        };
        let state = Arc::new(state);
        let result = serve(&config, listener, state, lock_lost.clone()).await;
        // Stop provider work after HTTP so no new message is accepted. The
        // singleton lock is held until every worker that changes runtime
        // state has stopped, so a successor never overlaps with them.
        stop_scheduler.cancel();
        let _ = scheduler_task.await;
        let _ = compactor.await;
        if let Some(task) = computer_task {
            let _ = task.await;
        }
        if let Some(task) = backup_task {
            let _ = task.await;
        }
        drop(lock);
        tracing::info!("API stopped");
        result
    })
}

async fn serve(
    config: &Config,
    listener: TcpListener,
    state: Arc<http::AppState>,
    lock_lost: CancellationToken,
) -> anyhow::Result<()> {
    tracing::info!(addr = %listener.local_addr()?, "API listening");

    let app = http::router(config, state);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut server = tokio::spawn(async move {
        let mut stop_rx = stop_rx;
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = stop_rx.wait_for(|stop| *stop).await;
            })
            .await
    });

    tokio::select! {
        result = &mut server => {
            return result.context("server task")?.context("serving HTTP");
        }
        signal = shutdown_signal() => signal?,
        _ = lock_lost.cancelled() => {
            let _ = stop_tx.send(true);
            server.abort();
            anyhow::bail!("lost the database singleton lock; another API may start, so this one stopped");
        }
    }
    tracing::info!("shutdown requested; draining connections");
    let _ = stop_tx.send(true);
    match tokio::time::timeout(config.shutdown_grace, &mut server).await {
        Ok(result) => result.context("server task")?.context("serving HTTP")?,
        Err(_) => {
            // Long-lived streams may not finish on their own. Dropping the
            // server task closes the listener and remaining connections.
            tracing::warn!(
                grace_secs = config.shutdown_grace.as_secs(),
                "grace period elapsed; closing remaining connections"
            );
            server.abort();
        }
    }
    Ok(())
}

async fn shutdown_signal() -> anyhow::Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing SIGTERM handler")?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.context("waiting for Ctrl-C")?,
        _ = term.recv() => {}
    }
    Ok(())
}
