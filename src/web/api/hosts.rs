//! Host endpoints of the API.

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde::Deserialize;

use super::views::HostView;
use super::{ApiError, ApiResult, Id, JsonBody, found, present};
use crate::db::hosts;
use crate::web::AppState;
use crate::web::form::optional_text;
use crate::web::host_ssh::store_pasted;
use crate::web::hosts::{HostValues, save_host, valid_host};

/// The host `id` as it is stored now.
async fn view(state: &AppState, id: i64) -> ApiResult<Json<HostView>> {
    let host = found(hosts::get(&state.pool, id).await?)?;
    Ok(Json(host.into()))
}

/// `GET /hosts`.
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<HostView>>> {
    let rows = hosts::list(&state.pool).await?;
    Ok(Json(rows.into_iter().map(HostView::from).collect()))
}

/// `GET /hosts/{id}`.
pub async fn show(State(state): State<AppState>, Id(id): Id) -> ApiResult<Json<HostView>> {
    view(&state, id).await
}

/// A host's credentials as a request sends them.
struct Credentials<'a> {
    username: Option<&'a str>,
    token: Option<&'a str>,
    /// Removes the stored token when `token` is blank.
    clear_token: bool,
}

/// Checks and stores the values of a new host, or with `id` of that host.
async fn save(
    state: &AppState,
    id: Option<i64>,
    values: &HostValues<'_>,
    credentials: &Credentials<'_>,
) -> ApiResult<i64> {
    let (username, token) = (credentials.username, credentials.token);
    let valid = valid_host(&state.cipher, values, username, token)?
        .map_err(|key| ApiError::rejected(state, key))?;
    save_host(state, id, &valid, credentials.clear_token)
        .await?
        .map_err(|key| ApiError::rejected(state, key))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewHostBody {
    host: String,
    provider: String,
    #[serde(default)]
    api_base_url: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    ref_exclude: Vec<String>,
    #[serde(default)]
    max_concurrency: Option<i64>,
}

/// `POST /hosts`.
pub async fn create(
    State(state): State<AppState>,
    JsonBody(body): JsonBody<NewHostBody>,
) -> ApiResult<impl IntoResponse> {
    let patterns = body.ref_exclude.join("\n");
    let values = HostValues {
        host: &body.host,
        provider: &body.provider,
        api_base_url: body.api_base_url.as_deref().unwrap_or(""),
        ref_exclude: &patterns,
        max_concurrency: body.max_concurrency,
    };
    let credentials = Credentials {
        username: body.username.as_deref(),
        token: body.token.as_deref(),
        clear_token: false,
    };
    let id = save(&state, None, &values, &credentials).await?;
    let location = [(header::LOCATION, format!("/api/v1/hosts/{id}"))];
    Ok((StatusCode::CREATED, location, view(&state, id).await?))
}

/// A change of some host values; a field that is absent keeps its value.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostPatch {
    host: Option<String>,
    provider: Option<String>,
    #[serde(default, deserialize_with = "present")]
    api_base_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    username: Option<Option<String>>,
    /// `null` or blank removes the stored token.
    #[serde(default, deserialize_with = "present")]
    token: Option<Option<String>>,
    ref_exclude: Option<Vec<String>>,
    #[serde(default, deserialize_with = "present")]
    max_concurrency: Option<Option<i64>>,
}

/// `PATCH /hosts/{id}`.
pub async fn update(
    State(state): State<AppState>,
    Id(id): Id,
    JsonBody(patch): JsonBody<HostPatch>,
) -> ApiResult<Json<HostView>> {
    let host = found(hosts::get(&state.pool, id).await?)?;
    let name = patch.host.unwrap_or(host.host);
    let provider = patch
        .provider
        .unwrap_or_else(|| host.provider.code().to_owned());
    let api_base_url = patch.api_base_url.unwrap_or(host.api_base_url);
    let patterns = match patch.ref_exclude {
        Some(lines) => lines.join("\n"),
        None => host.ref_exclude.unwrap_or_default(),
    };
    let values = HostValues {
        host: &name,
        provider: &provider,
        api_base_url: api_base_url.as_deref().unwrap_or(""),
        ref_exclude: &patterns,
        max_concurrency: patch.max_concurrency.unwrap_or(host.max_concurrency),
    };
    let username = patch.username.unwrap_or(host.username);
    let clear_token = matches!(&patch.token, Some(t) if optional_text(t.as_deref()).is_none());
    let token = patch.token.flatten();
    let credentials = Credentials {
        username: username.as_deref(),
        token: token.as_deref(),
        clear_token,
    };
    save(&state, Some(id), &values, &credentials).await?;
    view(&state, id).await
}

/// `DELETE /hosts/{id}`: removes the host and its sources; the
/// repositories stay in the archive.
pub async fn delete(State(state): State<AppState>, Id(id): Id) -> ApiResult<StatusCode> {
    let host = found(hosts::get(&state.pool, id).await?)?;
    hosts::delete(&state.pool, id).await?;
    tracing::info!(host = %host.host, "host deleted through the API");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyBody {
    private_key: String,
}

/// `PUT /hosts/{id}/ssh-key`: stores a private key. The answer carries
/// only its public line.
pub async fn put_ssh_key(
    State(state): State<AppState>,
    Id(id): Id,
    JsonBody(body): JsonBody<KeyBody>,
) -> ApiResult<Json<HostView>> {
    let host = found(hosts::get(&state.pool, id).await?)?;
    store_pasted(&state, &host, &body.private_key)
        .await?
        .map_err(|key| ApiError::rejected(&state, key))?;
    view(&state, id).await
}

/// `DELETE /hosts/{id}/ssh-key`.
pub async fn delete_ssh_key(State(state): State<AppState>, Id(id): Id) -> ApiResult<StatusCode> {
    if !hosts::set_ssh_key(&state.pool, id, None).await? {
        return Err(ApiError::not_found());
    }
    Ok(StatusCode::NO_CONTENT)
}
