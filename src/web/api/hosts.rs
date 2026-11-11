//! Host endpoints of the API.

use axum::Json;
use axum::extract::State;

use super::views::HostView;
use super::{ApiResult, Id, found};
use crate::db::hosts;
use crate::web::AppState;

/// `GET /hosts`.
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<HostView>>> {
    let rows = hosts::list(&state.pool).await?;
    Ok(Json(rows.into_iter().map(HostView::from).collect()))
}

/// `GET /hosts/{id}`.
pub async fn show(State(state): State<AppState>, Id(id): Id) -> ApiResult<Json<HostView>> {
    let host = found(hosts::get(&state.pool, id).await?)?;
    Ok(Json(host.into()))
}
