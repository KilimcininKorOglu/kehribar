//! Push webhooks: a secret URL per repository that starts a sync. The
//! source server calls it after a push; the payload is not read, so every
//! provider works the same way.

use std::time::Instant;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde_json::json;

use super::AppState;
use super::auth::{ClientIp, hash_token, new_token};
use super::repos::{PageContext, site_url};
use crate::db::repos;
use crate::db::unix_now;
use crate::error::AppError;

/// The readable start of a webhook token.
pub const WEBHOOK_PREFIX: &str = "khw_";

/// `<site>/hooks/<token>`; the public URL setting wins over the address the
/// browser used.
pub fn webhook_url(ctx: &PageContext, token: &str) -> String {
    let site = ctx
        .state
        .config
        .public_url
        .as_deref()
        .map_or_else(|| site_url(ctx), |url| url.trim_end_matches('/').to_owned());
    format!("{site}/hooks/{token}")
}

/// The decrypted webhook token of the repository, when it has one.
pub async fn current_token(ctx: &PageContext, id: i64) -> Result<Option<String>, AppError> {
    let Some(stored) = repos::webhook_secret(&ctx.state.pool, id).await? else {
        return Ok(None);
    };
    let token = ctx
        .state
        .cipher
        .decrypt(&stored)
        .map_err(AppError::internal)?;
    Ok(Some(token))
}

/// Creates the webhook URL of the repository, or replaces it: the old URL
/// stops working.
pub async fn renew(ctx: PageContext, Path(id): Path<i64>) -> Result<Response, AppError> {
    let token = format!("{WEBHOOK_PREFIX}{}", new_token()?);
    let encrypted = ctx
        .state
        .cipher
        .encrypt(&token)
        .map_err(AppError::internal)?;
    let hash = hash_token(&token);
    if !repos::set_webhook(&ctx.state.pool, id, Some((&hash, &encrypted))).await? {
        return Err(AppError::NotFound);
    }
    tracing::info!(repo = id, "webhook URL created");
    Ok(Redirect::to(&format!("/repos/{id}?notice=webhook_created#webhook")).into_response())
}

pub async fn remove(ctx: PageContext, Path(id): Path<i64>) -> Result<Response, AppError> {
    if !repos::set_webhook(&ctx.state.pool, id, None).await? {
        return Err(AppError::NotFound);
    }
    tracing::info!(repo = id, "webhook URL removed");
    Ok(Redirect::to(&format!("/repos/{id}?notice=webhook_removed#webhook")).into_response())
}

/// `POST /hooks/{token}`: syncs the repository of the token now, or once
/// more after the sync that runs. A wrong token counts as a failed login
/// of the caller's address.
pub async fn receive(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Path(token): Path<String>,
) -> Result<Response, AppError> {
    let now = Instant::now();
    if state.limiter.is_locked(ip, now) {
        tracing::warn!(%ip, "webhook refused, too many failures");
        return Ok(StatusCode::TOO_MANY_REQUESTS.into_response());
    }
    let found = if token.starts_with(WEBHOOK_PREFIX) {
        repos::find_by_webhook(&state.pool, &hash_token(&token)).await?
    } else {
        None
    };
    let Some(id) = found else {
        state.limiter.record_failure(ip, now);
        tracing::warn!(%ip, "webhook with an unknown token");
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    repos::schedule_now(&state.pool, id, unix_now()).await?;
    state.scheduler.wake();
    tracing::info!(repo = id, "webhook received, sync queued");
    Ok((StatusCode::ACCEPTED, Json(json!({ "queued": true }))).into_response())
}
