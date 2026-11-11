//! Git servers with their default token: list, add, edit and delete.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::form::{encrypt_token, is_checked, optional_text};
use super::pages::{Layout, Nav, render};
use super::repos::{NoticeQuery, PageContext};
use crate::db::hosts::{self, Host, HostInput, Provider};
use crate::db::unix_now;
use crate::error::AppError;
use crate::git::ssh;
use crate::settings::CONCURRENCY_RANGE;
use crate::{ref_filter, repo_url};

/// The values of the shared form (`host_form.html`) and where it posts to.
#[derive(Debug, Clone)]
pub struct HostFields {
    pub action: String,
    /// Set when an existing host is edited.
    pub editing: bool,
    pub token_stored: bool,
    pub host: String,
    pub provider: Provider,
    pub api_base_url: String,
    pub username: String,
    /// Ref patterns, one per line.
    pub ref_exclude: String,
    /// Syncs at once on this host; empty for no own limit.
    pub max_concurrency: String,
}

impl HostFields {
    fn new_host() -> Self {
        Self {
            action: "/hosts".to_owned(),
            editing: false,
            token_stored: false,
            host: String::new(),
            provider: Provider::Generic,
            api_base_url: String::new(),
            username: String::new(),
            ref_exclude: String::new(),
            max_concurrency: String::new(),
        }
    }

    fn from_host(host: &Host) -> Self {
        Self {
            action: format!("/hosts/{}", host.id),
            editing: true,
            token_stored: host.token_enc.is_some(),
            host: host.host.clone(),
            provider: host.provider,
            api_base_url: host.api_base_url.clone().unwrap_or_default(),
            username: host.username.clone().unwrap_or_default(),
            ref_exclude: host.ref_exclude.clone().unwrap_or_default(),
            max_concurrency: host
                .max_concurrency
                .map(|limit| limit.to_string())
                .unwrap_or_default(),
        }
    }

    /// The submitted values on the same form, shown again with an error.
    fn with_input(self, form: &HostForm) -> Self {
        Self {
            host: form.host.clone(),
            provider: Provider::parse(&form.provider).unwrap_or(self.provider),
            api_base_url: form.api_base_url.clone().unwrap_or_default(),
            username: form.username.clone().unwrap_or_default(),
            ref_exclude: form.ref_exclude.clone().unwrap_or_default(),
            max_concurrency: form.max_concurrency.clone().unwrap_or_default(),
            ..self
        }
    }

    pub fn providers(&self) -> [Provider; Provider::ALL.len()] {
        Provider::ALL
    }

    pub fn is_provider(&self, provider: &Provider) -> bool {
        self.provider == *provider
    }
}

#[derive(Template)]
#[template(path = "hosts.html")]
struct ListPage {
    layout: Layout,
    hosts: Vec<Host>,
    fields: HostFields,
    notice: Option<String>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "host.html")]
struct EditPage {
    layout: Layout,
    host: Host,
    fields: HostFields,
    /// The SSH host keys stored for this host, as `known_hosts` lines.
    known_keys: Vec<String>,
    notice: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HostForm {
    host: String,
    provider: String,
    api_base_url: Option<String>,
    username: Option<String>,
    token: Option<String>,
    clear_token: Option<String>,
    ref_exclude: Option<String>,
    max_concurrency: Option<String>,
}

/// A form that passed validation, with the values to store.
struct ValidHost {
    host: String,
    provider: Provider,
    api_base_url: Option<String>,
    username: Option<String>,
    token_enc: Option<Vec<u8>>,
    ref_exclude: Option<String>,
    max_concurrency: Option<i64>,
}

/// An empty field gives `None`; a number must lie in the range of the
/// global concurrency setting.
fn parse_limit(input: Option<&str>) -> Result<Option<i64>, &'static str> {
    let Some(text) = optional_text(input) else {
        return Ok(None);
    };
    match text.parse() {
        Ok(limit) if CONCURRENCY_RANGE.contains(&limit) => Ok(Some(limit)),
        _ => Err("hosts.invalid_concurrency"),
    }
}

impl ValidHost {
    fn input(&self) -> HostInput<'_> {
        HostInput {
            host: &self.host,
            provider: self.provider,
            api_base_url: self.api_base_url.as_deref(),
            username: self.username.as_deref(),
            token_enc: self.token_enc.as_deref(),
        }
    }

    /// Stores the values that [`HostInput`] does not carry.
    async fn save_extras(&self, pool: &sqlx::SqlitePool, id: i64) -> sqlx::Result<()> {
        hosts::set_ref_exclude(pool, id, self.ref_exclude.as_deref()).await?;
        hosts::set_max_concurrency(pool, id, self.max_concurrency).await?;
        Ok(())
    }
}

/// Checks the form. The inner `Err` carries the message key to show.
fn validate(
    ctx: &PageContext,
    form: &HostForm,
) -> Result<Result<ValidHost, &'static str>, AppError> {
    let Some(host) = repo_url::parse_host(&form.host) else {
        return Ok(Err("hosts.invalid_host"));
    };
    let Some(provider) = Provider::parse(&form.provider) else {
        return Ok(Err("hosts.invalid_provider"));
    };
    let api_base_url = match repo_url::parse_api_url(form.api_base_url.as_deref().unwrap_or("")) {
        Ok(url) => url,
        Err(err) => return Ok(Err(err.message_key())),
    };
    let Ok(ref_exclude) = ref_filter::normalize(form.ref_exclude.as_deref().unwrap_or("")) else {
        return Ok(Err("refs.invalid"));
    };
    let max_concurrency = match parse_limit(form.max_concurrency.as_deref()) {
        Ok(limit) => limit,
        Err(key) => return Ok(Err(key)),
    };
    Ok(Ok(ValidHost {
        max_concurrency,
        host,
        provider,
        api_base_url,
        username: optional_text(form.username.as_deref()).map(str::to_owned),
        token_enc: encrypt_token(&ctx.state.cipher, form.token.as_deref())?,
        ref_exclude,
    }))
}

fn is_unique_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .is_some_and(|db| db.is_unique_violation())
}

/// Maps the `notice` query value to a message key, ignoring anything else.
fn notice_key(notice: Option<&str>) -> Option<&'static str> {
    match notice? {
        "saved" => Some("hosts.saved"),
        "deleted" => Some("hosts.deleted"),
        _ => None,
    }
}

/// A page that shows a form error answers 422, so tests and clients can
/// tell a rejected form from a shown one.
fn respond<T: Template>(page: &T, has_error: bool) -> Result<Response, AppError> {
    let status = if has_error {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::OK
    };
    Ok((status, render(page)?).into_response())
}

async fn list_page(
    ctx: &PageContext,
    fields: HostFields,
    notice: Option<&str>,
    error: Option<&str>,
) -> Result<Response, AppError> {
    let layout = ctx.layout(Nav::Hosts);
    let page = ListPage {
        hosts: hosts::list(&ctx.state.pool).await?,
        fields,
        notice: notice.map(|key| layout.t(key).to_owned()),
        error: error.map(|key| layout.t(key).to_owned()),
        layout,
    };
    respond(&page, page.error.is_some())
}

pub async fn list(
    ctx: PageContext,
    Query(query): Query<NoticeQuery>,
) -> Result<Response, AppError> {
    let notice = notice_key(query.notice.as_deref());
    list_page(&ctx, HostFields::new_host(), notice, None).await
}

pub async fn create(ctx: PageContext, Form(form): Form<HostForm>) -> Result<Response, AppError> {
    let fields = HostFields::new_host().with_input(&form);
    let valid = match validate(&ctx, &form)? {
        Ok(valid) => valid,
        Err(key) => return list_page(&ctx, fields, None, Some(key)).await,
    };
    let id = match hosts::insert(&ctx.state.pool, &valid.input(), unix_now()).await {
        Ok(id) => id,
        Err(err) if is_unique_violation(&err) => {
            return list_page(&ctx, fields, None, Some("hosts.exists")).await;
        }
        Err(err) => return Err(err.into()),
    };
    valid.save_extras(&ctx.state.pool, id).await?;
    tracing::info!(host = %valid.host, "host added");
    Ok(Redirect::to("/hosts?notice=saved").into_response())
}

pub(super) async fn load_host(ctx: &PageContext, id: i64) -> Result<Host, AppError> {
    hosts::get(&ctx.state.pool, id)
        .await?
        .ok_or(AppError::NotFound)
}

/// Maps the `notice` query value of the host page to a message key.
fn edit_notice_key(notice: Option<&str>) -> Option<&'static str> {
    match notice? {
        "ssh_saved" => Some("hosts.ssh_saved"),
        "ssh_removed" => Some("hosts.ssh_removed"),
        "host_key_forgotten" => Some("hosts.host_key_forgotten"),
        _ => None,
    }
}

/// The host page. `message` is a notice key, or with `is_error` a form
/// error key.
pub(super) async fn edit_page(
    ctx: &PageContext,
    host: Host,
    fields: HostFields,
    message: Option<&str>,
    is_error: bool,
) -> Result<Response, AppError> {
    let known_keys = ssh::known_keys(&ctx.state.config.known_hosts(), &host.host)
        .await
        .map_err(AppError::internal)?;
    let layout = ctx.layout(Nav::Hosts);
    let text = message.map(|key| layout.t(key).to_owned());
    let (notice, error) = if is_error { (None, text) } else { (text, None) };
    let page = EditPage {
        layout,
        host,
        fields,
        known_keys,
        notice,
        error,
    };
    respond(&page, page.error.is_some())
}

pub async fn show(
    ctx: PageContext,
    Path(id): Path<i64>,
    Query(query): Query<NoticeQuery>,
) -> Result<Response, AppError> {
    let host = load_host(&ctx, id).await?;
    let fields = HostFields::from_host(&host);
    let notice = edit_notice_key(query.notice.as_deref());
    edit_page(&ctx, host, fields, notice, false).await
}

/// The host page with the stored values and an error.
pub(super) async fn edit_error(
    ctx: &PageContext,
    host: Host,
    key: &str,
) -> Result<Response, AppError> {
    let fields = HostFields::from_host(&host);
    edit_page(ctx, host, fields, Some(key), true).await
}

pub async fn update(
    ctx: PageContext,
    Path(id): Path<i64>,
    Form(form): Form<HostForm>,
) -> Result<Response, AppError> {
    let host = load_host(&ctx, id).await?;
    let fields = HostFields::from_host(&host).with_input(&form);
    let valid = match validate(&ctx, &form)? {
        Ok(valid) => valid,
        Err(key) => return edit_page(&ctx, host, fields, Some(key), true).await,
    };
    match hosts::update(&ctx.state.pool, id, &valid.input(), unix_now()).await {
        Ok(_) => {}
        Err(err) if is_unique_violation(&err) => {
            return edit_page(&ctx, host, fields, Some("hosts.exists"), true).await;
        }
        Err(err) => return Err(err.into()),
    }
    valid.save_extras(&ctx.state.pool, id).await?;
    // A new token wins over the clear box.
    if valid.token_enc.is_none() && is_checked(form.clear_token.as_deref()) {
        hosts::clear_token(&ctx.state.pool, id).await?;
    }
    tracing::info!(host = %valid.host, "host updated");
    Ok(Redirect::to("/hosts?notice=saved").into_response())
}

pub async fn delete(ctx: PageContext, Path(id): Path<i64>) -> Result<Response, AppError> {
    let host = load_host(&ctx, id).await?;
    hosts::delete(&ctx.state.pool, id).await?;
    tracing::info!(host = %host.host, "host deleted");
    Ok(Redirect::to("/hosts?notice=deleted").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_notices_are_shown() {
        assert_eq!(notice_key(Some("saved")), Some("hosts.saved"));
        assert_eq!(notice_key(Some("deleted")), Some("hosts.deleted"));
        assert_eq!(notice_key(Some("<b>")), None);
        assert_eq!(edit_notice_key(Some("ssh_saved")), Some("hosts.ssh_saved"));
        assert_eq!(edit_notice_key(Some("saved")), None);
    }
}
