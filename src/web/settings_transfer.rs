//! The export and import part of the settings page: hosts, sources and
//! repositories as a JSON file.

use askama::Template;
use axum::extract::Multipart;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::pages::{Layout, Nav, render};
use super::repos::PageContext;
use super::settings_page::{self, PageState};
use crate::error::AppError;
use crate::transfer::{self, ImportRow, Outcome};

/// The name of the file field on the import form.
const FILE_FIELD: &str = "file";

pub async fn export(ctx: PageContext) -> Result<Response, AppError> {
    let export = transfer::export(&ctx.state.pool).await?;
    let body = serde_json::to_vec_pretty(&export)
        .map_err(|err| AppError::Internal(format!("export is not serializable: {err}")))?;
    tracing::info!(
        hosts = export.hosts.len(),
        sources = export.sources.len(),
        repos = export.repos.len(),
        "configuration exported"
    );
    let headers = [
        (header::CONTENT_TYPE, "application/json"),
        (
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"kehribar-export.json\"",
        ),
        // The file holds encrypted secrets; no cache keeps a copy.
        (header::CACHE_CONTROL, "no-store"),
    ];
    Ok((headers, body).into_response())
}

#[derive(Template)]
#[template(path = "import.html")]
struct ImportPage {
    layout: Layout,
    rows: Vec<ImportRow>,
}

impl ImportPage {
    fn count(&self, matches: fn(Outcome) -> bool) -> i64 {
        let count = self.rows.iter().filter(|row| matches(row.outcome)).count();
        i64::try_from(count).unwrap_or(i64::MAX)
    }

    fn added(&self) -> i64 {
        self.count(|o| matches!(o, Outcome::Added | Outcome::AddedWithoutSecret))
    }

    fn without_secret(&self) -> i64 {
        self.count(|o| o == Outcome::AddedWithoutSecret)
    }
}

/// The bytes of the uploaded file; `None` when the form has no file field.
async fn read_file(multipart: &mut Multipart) -> Result<Option<Vec<u8>>, AppError> {
    let bad = |err: axum::extract::multipart::MultipartError| AppError::BadRequest(err.body_text());
    while let Some(field) = multipart.next_field().await.map_err(bad)? {
        if field.name() == Some(FILE_FIELD) {
            return Ok(Some(field.bytes().await.map_err(bad)?.to_vec()));
        }
    }
    Ok(None)
}

pub async fn import(ctx: PageContext, mut multipart: Multipart) -> Result<Response, AppError> {
    let file = read_file(&mut multipart).await?.unwrap_or_default();
    let parsed = if file.is_empty() {
        Err("transfer.no_file")
    } else {
        transfer::parse(&file).map_err(|err| err.message_key())
    };
    let export = match parsed {
        Ok(export) => export,
        Err(key) => {
            let layout = settings_page::layout(&ctx);
            let state = PageState {
                errors: vec![layout.t(key).to_owned()],
                status: StatusCode::UNPROCESSABLE_ENTITY,
                ..PageState::default()
            };
            return settings_page::respond(&ctx, layout, state).await;
        }
    };
    let rows = transfer::import(&ctx.state.pool, &ctx.state.cipher, &export).await?;
    let page = ImportPage {
        layout: ctx.layout(Nav::Settings),
        rows,
    };
    tracing::info!(
        added = page.added(),
        without_secret = page.without_secret(),
        "configuration imported"
    );
    if page.added() > 0 {
        ctx.state.scheduler.wake();
    }
    Ok(render(&page)?.into_response())
}
