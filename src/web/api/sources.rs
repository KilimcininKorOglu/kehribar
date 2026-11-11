//! Source endpoints of the API.

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde::Deserialize;

use super::views::SourceView;
use super::{Accepted, ApiError, ApiResult, Id, JsonBody, accepted, found};
use crate::db::sources::{self, SourceOptions};
use crate::web::AppState;
use crate::web::sources::{OptionsInput, add_source, check_options, start_scan};

/// The source `id` as it is stored now.
async fn view(state: &AppState, id: i64) -> ApiResult<Json<SourceView>> {
    let source = found(sources::get(&state.pool, id).await?)?;
    Ok(Json(source.into()))
}

/// `GET /sources`.
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<SourceView>>> {
    let rows = sources::list(&state.pool).await?;
    Ok(Json(rows.into_iter().map(SourceView::from).collect()))
}

/// `GET /sources/{id}`.
pub async fn show(State(state): State<AppState>, Id(id): Id) -> ApiResult<Json<SourceView>> {
    view(&state, id).await
}

/// Checks the options of a request; patterns arrive one per array item.
fn checked_options(state: &AppState, input: &OptionsInput<'_>) -> ApiResult<SourceOptions> {
    check_options(input).map_err(|key| ApiError::rejected(state, key))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSourceBody {
    host_id: i64,
    kind: String,
    name: String,
    #[serde(default = "super::yes")]
    auto_add: bool,
    #[serde(default)]
    include_forks: bool,
    #[serde(default)]
    skip_archived: bool,
    #[serde(default)]
    use_ssh: bool,
    #[serde(default)]
    include_patterns: Vec<String>,
    #[serde(default)]
    exclude_patterns: Vec<String>,
}

/// `POST /sources`: adds a source and starts its first scan.
pub async fn create(
    State(state): State<AppState>,
    JsonBody(body): JsonBody<NewSourceBody>,
) -> ApiResult<impl IntoResponse> {
    let (include, exclude) = (
        body.include_patterns.join("\n"),
        body.exclude_patterns.join("\n"),
    );
    let options = checked_options(
        &state,
        &OptionsInput {
            auto_add: body.auto_add,
            include_forks: body.include_forks,
            skip_archived: body.skip_archived,
            include_patterns: &include,
            exclude_patterns: &exclude,
            use_ssh: body.use_ssh,
        },
    )?;
    let id = add_source(&state, Some(body.host_id), &body.kind, &body.name, options)
        .await?
        .map_err(|key| ApiError::rejected(&state, key))?;
    let location = [(header::LOCATION, format!("/api/v1/sources/{id}"))];
    Ok((StatusCode::CREATED, location, view(&state, id).await?))
}

/// A change of some options; a field that is absent keeps its value.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionsPatch {
    auto_add: Option<bool>,
    include_forks: Option<bool>,
    skip_archived: Option<bool>,
    use_ssh: Option<bool>,
    include_patterns: Option<Vec<String>>,
    exclude_patterns: Option<Vec<String>>,
}

/// The patterns of the patch, or else the stored ones.
fn patterns(patch: Option<Vec<String>>, stored: Option<String>) -> String {
    patch.map_or_else(|| stored.unwrap_or_default(), |lines| lines.join("\n"))
}

/// `PATCH /sources/{id}`.
pub async fn update(
    State(state): State<AppState>,
    Id(id): Id,
    JsonBody(patch): JsonBody<OptionsPatch>,
) -> ApiResult<Json<SourceView>> {
    let source = found(sources::get(&state.pool, id).await?)?;
    let include = patterns(patch.include_patterns, source.include_patterns);
    let exclude = patterns(patch.exclude_patterns, source.exclude_patterns);
    let options = checked_options(
        &state,
        &OptionsInput {
            auto_add: patch.auto_add.unwrap_or(source.auto_add),
            include_forks: patch.include_forks.unwrap_or(source.include_forks),
            skip_archived: patch.skip_archived.unwrap_or(source.skip_archived),
            include_patterns: &include,
            exclude_patterns: &exclude,
            use_ssh: patch.use_ssh.unwrap_or(source.use_ssh),
        },
    )?;
    sources::update_options(&state.pool, id, &options).await?;
    view(&state, id).await
}

/// `DELETE /sources/{id}`: the repositories of the source stay.
pub async fn delete(State(state): State<AppState>, Id(id): Id) -> ApiResult<StatusCode> {
    if !sources::delete(&state.pool, id).await? {
        return Err(ApiError::not_found());
    }
    tracing::info!(id, "source deleted through the API, its repositories stay");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /sources/{id}/scan`: scans in the background.
pub async fn scan(State(state): State<AppState>, Id(id): Id) -> ApiResult<Accepted> {
    accepted(start_scan(&state, id).await?, "scanning")
}
