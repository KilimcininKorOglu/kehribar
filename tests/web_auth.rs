//! Behaviour of login, sessions, CSRF protection, preferences and the
//! shared page layout, exercised through the real router.

// Test helpers outside #[test] functions are test code too; a failed setup
// step must abort the test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode, header};
use common::{PASSWORD, USER, body_text, header_value, send, test_config};
use kehribar::db;
use kehribar::i18n::Catalog;
use kehribar::web::{self, AppState};

const FORM: &str = "application/x-www-form-urlencoded";

/// The router keeps no files on disk for these tests, so the data directory
/// is never created.
async fn app() -> Router {
    let config = test_config(std::path::Path::new("/nonexistent/kehribar-test"));
    let pool = db::open_in_memory().await.unwrap();
    let state = AppState::new(pool, config, Catalog::load().unwrap());
    web::router(state).layer(MockConnectInfo(SocketAddr::from(([192, 0, 2, 7], 4000))))
}

fn post_form(uri: &str) -> axum::http::request::Builder {
    Request::post(uri).header(header::CONTENT_TYPE, FORM)
}

fn login_request(username: &str, password: &str, next: &str) -> Request<Body> {
    let body = serde_urlencoded::to_string([
        ("username", username),
        ("password", password),
        ("next", next),
    ])
    .unwrap();
    post_form("/login").body(Body::from(body)).unwrap()
}

/// Signs in and returns the `name=value` pair of the session cookie.
async fn sign_in(app: &Router) -> String {
    let response = send(app, login_request(USER, PASSWORD, "/")).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let set_cookie = header_value(&response, header::SET_COOKIE);
    set_cookie.split(';').next().unwrap().to_owned()
}

/// Reads the CSRF token from the hidden logout form of a signed-in page.
async fn csrf_token(app: &Router, cookie: &str) -> String {
    let page = send(
        app,
        Request::get("/")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let html = body_text(page).await;
    let marker = "name=\"_csrf\" value=\"";
    let start = html.find(marker).expect("logout form with CSRF token") + marker.len();
    html[start..start + 64].to_owned()
}

#[tokio::test]
async fn private_page_redirects_to_login_with_return_path() {
    let app = app().await;
    let response = send(&app, Request::get("/?tab=all").body(Body::empty()).unwrap()).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        header_value(&response, header::LOCATION),
        "/login?next=/%3Ftab%3Dall"
    );
}

#[tokio::test]
async fn htmx_request_without_session_gets_hx_redirect() {
    let app = app().await;
    let response = send(
        &app,
        Request::get("/")
            .header("hx-request", "true")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(header_value(&response, "hx-redirect"), "/login?next=/");
}

#[tokio::test]
async fn wrong_password_is_rejected_and_sets_no_cookie() {
    let app = app().await;
    let response = send(&app, login_request(USER, "wrong", "/")).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().get(header::SET_COOKIE).is_none());
    let html = body_text(response).await;
    assert!(html.contains("Kullanıcı adı veya parola yanlış."));
    // The username stays in the form, the password does not.
    assert!(html.contains("value=\"admin\""));
    assert!(!html.contains("wrong"));
}

#[tokio::test]
async fn login_sets_a_strict_http_only_cookie_and_opens_private_pages() {
    let app = app().await;
    let response = send(&app, login_request(USER, PASSWORD, "/")).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let set_cookie = header_value(&response, header::SET_COOKIE);
    assert!(set_cookie.starts_with("kh_session="));
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Strict"));

    let cookie = set_cookie.split(';').next().unwrap();
    let page = send(
        &app,
        Request::get("/")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(page.status(), StatusCode::OK);
    let html = body_text(page).await;
    assert!(html.contains("Repolar"));
    assert!(html.contains("hx-headers"));
}

#[tokio::test]
async fn login_never_redirects_to_another_site() {
    let app = app().await;
    let response = send(&app, login_request(USER, PASSWORD, "//evil.example/x")).await;
    assert_eq!(header_value(&response, header::LOCATION), "/");
}

#[tokio::test]
async fn post_without_csrf_token_is_forbidden() {
    let app = app().await;
    let cookie = sign_in(&app).await;
    let response = send(
        &app,
        post_form("/logout")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let wrong = send(
        &app,
        post_form("/logout")
            .header(header::COOKIE, &cookie)
            .body(Body::from(format!("_csrf={}", "0".repeat(64))))
            .unwrap(),
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn logout_with_csrf_token_ends_the_session() {
    let app = app().await;
    let cookie = sign_in(&app).await;
    let token = csrf_token(&app, &cookie).await;

    let response = send(
        &app,
        post_form("/logout")
            .header(header::COOKIE, &cookie)
            .body(Body::from(format!("_csrf={token}")))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(header_value(&response, header::LOCATION), "/login");

    let after = send(
        &app,
        Request::get("/")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(after.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn csrf_header_is_accepted_for_htmx_requests() {
    let app = app().await;
    let cookie = sign_in(&app).await;
    let token = csrf_token(&app, &cookie).await;
    let response = send(
        &app,
        Request::post("/logout")
            .header(header::COOKIE, &cookie)
            .header("x-csrf-token", &token)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn cross_site_post_is_refused_even_with_valid_credentials() {
    let app = app().await;
    let body = format!("username={USER}&password=correct+horse");
    let response = send(
        &app,
        post_form("/login")
            .header(header::HOST, "archive.local")
            .header(header::ORIGIN, "https://evil.example")
            .body(Body::from(body.clone()))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn repeated_failures_lock_the_login() {
    let app = app().await;
    for _ in 0..kehribar::web::auth::MAX_FAILED_LOGINS {
        let response = send(&app, login_request(USER, "wrong", "/")).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    // Even the right password is refused while the address is locked.
    let response = send(&app, login_request(USER, PASSWORD, "/")).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().get(header::SET_COOKIE).is_none());
}

fn login_from(client: &str, password: &str) -> Request<Body> {
    let mut request = login_request(USER, password, "/");
    request
        .headers_mut()
        .insert("x-forwarded-for", client.parse().unwrap());
    request
}

#[tokio::test]
async fn behind_a_trusted_proxy_failures_lock_only_their_client() {
    let mut config = test_config(std::path::Path::new("/nonexistent/kehribar-test"));
    // The test connection comes from 192.0.2.7, the proxy.
    config.trusted_proxies = kehribar::proxy::parse_list("192.0.2.0/24").unwrap();
    let pool = db::open_in_memory().await.unwrap();
    let state = AppState::new(pool, config, Catalog::load().unwrap());
    let app = web::router(state).layer(MockConnectInfo(SocketAddr::from(([192, 0, 2, 7], 4000))));

    for _ in 0..kehribar::web::auth::MAX_FAILED_LOGINS {
        let response = send(&app, login_from("198.51.100.1", "wrong")).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let locked = send(&app, login_from("198.51.100.1", PASSWORD)).await;
    assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);
    // Another client behind the same proxy still signs in.
    let other = send(&app, login_from("198.51.100.2", PASSWORD)).await;
    assert_eq!(other.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn without_a_trusted_proxy_the_forwarded_header_changes_nothing() {
    let app = app().await;
    for _ in 0..kehribar::web::auth::MAX_FAILED_LOGINS {
        send(&app, login_from("198.51.100.1", "wrong")).await;
    }
    // A client cannot escape the lock by naming another address.
    let response = send(&app, login_from("198.51.100.2", PASSWORD)).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn preferences_are_saved_in_cookies_and_applied() {
    let app = app().await;
    let response = send(
        &app,
        post_form("/prefs")
            .body(Body::from("theme=dark&lang=en&back=/login"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(header_value(&response, header::LOCATION), "/login");
    let cookies: Vec<String> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_owned())
        .collect();
    assert!(cookies.contains(&"kh_theme=dark".to_owned()));
    assert!(cookies.contains(&"kh_lang=en".to_owned()));

    let page = send(
        &app,
        Request::get("/login")
            .header(header::COOKIE, cookies.join("; "))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let html = body_text(page).await;
    assert!(html.contains("<html lang=\"en\" data-theme=\"dark\">"));
    assert!(html.contains("Sign in to the archive"));
}

#[tokio::test]
async fn htmx_preference_change_asks_for_a_refresh() {
    let app = app().await;
    let response = send(
        &app,
        post_form("/prefs")
            .header("hx-request", "true")
            .body(Body::from("theme=light"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, "hx-refresh"), "true");
}

#[tokio::test]
async fn pages_carry_security_headers_and_assets_are_served() {
    let app = app().await;
    let page = send(&app, Request::get("/login").body(Body::empty()).unwrap()).await;
    let csp = header_value(&page, header::CONTENT_SECURITY_POLICY);
    assert!(csp.contains("script-src 'self'"));
    assert_eq!(
        header_value(&page, header::X_CONTENT_TYPE_OPTIONS),
        "nosniff"
    );
    assert_eq!(header_value(&page, header::X_FRAME_OPTIONS), "DENY");

    let css = send(
        &app,
        Request::get("/static/app.css").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(css.status(), StatusCode::OK);
    assert!(header_value(&css, header::CONTENT_TYPE).starts_with("text/css"));
    let missing = send(
        &app,
        Request::get("/static/missing.js")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    // Only the embedded assets exist; a traversal path never reaches a file.
    for path in ["/static/../Cargo.toml", "/static/..%2FCargo.toml"] {
        let response = send(&app, Request::get(path).body(Body::empty()).unwrap()).await;
        assert_ne!(response.status(), StatusCode::OK, "{path}");
        assert!(!body_text(response).await.contains("[package]"), "{path}");
    }

    let health = send(&app, Request::get("/healthz").body(Body::empty()).unwrap()).await;
    assert_eq!(health.status(), StatusCode::OK);
}
