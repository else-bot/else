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

const USAGE: &str = "usage: else-api [serve|setup|migrate]\n\n\
commands:\n  \
serve    apply pending migrations and run the API (default)\n  \
setup    provision missing credentials, apply migrations, and create the main\n           \
bot and default provider profiles once; never replaces existing state\n  \
migrate  apply pending versioned migrations only\n";

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
    if args.len() > 1 {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let result = match command {
        "serve" => run_serve(),
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
    let _secret_key = credentials::load_secret_key(&config.secret_key)?;
    runtime()?.block_on(async {
        let pool = database(&config).await?;
        db::installation::check_installed(&pool).await?;
        // Take the singleton locks and bind before touching runtime state: a
        // second instance must not reconcile or dispatch another's work.
        let _lock = else_api::runtime_lock::acquire(&config.data_dir, &pool).await?;
        let listener = TcpListener::bind(config.bind)
            .await
            .with_context(|| format!("binding {}", config.bind))?;
        let registry = else_api::providers::registry(&config);
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
        let events = else_api::events::EventHub::start(pool.clone(), stop_scheduler.clone());
        let state = Arc::new(http::AppState::new(owner_token, pool, scheduler, events));
        let result = serve(&config, listener, state).await;
        // Stop provider work after HTTP so no new message is accepted.
        stop_scheduler.cancel();
        let _ = scheduler_task.await;
        tracing::info!("API stopped");
        result
    })
}

async fn serve(
    config: &Config,
    listener: TcpListener,
    state: Arc<http::AppState>,
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
