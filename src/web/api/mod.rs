//! The REST API under `/api/v1`: JSON in and out, authenticated with an
//! API token in the `Authorization: Bearer` header. A read-only token may
//! only `GET`; no answer ever contains a secret.

mod hosts;
mod repos;
mod sources;
mod views;

use std::net::SocketAddr;
use std::time::Instant;

use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Path, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::json;

use super::AppState;
use super::auth::hash_token;
use super::tokens::API_PREFIX;
use crate::db::tokens::{self, AccessToken, TokenKind};
use crate::db::unix_now;
use crate::error::AppError;
use crate::i18n::{Lang, Tr};

/// An error answer: `{"error": {"code": ..., "message": ...}}`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "No such record.")
    }

    /// A request whose values are wrong; `code` names the problem.
    pub fn invalid(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, code, message)
    }

    /// A request the web forms would refuse with the message `key`: the
    /// key is the code and its English text the message.
    pub fn refused(state: &AppState, status: StatusCode, key: &'static str) -> Self {
        let message = Tr::new(state.catalog.clone(), Lang::En).t(key).to_owned();
        Self::new(status, key, message)
    }

    /// A value the checks refused: `409` when the key says that the record
    /// exists already, else `422`.
    pub fn rejected(state: &AppState, key: &'static str) -> Self {
        let status = if key.ends_with(".exists") {
            StatusCode::CONFLICT
        } else {
            StatusCode::UNPROCESSABLE_ENTITY
        };
        Self::refused(state, status, key)
    }

    fn internal(err: impl std::fmt::Display) -> Self {
        // The detail can name paths or commands, so it goes to the log only.
        tracing::error!(%err, "API request failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "The request failed; the server log has the details.",
        )
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        Self::internal(err)
    }
}

impl From<AppError> for ApiError {
    fn from(err: AppError) -> Self {
        match err {
            AppError::NotFound => Self::not_found(),
            AppError::BadRequest(message) => {
                Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
            }
            AppError::Forbidden => Self::new(StatusCode::FORBIDDEN, "forbidden", "Not allowed."),
            AppError::Internal(detail) => Self::internal(detail),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(json!({ "error": { "code": self.code, "message": self.message } }));
        let mut response = (self.status, body).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// The record, or a `404` answer.
pub fn found<T>(record: Option<T>) -> ApiResult<T> {
    record.ok_or_else(ApiError::not_found)
}

/// A `202` answer for work that runs in the background.
pub type Accepted = (StatusCode, Json<serde_json::Value>);

/// `{"<flag>": true}` with `202` when the work `started`, else `404`
/// because its record does not exist.
pub fn accepted(started: bool, flag: &str) -> ApiResult<Accepted> {
    if !started {
        return Err(ApiError::not_found());
    }
    Ok((StatusCode::ACCEPTED, Json(json!({ flag: true }))))
}

/// The record id of the `{id}` path segment; any other text is a `404`
/// answer in the API's error shape.
pub struct Id(pub i64);

impl<S: Send + Sync> FromRequestParts<S> for Id {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> ApiResult<Self> {
        let Path(text) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::not_found())?;
        text.parse().map(Id).map_err(|_| ApiError::not_found())
    }
}

/// A JSON request body; a body that does not parse is answered in the
/// API's error shape with the status of the rejection.
pub struct JsonBody<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for JsonBody<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> ApiResult<Self> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(|rejection| {
                ApiError::new(rejection.status(), "invalid_body", rejection.body_text())
            })
    }
}

/// The default of a body field that is on unless the request says otherwise.
pub fn yes() -> bool {
    true
}

/// Reads a field that may be `null`: an absent field stays `None`, `null`
/// is `Some(None)`. Use it with `#[serde(default)]`.
pub fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// The API token of a `Bearer` header value, if it looks like one.
fn bearer(value: Option<&HeaderValue>) -> Option<&str> {
    let token = value?.to_str().ok()?.strip_prefix("Bearer ")?.trim();
    token.starts_with(API_PREFIX).then_some(token)
}

/// The valid API token whose hash is `hash`. It takes the hash, not the
/// request, because a request reference held across an await is not `Send`.
async fn token_of(state: &AppState, hash: Option<Vec<u8>>) -> ApiResult<Option<AccessToken>> {
    let Some(hash) = hash else {
        return Ok(None);
    };
    let now = unix_now();
    let found = tokens::find_valid(&state.pool, &hash, TokenKind::Api, now).await?;
    if let Some(token) = &found {
        tokens::touch_used(&state.pool, token.id, now).await?;
    }
    Ok(found)
}

/// Lets a request through with a valid API token, and a change only with a
/// token that may write. A wrong token counts as a failed login of the
/// caller's address.
async fn authenticate(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    let ip = peer.ip();
    let now = Instant::now();
    if state.limiter.is_locked(ip, now) {
        tracing::warn!(%ip, "API request refused, too many failures");
        let message = "Too many failed attempts from this address; try again later.";
        return ApiError::new(StatusCode::TOO_MANY_REQUESTS, "locked", message).into_response();
    }
    let hash = bearer(request.headers().get(header::AUTHORIZATION)).map(hash_token);
    let token = match token_of(&state, hash).await {
        Ok(token) => token,
        Err(err) => return err.into_response(),
    };
    let Some(token) = token else {
        if request.headers().contains_key(header::AUTHORIZATION) {
            state.limiter.record_failure(ip, now);
            tracing::warn!(%ip, "API request with a wrong token");
        }
        let message = "Send a valid API token in the Authorization: Bearer header.";
        return ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", message).into_response();
    };
    state.limiter.reset(ip);
    let reads = matches!(*request.method(), Method::GET | Method::HEAD);
    if !reads && !token.can_write {
        let message = "This token may only read.";
        return ApiError::new(StatusCode::FORBIDDEN, "read_only", message).into_response();
    }
    next.run(request).await
}

async fn unknown_route() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "no_route", "No such API endpoint.")
}

/// The routes under `/api/v1`.
pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/repos", get(repos::list).post(repos::create))
        .route(
            "/repos/{id}",
            get(repos::show).patch(repos::update).delete(repos::delete),
        )
        .route("/repos/{id}/runs", get(repos::runs))
        .route("/repos/{id}/credentials", put(repos::credentials))
        .route("/repos/{id}/sync", post(repos::sync))
        .route("/hosts", get(hosts::list).post(hosts::create))
        .route(
            "/hosts/{id}",
            get(hosts::show).patch(hosts::update).delete(hosts::delete),
        )
        .route(
            "/hosts/{id}/ssh-key",
            put(hosts::put_ssh_key).delete(hosts::delete_ssh_key),
        )
        .route("/sources", get(sources::list).post(sources::create))
        .route(
            "/sources/{id}",
            get(sources::show)
                .patch(sources::update)
                .delete(sources::delete),
        )
        .route("/sources/{id}/scan", post(sources::scan))
        .fallback(unknown_route)
        .layer(from_fn_with_state(state, authenticate))
}
