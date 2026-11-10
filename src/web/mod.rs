//! HTTP routing.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use axum::Router;
use axum::routing::get;
use tokio::net::TcpListener;

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
pub fn router() -> Router {
    Router::new().route("/healthz", get(healthz))
}

async fn healthz() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn healthz_answers_ok() {
        let response = router()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

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
