//! HTTP routing and shared request state.

pub mod auth;
pub mod pages;
pub mod prefs;
pub mod security;

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post};
use sqlx::SqlitePool;
use tokio::net::TcpListener;

use crate::config::Config;
use crate::crypto::Cipher;
use crate::i18n::Catalog;

/// State shared by every request handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    pub cipher: Cipher,
    pub catalog: Arc<Catalog>,
    pub limiter: Arc<auth::LoginLimiter>,
}

impl AppState {
    pub fn new(pool: SqlitePool, config: Config, catalog: Catalog) -> Self {
        Self {
            pool,
            cipher: Cipher::new(&config.secret_key),
            config: Arc::new(config),
            catalog: Arc::new(catalog),
            limiter: Arc::new(auth::LoginLimiter::default()),
        }
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
        .route("/", get(pages::home))
        .route("/logout", post(auth::logout))
        .layer(from_fn(security::verify_csrf))
        .layer(from_fn_with_state(state.clone(), auth::require_session));

    Router::new()
        .route("/healthz", get(healthz))
        .route("/login", get(auth::login_form).post(auth::login))
        .route("/prefs", post(prefs::update))
        .route("/static/{file}", get(pages::static_file))
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
