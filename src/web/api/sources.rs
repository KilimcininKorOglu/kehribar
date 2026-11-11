//! Source endpoints of the API.

use axum::Json;
use axum::extract::State;

use super::views::SourceView;
use super::{ApiResult, Id, found};
use crate::db::sources;
use crate::web::AppState;

/// `GET /sources`.
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<SourceView>>> {
    let rows = sources::list(&state.pool).await?;
    Ok(Json(rows.into_iter().map(SourceView::from).collect()))
}

/// `GET /sources/{id}`.
pub async fn show(State(state): State<AppState>, Id(id): Id) -> ApiResult<Json<SourceView>> {
    let source = found(sources::get(&state.pool, id).await?)?;
    Ok(Json(source.into()))
}
