//! Cross-site request checks and response security headers.

use std::convert::Infallible;

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::auth::{CurrentSession, secrets_equal};
use crate::error::AppError;

/// Largest form body the CSRF check reads.
const MAX_FORM_BYTES: usize = 1024 * 1024;

/// Largest file upload (multipart body), the size limit of an import file.
pub const MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024;

const CSRF_HEADER: &str = "x-csrf-token";
const CSRF_FIELD: &str = "_csrf";

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; \
     style-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'none'; \
     frame-ancestors 'none'; form-action 'self'";

fn is_safe(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Rejects state-changing requests that a browser sent from another site.
pub async fn same_origin(request: Request, next: Next) -> Response {
    if is_safe(request.method()) || from_same_site(request.headers()) {
        return next.run(request).await;
    }
    tracing::warn!(uri = %request.uri(), "cross-site request refused");
    AppError::Forbidden.into_response()
}

fn from_same_site(headers: &HeaderMap) -> bool {
    let text = |name| {
        headers
            .get(name)
            .and_then(|v: &HeaderValue| v.to_str().ok())
    };
    if text("sec-fetch-site").is_some_and(|site| site == "cross-site") {
        return false;
    }
    match (text(header::ORIGIN.as_str()), text(header::HOST.as_str())) {
        // Clients that send no Origin (curl, git) are not browsers.
        (None, _) => true,
        (Some(origin), Some(host)) => origin_host(origin) == Some(host),
        (Some(_), None) => false,
    }
}

/// Returns the `host[:port]` part of an Origin value.
fn origin_host(origin: &str) -> Option<&str> {
    origin.split_once("://").map(|(_, rest)| rest)
}

/// Requires the session's CSRF token on every state-changing request, in the
/// `X-CSRF-Token` header (htmx) or the `_csrf` form field (plain forms).
pub async fn verify_csrf(request: Request, next: Next) -> Result<Response, AppError> {
    if is_safe(request.method()) {
        return Ok(next.run(request).await);
    }
    let expected = request
        .extensions()
        .get::<CurrentSession>()
        .map(|session| session.csrf_token.clone())
        .ok_or(AppError::Forbidden)?;
    let header_token = request
        .headers()
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if let Some(token) = header_token {
        return check_token(&token, &expected, request, next).await;
    }
    let (request, form_token) = read_form_token(request).await?;
    check_token(&form_token.unwrap_or_default(), &expected, request, next).await
}

async fn check_token(
    token: &str,
    expected: &str,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    if token.is_empty() || !secrets_equal(token, expected) {
        tracing::warn!(uri = %request.uri(), "CSRF token missing or wrong");
        return Err(AppError::Forbidden);
    }
    Ok(next.run(request).await)
}

/// Reads `_csrf` from a URL-encoded or multipart body and puts the body back.
async fn read_form_token(request: Request) -> Result<(Request, Option<String>), AppError> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let boundary = multer::parse_boundary(&content_type).ok();
    let is_form = content_type.starts_with("application/x-www-form-urlencoded");
    if !is_form && boundary.is_none() {
        return Ok((request, None));
    }
    let limit = if is_form {
        MAX_FORM_BYTES
    } else {
        MAX_UPLOAD_BYTES
    };
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, limit)
        .await
        .map_err(|_| AppError::BadRequest(String::from("form body is too large")))?;
    let token = match boundary {
        Some(boundary) => multipart_token(bytes.clone(), boundary).await?,
        None => urlencoded_token(&bytes)?,
    };
    Ok((Request::from_parts(parts, Body::from(bytes)), token))
}

fn urlencoded_token(bytes: &[u8]) -> Result<Option<String>, AppError> {
    let fields: Vec<(String, String)> = serde_urlencoded::from_bytes(bytes)
        .map_err(|_| AppError::BadRequest(String::from("form body is malformed")))?;
    Ok(fields
        .into_iter()
        .find_map(|(name, value)| (name == CSRF_FIELD).then_some(value)))
}

async fn multipart_token(bytes: Bytes, boundary: String) -> Result<Option<String>, AppError> {
    let malformed = |_| AppError::BadRequest(String::from("form body is malformed"));
    let stream = futures_util::stream::once(async move { Ok::<_, Infallible>(bytes) });
    let mut multipart = multer::Multipart::new(stream, boundary);
    while let Some(field) = multipart.next_field().await.map_err(malformed)? {
        if field.name() == Some(CSRF_FIELD) {
            return field.text().await.map(Some).map_err(malformed);
        }
    }
    Ok(None)
}

/// Adds the security headers to every response.
pub async fn headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    let defaults = [
        (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "same-origin"),
        (header::X_FRAME_OPTIONS, "DENY"),
    ];
    for (name, value) in defaults {
        // A handler that set its own value, such as a stricter CSP, keeps it.
        headers
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn same_site_rules() {
        assert!(from_same_site(&headers(&[])));
        assert!(from_same_site(&headers(&[
            ("host", "localhost:8600"),
            ("origin", "http://localhost:8600")
        ])));
        assert!(!from_same_site(&headers(&[
            ("host", "localhost:8600"),
            ("origin", "http://evil.example")
        ])));
        assert!(!from_same_site(&headers(&[(
            "sec-fetch-site",
            "cross-site"
        )])));
        assert!(!from_same_site(&headers(&[(
            "origin",
            "http://localhost:8600"
        )])));
    }
}
