//! Repository endpoints of the API.

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use super::views::{RepoView, RunView};
use super::{ApiError, ApiResult, Id, found};
use crate::db::repos::{self, ListFilter, RepoSort, RepoStatus};
use crate::db::sync_runs::{self, KEEP_PER_REPO};
use crate::web::AppState;

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

/// `GET /repos/{id}`.
pub async fn show(State(state): State<AppState>, Id(id): Id) -> ApiResult<Json<RepoView>> {
    let repo = found(repos::get(&state.pool, id).await?)?;
    Ok(Json(repo.into()))
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
