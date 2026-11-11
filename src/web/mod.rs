//! HTTP routing and shared request state.

pub mod auth;
pub mod browse;
pub mod form;
pub mod format;
pub mod git_http;
pub mod highlight;
pub mod host_ssh;
pub mod hosts;
pub mod markdown;
pub mod pages;
pub mod prefs;
pub mod repos;
pub mod security;
pub mod settings_backup;
pub mod settings_notify;
pub mod settings_page;
pub mod sources;

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{any, get, post};
use sqlx::SqlitePool;
use tokio::net::TcpListener;

use crate::config::Config;
use crate::crypto::Cipher;
use crate::i18n::Catalog;
use crate::sync::SyncContext;
use crate::sync::locks::RepoLocks;
use crate::sync::scheduler::Scheduler;

/// State shared by every request handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    pub cipher: Cipher,
    pub catalog: Arc<Catalog>,
    pub limiter: Arc<auth::LoginLimiter>,
    /// The caller decides whether its loop runs; handlers only wake it.
    pub scheduler: Scheduler,
}

impl AppState {
    pub fn new(pool: SqlitePool, config: Config, catalog: Catalog) -> Self {
        let config = Arc::new(config);
        let cipher = Cipher::new(&config.secret_key);
        let catalog = Arc::new(catalog);
        let scheduler = Scheduler::new(SyncContext {
            pool: pool.clone(),
            cipher: cipher.clone(),
            config: config.clone(),
            catalog: catalog.clone(),
            locks: RepoLocks::default(),
        });
        Self {
            pool,
            cipher,
            config,
            catalog,
            limiter: Arc::new(auth::LoginLimiter::default()),
            scheduler,
        }
    }

    /// The context background work uses, for work a request starts itself.
    /// It shares the repository locks of the scheduler.
    pub fn sync_context(&self) -> SyncContext {
        self.scheduler.context().clone()
    }
}

/// Binds the HTTP listener. An explicit address is used as given. Without
/// one, the listener takes `[::]:port`, which accepts IPv6 and IPv4 clients,
/// and takes `0.0.0.0:port` only when the host has no IPv6 support.
pub async fn bind_listener(addr: Option<SocketAddr>, default_port: u16) -> io::Result<TcpListener> {
    if let Some(addr) = addr {
        return TcpListener::bind(addr).await;
    }
    match TcpListener::bind((Ipv6Addr::UNSPECIFIED, default_port)).await {
        Ok(listener) => Ok(listener),
        // A port in use is a real error; the IPv4 socket would fail the same way.
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => Err(err),
        Err(err) => {
            tracing::info!(%err, "IPv6 is unavailable, listening on IPv4 only");
            TcpListener::bind((Ipv4Addr::UNSPECIFIED, default_port)).await
        }
    }
}

/// Builds the application router.
pub fn router(state: AppState) -> Router {
    // Layers run from the last added to the first: the session check runs
    // before the CSRF check, which needs the session.
    let private = Router::new()
        .route("/", get(repos::list))
        .route("/repos/new", get(repos::new_form))
        .route("/repos", post(repos::create))
        .route("/repos/bulk", post(repos::create_bulk))
        .route("/repos/{id}", get(repos::show))
        .route("/repos/{id}/sync", post(repos::sync_now))
        .route("/repos/{id}/settings", post(repos::update))
        .route("/repos/{id}/credentials", post(repos::update_credentials))
        .route("/repos/{id}/tree", get(browse::tree))
        .route("/repos/{id}/blob", get(browse::blob))
        .route("/repos/{id}/raw", get(browse::raw))
        .route("/repos/{id}/blame", get(browse::blame))
        .route("/repos/{id}/commits", get(browse::commits))
        .route("/repos/{id}/commit/{sha}", get(browse::commit))
        .route("/repos/{id}/refs", get(browse::refs))
        .route("/repos/{id}/archive", get(browse::archive))
        .route("/repos/{id}/delete", post(repos::delete))
        .route("/hosts", get(hosts::list).post(hosts::create))
        .route("/hosts/{id}", get(hosts::show).post(hosts::update))
        .route("/hosts/{id}/delete", post(hosts::delete))
        .route("/hosts/{id}/ssh/generate", post(host_ssh::generate))
        .route("/hosts/{id}/ssh/key", post(host_ssh::paste))
        .route("/hosts/{id}/ssh/delete", post(host_ssh::remove))
        .route(
            "/hosts/{id}/ssh/forget-host-keys",
            post(host_ssh::forget_host_keys),
        )
        .route("/sources", get(sources::list).post(sources::create))
        .route("/sources/{id}", post(sources::update))
        .route("/sources/{id}/scan", post(sources::scan))
        .route("/sources/{id}/delete", post(sources::delete))
        .route(
            "/settings",
            get(settings_page::show).post(settings_page::save),
        )
        .route("/settings/notifications", post(settings_notify::save))
        .route("/settings/notifications/test", post(settings_notify::test))
        .route("/settings/backup", post(settings_backup::save))
        .route("/settings/backup/test", post(settings_backup::test))
        .route("/settings/backup/run", post(settings_backup::run))
        .route("/logout", post(auth::logout))
        .layer(from_fn(security::verify_csrf))
        .layer(from_fn_with_state(state.clone(), auth::require_session));

    Router::new()
        .route("/healthz", get(healthz))
        .route("/login", get(auth::login_form).post(auth::login))
        .route("/prefs", post(prefs::update))
        .route("/static/{file}", get(pages::static_file))
        // Git clients authenticate with HTTP Basic, not with the session.
        .route("/git/{*path}", any(git_http::serve))
        .merge(private)
        .layer(from_fn(security::same_origin))
        .layer(from_fn(security::headers))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_listener_accepts_ipv4_and_ipv6_clients() {
        let listener = bind_listener(None, 0).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = tokio::spawn(async move {
            for _ in 0..2 {
                listener.accept().await.unwrap();
            }
        });
        tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        tokio::net::TcpStream::connect((Ipv6Addr::LOCALHOST, port))
            .await
            .unwrap();
        accept.await.unwrap();
    }

    #[tokio::test]
    async fn explicit_address_is_used_as_given() {
        let listener = bind_listener(Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))), 1)
            .await
            .unwrap();
        assert!(listener.local_addr().unwrap().is_ipv4());
    }
}
