use std::error::Error;
use std::net::SocketAddr;
use std::process::ExitCode;

use git_archive::config::{Config, DEFAULT_PORT};
use git_archive::web;
use tracing_subscriber::EnvFilter;

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
    serve(config.listen_addr).await
}

fn init_tracing(level: &str) -> Result<(), Box<dyn Error>> {
    let filter = EnvFilter::try_new(level)?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init()
        .map_err(|err| -> Box<dyn Error> { err })
}

async fn serve(addr: Option<SocketAddr>) -> Result<(), Box<dyn Error>> {
    let listener = web::bind_listener(addr, DEFAULT_PORT).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, web::router())
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
