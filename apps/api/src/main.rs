//! else.bot API executable: command parsing and process composition only.

mod config;
mod credentials;
mod http;

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use config::{Config, ProcessEnv};

const USAGE: &str = "usage: else-api [serve|setup]\n\n\
commands:\n  serve   run the API (default)\n  \
setup   provision missing credentials once; never replaces existing ones\n";

fn main() -> ExitCode {
    // A local `.env` is a development convenience. Existing process variables
    // take precedence, and a missing file is not an error.
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
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

/// Explicit installation setup. Creates only what is missing and reports
/// paths, never credential values.
fn run_setup() -> anyhow::Result<()> {
    use credentials::Provisioned;
    let config = load_config()?;
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
    Ok(())
}

fn run_serve() -> anyhow::Result<()> {
    let config = load_config()?;
    // Credentials load before the listener opens. Neither is logged.
    let owner_token = credentials::load_owner_token(&config.owner_token_file)?;
    let _secret_key = credentials::load_secret_key(&config.secret_key)?;
    let state = Arc::new(http::AppState::new(owner_token));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting async runtime")?;
    runtime.block_on(serve(config, state))
}

async fn serve(config: Config, state: Arc<http::AppState>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("binding {}", config.bind))?;
    tracing::info!(addr = %listener.local_addr()?, "API listening");

    let app = http::router(&config, state);
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
    tracing::info!("API stopped");
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
