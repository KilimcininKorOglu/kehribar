//! Watched users and organizations: list, add with an immediate scan,
//! change options, scan now and delete. Deleting a source keeps its
//! repositories in the archive.

use std::collections::HashMap;

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::form::is_checked;
use super::pages::{Layout, Nav, render};
use super::repos::{NoticeQuery, PageContext};
use crate::db::hosts::{self, Host, Provider};
use crate::db::sources::{self, NewSource, Source, SourceKind, SourceOptions};
use crate::db::unix_now;
use crate::error::AppError;
use crate::name_filter;
use crate::providers;
use crate::repo_url;

/// One row of the source table.
pub struct Row {
    pub source: Source,
    pub host: Option<Host>,
    pub repos: i64,
}

/// The values of the add form, shown again after an error.
#[derive(Debug, Clone)]
pub struct SourceFields {
    pub host_id: i64,
    pub kind: SourceKind,
    pub name: String,
    pub auto_add: bool,
    pub include_forks: bool,
    pub skip_archived: bool,
    pub include_patterns: String,
    pub exclude_patterns: String,
}

impl SourceFields {
    fn blank() -> Self {
        Self {
            host_id: 0,
            kind: SourceKind::Org,
            name: String::new(),
            auto_add: true,
            include_forks: false,
            skip_archived: false,
            include_patterns: String::new(),
            exclude_patterns: String::new(),
        }
    }

    fn from_form(form: &SourceForm) -> Self {
        Self {
            host_id: form.host_id.parse().unwrap_or_default(),
            kind: SourceKind::parse(&form.kind).unwrap_or(SourceKind::Org),
            name: form.name.clone(),
            auto_add: is_checked(form.options.auto_add.as_deref()),
            include_forks: is_checked(form.options.include_forks.as_deref()),
            skip_archived: is_checked(form.options.skip_archived.as_deref()),
            include_patterns: form.options.include_patterns.clone().unwrap_or_default(),
            exclude_patterns: form.options.exclude_patterns.clone().unwrap_or_default(),
        }
    }

    /// Whether the filter part of the form has a value, so it opens.
    pub fn has_filter(&self) -> bool {
        self.skip_archived
            || !self.include_patterns.trim().is_empty()
            || !self.exclude_patterns.trim().is_empty()
    }

    pub fn kinds(&self) -> [SourceKind; SourceKind::ALL.len()] {
        SourceKind::ALL
    }

    pub fn is_kind(&self, kind: &SourceKind) -> bool {
        self.kind == *kind
    }

    pub fn is_host(&self, id: &i64) -> bool {
        self.host_id == *id
    }
}

#[derive(Template)]
#[template(path = "sources.html")]
struct ListPage {
    layout: Layout,
    rows: Vec<Row>,
    /// Hosts whose provider can list repositories.
    hosts: Vec<Host>,
    fields: SourceFields,
    notice: Option<String>,
    error: Option<String>,
}

impl ListPage {
    pub fn kind_label(&self, kind: &SourceKind) -> &str {
        self.layout.t(match kind {
            SourceKind::User => "sources.kind_user",
            SourceKind::Org => "sources.kind_org",
            SourceKind::Starred => "sources.kind_starred",
        })
    }

    /// A source added a moment ago is scanned in the background; the page
    /// refreshes until every source has a first result.
    pub fn scanning(&self) -> bool {
        self.rows
            .iter()
            .any(|row| row.source.last_scan_at.is_none())
    }
}

#[derive(Debug, Deserialize)]
pub struct SourceForm {
    host_id: String,
    kind: String,
    name: String,
    #[serde(flatten)]
    options: OptionsForm,
}

/// Maps the `notice` query value to a message key, ignoring anything else.
fn notice_key(notice: Option<&str>) -> Option<&'static str> {
    match notice? {
        "added" => Some("sources.added"),
        "saved" => Some("sources.saved"),
        "scanning" => Some("sources.scanning"),
        "deleted" => Some("sources.deleted"),
        _ => None,
    }
}

async fn list_page(
    ctx: &PageContext,
    fields: SourceFields,
    notice: Option<&str>,
    error: Option<&str>,
) -> Result<Response, AppError> {
    let pool = &ctx.state.pool;
    let all_hosts = hosts::list(pool).await?;
    let counts: HashMap<i64, i64> = sources::repo_counts(pool).await?.into_iter().collect();
    let rows = sources::list(pool)
        .await?
        .into_iter()
        .map(|source| Row {
            host: all_hosts.iter().find(|h| h.id == source.host_id).cloned(),
            repos: counts.get(&source.id).copied().unwrap_or_default(),
            source,
        })
        .collect();
    let layout = ctx.layout(Nav::Sources);
    let page = ListPage {
        rows,
        hosts: all_hosts
            .into_iter()
            .filter(|h| providers::can_list(h.provider))
            .collect(),
        fields,
        notice: notice.map(|key| layout.t(key).to_owned()),
        error: error.map(|key| layout.t(key).to_owned()),
        layout,
    };
    let status = if page.error.is_some() {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::OK
    };
    Ok((status, render(&page)?).into_response())
}

pub async fn list(
    ctx: PageContext,
    Query(query): Query<NoticeQuery>,
) -> Result<Response, AppError> {
    let notice = notice_key(query.notice.as_deref());
    list_page(&ctx, SourceFields::blank(), notice, None).await
}

/// Checks the form. The inner `Err` carries the message key to show.
async fn validate(
    ctx: &PageContext,
    form: &SourceForm,
) -> Result<Result<(Host, SourceKind, String), &'static str>, AppError> {
    let host = match form.host_id.parse() {
        Ok(id) => hosts::get(&ctx.state.pool, id).await?,
        Err(_) => None,
    };
    let Some(host) = host.filter(|h| providers::can_list(h.provider)) else {
        return Ok(Err("sources.invalid_host"));
    };
    let Some(kind) = SourceKind::parse(&form.kind) else {
        return Ok(Err("sources.invalid_kind"));
    };
    if !providers::supports(host.provider, kind) {
        return Ok(Err("sources.kind_unsupported"));
    }
    // Only GitLab groups nest.
    let nested = host.provider == Provider::Gitlab && kind == SourceKind::Org;
    let Some(name) = repo_url::parse_owner(&form.name, nested) else {
        return Ok(Err("sources.invalid_name"));
    };
    Ok(Ok((host, kind, name)))
}

pub async fn create(ctx: PageContext, Form(form): Form<SourceForm>) -> Result<Response, AppError> {
    let fields = SourceFields::from_form(&form);
    let checked = match validate(&ctx, &form).await? {
        Ok(valid) => form.options.parse().map(|options| (valid, options)),
        Err(key) => Err(key),
    };
    let ((host, kind, name), options) = match checked {
        Ok(checked) => checked,
        Err(key) => return list_page(&ctx, fields, None, Some(key)).await,
    };
    let new = NewSource {
        host_id: host.id,
        kind,
        name: &name,
        options,
    };
    let Some(id) = sources::insert(&ctx.state.pool, &new, unix_now()).await? else {
        return list_page(&ctx, fields, None, Some("sources.exists")).await;
    };
    tracing::info!(host = %host.host, %name, "source added");
    scan_by_id(&ctx, id).await?;
    Ok(Redirect::to("/sources?notice=added").into_response())
}

/// Starts a scan in the background; the page shows its result later.
async fn scan_by_id(ctx: &PageContext, id: i64) -> Result<(), AppError> {
    let source = sources::get(&ctx.state.pool, id)
        .await?
        .ok_or(AppError::NotFound)?;
    drop(ctx.state.scheduler.scan_now(source));
    Ok(())
}

pub async fn scan(ctx: PageContext, Path(id): Path<i64>) -> Result<Response, AppError> {
    scan_by_id(&ctx, id).await?;
    Ok(Redirect::to("/sources?notice=scanning").into_response())
}

#[derive(Debug, Deserialize)]
pub struct OptionsForm {
    auto_add: Option<String>,
    include_forks: Option<String>,
    skip_archived: Option<String>,
    include_patterns: Option<String>,
    exclude_patterns: Option<String>,
}

impl OptionsForm {
    /// The checked options; the `Err` is the message key to show.
    fn parse(&self) -> Result<SourceOptions, &'static str> {
        let patterns = |text: &Option<String>| {
            name_filter::normalize(text.as_deref().unwrap_or_default())
                .map_err(|_| "sources.invalid_pattern")
        };
        Ok(SourceOptions {
            auto_add: is_checked(self.auto_add.as_deref()),
            include_forks: is_checked(self.include_forks.as_deref()),
            skip_archived: is_checked(self.skip_archived.as_deref()),
            include_patterns: patterns(&self.include_patterns)?,
            exclude_patterns: patterns(&self.exclude_patterns)?,
        })
    }
}

pub async fn update(
    ctx: PageContext,
    Path(id): Path<i64>,
    Form(form): Form<OptionsForm>,
) -> Result<Response, AppError> {
    let options = match form.parse() {
        Ok(options) => options,
        Err(key) => return list_page(&ctx, SourceFields::blank(), None, Some(key)).await,
    };
    if !sources::update_options(&ctx.state.pool, id, &options).await? {
        return Err(AppError::NotFound);
    }
    Ok(Redirect::to("/sources?notice=saved").into_response())
}

pub async fn delete(ctx: PageContext, Path(id): Path<i64>) -> Result<Response, AppError> {
    if !sources::delete(&ctx.state.pool, id).await? {
        return Err(AppError::NotFound);
    }
    tracing::info!(id, "source deleted, its repositories stay");
    Ok(Redirect::to("/sources?notice=deleted").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_notices_are_shown() {
        assert_eq!(notice_key(Some("added")), Some("sources.added"));
        assert_eq!(notice_key(Some("scanning")), Some("sources.scanning"));
        assert_eq!(notice_key(Some("<b>")), None);
        assert_eq!(notice_key(None), None);
    }
}
