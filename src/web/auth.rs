//! Login, logout and the session check that guards every private page.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use askama::Template;
use axum::Form;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::AppState;
use super::pages::{Layout, render};
use super::prefs::Prefs;
use crate::db::{self, sessions};
use crate::error::AppError;

pub const SESSION_COOKIE: &str = "ga_session";

/// Failed logins allowed from one address inside [`LOCKOUT_WINDOW`].
pub const MAX_FAILED_LOGINS: u32 = 10;
pub const LOCKOUT_WINDOW: Duration = Duration::from_secs(15 * 60);

/// The signed-in session, inserted into the request by [`require_session`].
#[derive(Debug, Clone)]
pub struct CurrentSession {
    pub csrf_token: String,
}

/// Returns 32 random bytes as 64 hex characters.
pub fn new_token() -> Result<String, AppError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(AppError::internal)?;
    Ok(hex(&bytes))
}

pub fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// The credentials of an `Authorization` value that uses `scheme`. The
/// scheme name is case-insensitive (RFC 9110), so `bearer` works too.
pub fn auth_param<'a>(value: &'a HeaderValue, scheme: &str) -> Option<&'a str> {
    let (name, param) = value.to_str().ok()?.split_once(' ')?;
    name.eq_ignore_ascii_case(scheme).then(|| param.trim())
}

/// Compares two secrets in constant time. Hashing first hides their lengths.
pub fn secrets_equal(a: &str, b: &str) -> bool {
    let a = Sha256::digest(a.as_bytes());
    let b = Sha256::digest(b.as_bytes());
    bool::from(a.as_slice().ct_eq(b.as_slice()))
}

/// Returns `target` when it is a path on this site, else `/`. This stops a
/// crafted `next` value from redirecting to another site.
pub fn local_path(target: Option<&str>) -> &str {
    match target {
        Some(path)
            if path.starts_with('/')
                && !path.starts_with("//")
                && !path.starts_with("/\\")
                && !path.chars().any(char::is_control) =>
        {
            path
        }
        _ => "/",
    }
}

/// Counts failed logins per client address.
#[derive(Debug, Default)]
pub struct LoginLimiter {
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl LoginLimiter {
    pub fn is_locked(&self, ip: IpAddr, now: Instant) -> bool {
        let failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        failures.get(&ip).is_some_and(|(count, since)| {
            *count >= MAX_FAILED_LOGINS && now.duration_since(*since) < LOCKOUT_WINDOW
        })
    }

    pub fn record_failure(&self, ip: IpAddr, now: Instant) {
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        failures.retain(|_, (_, since)| now.duration_since(*since) < LOCKOUT_WINDOW);
        let entry = failures.entry(ip).or_insert((0, now));
        entry.0 += 1;
    }

    pub fn reset(&self, ip: IpAddr) {
        self.failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&ip);
    }
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage<'a> {
    layout: Layout,
    next: &'a str,
    error: Option<&'a str>,
    username: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct NextQuery {
    next: Option<String>,
}

pub async fn login_form(
    State(state): State<AppState>,
    prefs: Prefs,
    jar: CookieJar,
    Query(query): Query<NextQuery>,
) -> Result<Response, AppError> {
    let next = local_path(query.next.as_deref());
    if session_from_jar(&state, &jar).await?.is_some() {
        return Ok(Redirect::to(next).into_response());
    }
    let page = LoginPage {
        layout: Layout::public(&state, prefs, "/login"),
        next,
        error: None,
        username: "",
    };
    Ok(render(&page)?.into_response())
}

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
    next: Option<String>,
}

pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    prefs: Prefs,
    jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> Result<Response, AppError> {
    let ip = peer.ip();
    let now = Instant::now();
    let next = local_path(form.next.as_deref());
    if state.limiter.is_locked(ip, now) {
        tracing::warn!(%ip, "login refused, too many failures");
        return login_error(
            &state,
            prefs,
            next,
            &form.username,
            "login.locked",
            StatusCode::TOO_MANY_REQUESTS,
        );
    }
    let config = &state.config;
    let valid = secrets_equal(&form.username, &config.admin_username)
        & secrets_equal(&form.password, &config.admin_password);
    if !valid {
        state.limiter.record_failure(ip, now);
        tracing::warn!(%ip, "login failed");
        return login_error(
            &state,
            prefs,
            next,
            &form.username,
            "login.failed",
            StatusCode::UNAUTHORIZED,
        );
    }
    state.limiter.reset(ip);
    let jar = jar.add(start_session(&state).await?);
    tracing::info!(%ip, "login succeeded");
    Ok((jar, Redirect::to(next)).into_response())
}

fn login_error(
    state: &AppState,
    prefs: Prefs,
    next: &str,
    username: &str,
    message_key: &'static str,
    status: StatusCode,
) -> Result<Response, AppError> {
    let layout = Layout::public(state, prefs, "/login");
    let error = layout.t(message_key).to_owned();
    let page = LoginPage {
        layout,
        next,
        error: Some(&error),
        username,
    };
    Ok((status, render(&page)?).into_response())
}

/// Stores a new session and returns its cookie.
async fn start_session(state: &AppState) -> Result<Cookie<'static>, AppError> {
    let token = new_token()?;
    let csrf = new_token()?;
    let now = db::unix_now();
    let ttl_hours = i64::from(state.config.session_ttl_hours);
    sessions::purge_expired(&state.pool, now).await?;
    sessions::create(
        &state.pool,
        &hash_token(&token),
        &csrf,
        now,
        now + ttl_hours * 3600,
    )
    .await?;
    Ok(Cookie::build((SESSION_COOKIE, token))
        .path("/")
        .http_only(true)
        .secure(state.config.cookie_secure)
        .same_site(SameSite::Strict)
        .max_age(time::Duration::hours(ttl_hours))
        .build())
}

pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> Result<Response, AppError> {
    if let Some(cookie) = jar.get(SESSION_COOKIE) {
        sessions::delete(&state.pool, &hash_token(cookie.value())).await?;
    }
    let jar = jar.remove(Cookie::build(SESSION_COOKIE).path("/"));
    Ok((jar, Redirect::to("/login")).into_response())
}

async fn session_from_jar(
    state: &AppState,
    jar: &CookieJar,
) -> Result<Option<CurrentSession>, AppError> {
    let Some(cookie) = jar.get(SESSION_COOKIE) else {
        return Ok(None);
    };
    let session =
        sessions::find_valid(&state.pool, &hash_token(cookie.value()), db::unix_now()).await?;
    Ok(session.map(|s| CurrentSession {
        csrf_token: s.csrf_token,
    }))
}

/// Lets the request through only with a valid session; otherwise sends the
/// browser to the login page.
pub async fn require_session(
    State(state): State<AppState>,
    jar: CookieJar,
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    if let Some(session) = session_from_jar(&state, &jar).await? {
        request.extensions_mut().insert(session);
        return Ok(next.run(request).await);
    }
    Ok(login_redirect(
        request.method(),
        request.headers(),
        request.uri().to_string(),
    ))
}

fn login_redirect(method: &Method, headers: &HeaderMap, target: String) -> Response {
    let login = if method == Method::GET {
        format!("/login?next={}", encode_component(&target))
    } else {
        String::from("/login")
    };
    if headers.contains_key("hx-request") {
        // htmx follows this header with a full page load.
        let mut response = StatusCode::UNAUTHORIZED.into_response();
        if let Ok(value) = HeaderValue::from_str(&login) {
            response.headers_mut().insert("hx-redirect", value);
        }
        return response;
    }
    Redirect::to(&login).into_response()
}

/// Percent-encodes everything except unreserved characters and `/`.
fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_path_rejects_other_sites() {
        assert_eq!(local_path(Some("/repos/1?tab=log")), "/repos/1?tab=log");
        for bad in [
            "//evil.example",
            "/\\evil.example",
            "https://evil.example",
            "repos",
            "/a\nb",
        ] {
            assert_eq!(local_path(Some(bad)), "/", "{bad}");
        }
        assert_eq!(local_path(None), "/");
    }

    #[test]
    fn tokens_are_random_hex() {
        let a = new_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, new_token().unwrap());
        assert_eq!(hash_token(&a).len(), 32);
    }

    #[test]
    fn secrets_compare_by_value() {
        assert!(secrets_equal("admin", "admin"));
        assert!(!secrets_equal("admin", "admin "));
        assert!(!secrets_equal("", "x"));
    }

    #[test]
    fn limiter_locks_after_the_maximum_and_expires() {
        let limiter = LoginLimiter::default();
        let ip: IpAddr = [10, 0, 0, 1].into();
        let other: IpAddr = [10, 0, 0, 2].into();
        let start = Instant::now();
        for _ in 0..MAX_FAILED_LOGINS {
            assert!(!limiter.is_locked(ip, start));
            limiter.record_failure(ip, start);
        }
        assert!(limiter.is_locked(ip, start));
        assert!(!limiter.is_locked(other, start));
        assert!(!limiter.is_locked(ip, start + LOCKOUT_WINDOW));
        limiter.reset(ip);
        assert!(!limiter.is_locked(ip, start));
    }

    #[test]
    fn encode_component_escapes_query_characters() {
        assert_eq!(
            encode_component("/repos?q=a b&x=1"),
            "/repos%3Fq%3Da%20b%26x%3D1"
        );
    }
}
