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

use super::AppState;
use super::form::is_checked;
use super::pages::{Layout, Nav, render};
use super::repos::{NoticeQuery, PageContext};
use crate::db::hosts::{self, Host};
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
    pub use_ssh: bool,
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
            use_ssh: false,
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
            use_ssh: is_checked(form.options.use_ssh.as_deref()),
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
            SourceKind::Gists => "sources.kind_gists",
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

/// Checks the kind and the name of a source of `host`; the error is the
/// message key.
pub(crate) fn check_kind_and_name(
    host: &Host,
    kind: &str,
    name: &str,
) -> Result<(SourceKind, String), &'static str> {
    let kind = SourceKind::parse(kind).ok_or("sources.invalid_kind")?;
    if !providers::supports(host, kind) {
        return Err("sources.kind_unsupported");
    }
    let depth = providers::name_depth(host.provider, kind);
    let name = repo_url::parse_owner(name, depth > 1)
        .filter(|name| name.split('/').count() <= depth)
        .ok_or("sources.invalid_name")?;
    Ok((kind, name))
}

/// The host `host_id` when it can list repositories.
async fn listing_host(state: &AppState, host_id: Option<i64>) -> Result<Option<Host>, AppError> {
    let Some(id) = host_id else {
        return Ok(None);
    };
    let host = hosts::get(&state.pool, id).await?;
    Ok(host.filter(|h| providers::can_list(h.provider)))
}

/// Adds a source of the host `host_id` and starts its first scan; returns
/// its id. The inner `Err` carries the message key.
pub(super) async fn add_source(
    state: &AppState,
    host_id: Option<i64>,
    kind: &str,
    name: &str,
    options: SourceOptions,
) -> Result<Result<i64, &'static str>, AppError> {
    let Some(host) = listing_host(state, host_id).await? else {
        return Ok(Err("sources.invalid_host"));
    };
    let (kind, name) = match check_kind_and_name(&host, kind, name) {
        Ok(checked) => checked,
        Err(key) => return Ok(Err(key)),
    };
    let new = NewSource {
        host_id: host.id,
        kind,
        name: &name,
        options,
    };
    let Some(id) = sources::insert(&state.pool, &new, unix_now()).await? else {
        return Ok(Err("sources.exists"));
    };
    tracing::info!(host = %host.host, %name, "source added");
    start_scan(state, id).await?;
    Ok(Ok(id))
}

pub async fn create(ctx: PageContext, Form(form): Form<SourceForm>) -> Result<Response, AppError> {
    let fields = SourceFields::from_form(&form);
    let options = match form.options.parse() {
        Ok(options) => options,
        Err(key) => return list_page(&ctx, fields, None, Some(key)).await,
    };
    let host_id = form.host_id.parse().ok();
    let added = add_source(&ctx.state, host_id, &form.kind, &form.name, options).await?;
    if let Err(key) = added {
        return list_page(&ctx, fields, None, Some(key)).await;
    }
    Ok(Redirect::to("/sources?notice=added").into_response())
}

/// Starts a scan in the background; `false` when there is no such source.
pub(super) async fn start_scan(state: &AppState, id: i64) -> Result<bool, AppError> {
    let Some(source) = sources::get(&state.pool, id).await? else {
        return Ok(false);
    };
    drop(state.scheduler.scan_now(source));
    Ok(true)
}

pub async fn scan(ctx: PageContext, Path(id): Path<i64>) -> Result<Response, AppError> {
    if !start_scan(&ctx.state, id).await? {
        return Err(AppError::NotFound);
    }
    Ok(Redirect::to("/sources?notice=scanning").into_response())
}

#[derive(Debug, Deserialize)]
pub struct OptionsForm {
    auto_add: Option<String>,
    include_forks: Option<String>,
    skip_archived: Option<String>,
    include_patterns: Option<String>,
    exclude_patterns: Option<String>,
    use_ssh: Option<String>,
}

/// The options of a source as the form, the import or the API sends them.
pub(crate) struct OptionsInput<'a> {
    pub auto_add: bool,
    pub include_forks: bool,
    pub skip_archived: bool,
    /// Name patterns, one per line.
    pub include_patterns: &'a str,
    pub exclude_patterns: &'a str,
    pub use_ssh: bool,
}

/// Checks the name patterns of the options; the error is the message key.
pub(crate) fn check_options(input: &OptionsInput<'_>) -> Result<SourceOptions, &'static str> {
    let patterns = |text: &str| name_filter::normalize(text).map_err(|_| "sources.invalid_pattern");
    Ok(SourceOptions {
        auto_add: input.auto_add,
        include_forks: input.include_forks,
        skip_archived: input.skip_archived,
        include_patterns: patterns(input.include_patterns)?,
        exclude_patterns: patterns(input.exclude_patterns)?,
        use_ssh: input.use_ssh,
    })
}

impl OptionsForm {
    /// The checked options; the `Err` is the message key to show.
    fn parse(&self) -> Result<SourceOptions, &'static str> {
        check_options(&OptionsInput {
            auto_add: is_checked(self.auto_add.as_deref()),
            include_forks: is_checked(self.include_forks.as_deref()),
            skip_archived: is_checked(self.skip_archived.as_deref()),
            include_patterns: self.include_patterns.as_deref().unwrap_or_default(),
            exclude_patterns: self.exclude_patterns.as_deref().unwrap_or_default(),
            use_ssh: is_checked(self.use_ssh.as_deref()),
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
