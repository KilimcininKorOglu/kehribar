//! Repository endpoints of the API.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use super::views::{RepoView, RunView};
use super::{Accepted, ApiError, ApiResult, Id, JsonBody, accepted, found, present};
use crate::db::repos::{self, ListFilter, RepoSort, RepoStatus};
use crate::db::sync_runs::{self, KEEP_PER_REPO};
use crate::web::AppState;
use crate::web::form::{encrypt_token, optional_text};
use crate::web::repos::{
    AddSettings, Added, SettingsInput, add_repo, check_settings, queue_sync, remove_repo,
    save_credentials,
};

/// Repositories per page when the request names no `per_page`.
const DEFAULT_PER_PAGE: i64 = 50;
const MAX_PER_PAGE: i64 = 100;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    q: Option<String>,
    status: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
    page: Option<String>,
    per_page: Option<String>,
}

/// A query value the list cannot use.
fn bad_query(message: &str) -> ApiError {
    ApiError::invalid("invalid_query", message)
}

/// A text code of `parse`, or the default when the query has none.
fn parsed<T: Default>(
    value: Option<&str>,
    parse: fn(&str) -> Option<T>,
    message: &str,
) -> ApiResult<T> {
    match value {
        None => Ok(T::default()),
        Some(text) => parse(text).ok_or_else(|| bad_query(message)),
    }
}

fn descending(dir: Option<&str>) -> ApiResult<bool> {
    match dir {
        None | Some("asc") => Ok(false),
        Some("desc") => Ok(true),
        Some(_) => Err(bad_query("dir is asc or desc.")),
    }
}

/// A whole number of the query in `range`, or `default` when it is absent.
fn number(
    value: Option<&str>,
    default: i64,
    range: std::ops::RangeInclusive<i64>,
    message: &str,
) -> ApiResult<i64> {
    let Some(text) = value else {
        return Ok(default);
    };
    text.parse()
        .ok()
        .filter(|n| range.contains(n))
        .ok_or_else(|| bad_query(message))
}

/// The page number and the page size of the query.
fn paging(query: &ListQuery) -> ApiResult<(i64, i64)> {
    let page = number(query.page.as_deref(), 1, 1..=i64::MAX, "page starts at 1.")?;
    let per_page = number(
        query.per_page.as_deref(),
        DEFAULT_PER_PAGE,
        1..=MAX_PER_PAGE,
        "per_page is between 1 and 100.",
    )?;
    Ok((page, per_page))
}

#[derive(Debug, Serialize)]
pub struct RepoPage {
    repos: Vec<RepoView>,
    /// The number of repositories that match the filter, on all pages.
    total: i64,
    page: i64,
    per_page: i64,
}

/// `GET /repos`: one page of the repositories that match the filter.
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<RepoPage>> {
    let status = query
        .status
        .as_deref()
        .map(|code| RepoStatus::parse(code).ok_or_else(|| bad_query("Unknown status.")))
        .transpose()?;
    let sort = parsed(query.sort.as_deref(), RepoSort::parse, "Unknown sort.")?;
    let (page, per_page) = paging(&query)?;
    let search = query.q.as_deref().unwrap_or("");
    let counts = repos::status_counts(&state.pool, search).await?;
    let total = counts
        .iter()
        .filter(|(s, _)| status.is_none_or(|wanted| *s == wanted))
        .map(|(_, n)| n)
        .sum();
    let filter = ListFilter {
        search,
        status,
        sort,
        descending: descending(query.dir.as_deref())?,
        limit: per_page,
        offset: (page - 1).saturating_mul(per_page),
    };
    let rows = repos::page(&state.pool, &filter).await?;
    Ok(Json(RepoPage {
        repos: rows.into_iter().map(RepoView::from).collect(),
        total,
        page,
        per_page,
    }))
}

/// The repository `id` as it is stored now.
async fn view(state: &AppState, id: i64) -> ApiResult<Json<RepoView>> {
    let repo = found(repos::get(&state.pool, id).await?)?;
    Ok(Json(repo.into()))
}

/// `GET /repos/{id}`.
pub async fn show(State(state): State<AppState>, Id(id): Id) -> ApiResult<Json<RepoView>> {
    view(&state, id).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewRepoBody {
    url: String,
    #[serde(default)]
    interval_minutes: Option<i64>,
    #[serde(default = "super::yes")]
    wiki_enabled: bool,
    #[serde(default)]
    lfs_enabled: bool,
    #[serde(default)]
    ref_exclude: Vec<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

/// `POST /repos`: adds a repository, due for an immediate sync.
pub async fn create(
    State(state): State<AppState>,
    JsonBody(body): JsonBody<NewRepoBody>,
) -> ApiResult<impl IntoResponse> {
    let patterns = body.ref_exclude.join("\n");
    let settings = check_settings(&SettingsInput {
        interval_minutes: body.interval_minutes,
        wiki_enabled: body.wiki_enabled,
        lfs_enabled: body.lfs_enabled,
        ref_exclude: &patterns,
    })
    .map_err(|key| ApiError::refused(&state, StatusCode::UNPROCESSABLE_ENTITY, key))?;
    let token_enc = encrypt_token(&state.cipher, body.token.as_deref())?;
    let add = AddSettings {
        interval: body.interval_minutes,
        wiki: body.wiki_enabled,
        username: optional_text(body.username.as_deref()),
        token_enc: token_enc.as_deref(),
        checked: Some(&settings),
    };
    let id = match add_repo(&state, &body.url, &add).await? {
        Added::Yes(id) => id,
        Added::Exists => {
            return Err(ApiError::refused(
                &state,
                StatusCode::CONFLICT,
                "repo_new.exists",
            ));
        }
        Added::Invalid(key) => {
            return Err(ApiError::refused(
                &state,
                StatusCode::UNPROCESSABLE_ENTITY,
                key,
            ));
        }
    };
    state.scheduler.wake();
    let location = [(header::LOCATION, format!("/api/v1/repos/{id}"))];
    Ok((StatusCode::CREATED, location, view(&state, id).await?))
}

/// A change of some settings; a field that is absent keeps its value.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    /// `null` returns to the default interval.
    #[serde(default, deserialize_with = "present")]
    interval_minutes: Option<Option<i64>>,
    wiki_enabled: Option<bool>,
    lfs_enabled: Option<bool>,
    ref_exclude: Option<Vec<String>>,
}

/// `PATCH /repos/{id}`: changes the sync settings.
pub async fn update(
    State(state): State<AppState>,
    Id(id): Id,
    JsonBody(patch): JsonBody<SettingsPatch>,
) -> ApiResult<Json<RepoView>> {
    let repo = found(repos::get(&state.pool, id).await?)?;
    let patterns = match patch.ref_exclude {
        Some(lines) => lines.join("\n"),
        None => repo.ref_exclude.unwrap_or_default(),
    };
    let settings = check_settings(&SettingsInput {
        interval_minutes: patch.interval_minutes.unwrap_or(repo.interval_minutes),
        wiki_enabled: patch.wiki_enabled.unwrap_or(repo.wiki_enabled),
        lfs_enabled: patch.lfs_enabled.unwrap_or(repo.lfs_enabled),
        ref_exclude: &patterns,
    })
    .map_err(|key| ApiError::refused(&state, StatusCode::UNPROCESSABLE_ENTITY, key))?;
    repos::update_settings(&state.pool, id, settings.as_settings()).await?;
    view(&state, id).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialsBody {
    /// Absent keeps the stored username; `null` removes it.
    #[serde(default, deserialize_with = "present")]
    username: Option<Option<String>>,
    /// Absent keeps the stored token; `null` or blank removes it.
    #[serde(default, deserialize_with = "present")]
    token: Option<Option<String>>,
}

/// `PUT /repos/{id}/credentials`: the username and the token of the source.
/// The answer tells only whether a token is stored.
pub async fn credentials(
    State(state): State<AppState>,
    Id(id): Id,
    JsonBody(body): JsonBody<CredentialsBody>,
) -> ApiResult<Json<RepoView>> {
    let repo = found(repos::get(&state.pool, id).await?)?;
    let username = body.username.unwrap_or(repo.username);
    let clear = matches!(&body.token, Some(token) if optional_text(token.as_deref()).is_none());
    let token = body.token.flatten();
    save_credentials(&state, id, username.as_deref(), token.as_deref(), clear).await?;
    view(&state, id).await
}

/// `DELETE /repos/{id}`: removes the repository with its archive. A
/// repository that is syncing now is refused.
pub async fn delete(State(state): State<AppState>, Id(id): Id) -> ApiResult<StatusCode> {
    let repo = found(repos::get(&state.pool, id).await?)?;
    if repo.status == RepoStatus::Syncing || !remove_repo(&state, &repo).await? {
        return Err(ApiError::refused(
            &state,
            StatusCode::CONFLICT,
            "repo.delete_busy",
        ));
    }
    tracing::info!(url = %repo.url, "repository deleted with its archive through the API");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /repos/{id}/sync`: syncs now, or once more after the running sync.
pub async fn sync(State(state): State<AppState>, Id(id): Id) -> ApiResult<Accepted> {
    accepted(queue_sync(&state, id).await?, "queued")
}

#[derive(Debug, Deserialize)]
pub struct RunsQuery {
    limit: Option<String>,
}

/// `GET /repos/{id}/runs`: the sync history, newest first.
pub async fn runs(
    State(state): State<AppState>,
    Id(id): Id,
    Query(query): Query<RunsQuery>,
) -> ApiResult<Json<Vec<RunView>>> {
    let limit = number(
        query.limit.as_deref(),
        DEFAULT_PER_PAGE,
        1..=KEEP_PER_REPO,
        "limit is between 1 and 200.",
    )?;
    found(repos::get(&state.pool, id).await?)?;
    let runs = sync_runs::list_for_repo(&state.pool, id, limit).await?;
    Ok(Json(runs.into_iter().map(RunView::from).collect()))
}
