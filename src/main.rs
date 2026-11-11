use std::error::Error;
use std::net::SocketAddr;
use std::process::ExitCode;

use git_archive::config::{Config, DEFAULT_PORT};
use git_archive::db::{self, repos};
use git_archive::i18n::Catalog;
use git_archive::web::{self, AppState};
use tracing_subscriber::EnvFilter;

const DATABASE_FILE: &str = "git-archive.db";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("git-archive: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let config = Config::from_env()?;
    init_tracing(&config.log_level)?;
    let state = build_state(config).await?;
    state.scheduler.clone().spawn();
    serve(state).await
}

fn init_tracing(level: &str) -> Result<(), Box<dyn Error>> {
    let filter = EnvFilter::try_new(level)?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init()
        .map_err(|err| -> Box<dyn Error> { err })
}

async fn build_state(config: Config) -> Result<AppState, Box<dyn Error>> {
    tokio::fs::create_dir_all(&config.data_dir).await?;
    let pool = db::open(&config.data_dir.join(DATABASE_FILE)).await?;
    let interrupted = repos::reset_interrupted(&pool).await?;
    if interrupted > 0 {
        tracing::warn!(
            interrupted,
            "syncs interrupted by the last shutdown will run again"
        );
    }
    tidy_disk(&config)?;
    Ok(AppState::new(pool, config, Catalog::load()?))
}

/// Brings the data directory in line before any sync runs.
fn tidy_disk(config: &Config) -> Result<(), Box<dyn Error>> {
    // A private key that a killed process left must not stay on the disk.
    let keys = git_archive::git::ssh::remove_leftovers(&config.tmp_dir())?;
    if keys > 0 {
        tracing::info!(keys, "removed SSH key files of an earlier run");
    }
    let moved = git_archive::git::ssh::split_legacy(
        &config.legacy_known_hosts(),
        &config.known_hosts_dir(),
    )?;
    if moved > 0 {
        tracing::info!(moved, "moved stored SSH host keys into one file per host");
    }
    Ok(())
}

async fn serve(state: AppState) -> Result<(), Box<dyn Error>> {
    let listener = web::bind_listener(state.config.listen_addr, DEFAULT_PORT).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    let app = web::router(state).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("stopped");
    Ok(())
}

/// Completes on Ctrl-C or SIGTERM, the signal `docker stop` sends.
async fn shutdown_signal() {
    tokio::select! {
        () = wait_ctrl_c() => {},
        () = wait_terminate() => {},
    }
    tracing::info!("shutdown requested");
}

async fn wait_ctrl_c() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        tracing::error!(%err, "cannot listen for Ctrl-C");
        std::future::pending::<()>().await;
    }
}

async fn wait_terminate() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut stream) => {
            stream.recv().await;
        }
        Err(err) => {
            tracing::error!(%err, "cannot listen for SIGTERM");
            std::future::pending::<()>().await;
        }
    }
}
