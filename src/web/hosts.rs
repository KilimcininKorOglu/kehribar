//! Git servers with their default token: list, add, edit and delete.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::AppState;
use super::form::{encrypt_token, is_checked, optional_number, optional_text};
use super::pages::{Layout, Nav, render};
use super::repos::{NoticeQuery, PageContext};
use crate::crypto::Cipher;
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

/// The values of a host as the form, the import or the API sends them.
pub(crate) struct HostValues<'a> {
    pub host: &'a str,
    pub provider: &'a str,
    pub api_base_url: &'a str,
    /// Ref patterns, one per line.
    pub ref_exclude: &'a str,
    pub max_concurrency: Option<i64>,
}

/// Host values that passed [`check_host`].
pub(crate) struct CheckedHost {
    pub host: String,
    pub provider: Provider,
    pub api_base_url: Option<String>,
    pub ref_exclude: Option<String>,
    pub max_concurrency: Option<i64>,
}

/// Applies the checks of the host form; the error is the message key.
pub(crate) fn check_host(values: &HostValues<'_>) -> Result<CheckedHost, &'static str> {
    let host = repo_url::parse_host(values.host).ok_or("hosts.invalid_host")?;
    let provider = Provider::parse(values.provider).ok_or("hosts.invalid_provider")?;
    let api_base_url =
        repo_url::parse_api_url(values.api_base_url).map_err(|err| err.message_key())?;
    let ref_exclude = ref_filter::normalize(values.ref_exclude).map_err(|_| "refs.invalid")?;
    if values
        .max_concurrency
        .is_some_and(|limit| !CONCURRENCY_RANGE.contains(&limit))
    {
        return Err("hosts.invalid_concurrency");
    }
    Ok(CheckedHost {
        host,
        provider,
        api_base_url,
        ref_exclude,
        max_concurrency: values.max_concurrency,
    })
}

/// A host that passed the checks, with the values to store.
pub(super) struct ValidHost {
    checked: CheckedHost,
    username: Option<String>,
    token_enc: Option<Vec<u8>>,
}

impl ValidHost {
    fn input(&self) -> HostInput<'_> {
        HostInput {
            host: &self.checked.host,
            provider: self.checked.provider,
            api_base_url: self.checked.api_base_url.as_deref(),
            username: self.username.as_deref(),
            token_enc: self.token_enc.as_deref(),
        }
    }
}

/// Checks the values and encrypts the token. The inner `Err` carries the
/// message key.
pub(super) fn valid_host(
    cipher: &Cipher,
    values: &HostValues<'_>,
    username: Option<&str>,
    token: Option<&str>,
) -> Result<Result<ValidHost, &'static str>, AppError> {
    let checked = match check_host(values) {
        Ok(checked) => checked,
        Err(key) => return Ok(Err(key)),
    };
    Ok(Ok(ValidHost {
        checked,
        username: optional_text(username).map(str::to_owned),
        token_enc: encrypt_token(cipher, token)?,
    }))
}

/// Checks the form. The inner `Err` carries the message key to show.
fn validate(
    ctx: &PageContext,
    form: &HostForm,
) -> Result<Result<ValidHost, &'static str>, AppError> {
    let Ok(max_concurrency) = optional_number(form.max_concurrency.as_deref(), CONCURRENCY_RANGE)
    else {
        return Ok(Err("hosts.invalid_concurrency"));
    };
    let values = HostValues {
        host: &form.host,
        provider: &form.provider,
        api_base_url: form.api_base_url.as_deref().unwrap_or(""),
        ref_exclude: form.ref_exclude.as_deref().unwrap_or(""),
        max_concurrency,
    };
    let (username, token) = (form.username.as_deref(), form.token.as_deref());
    valid_host(&ctx.state.cipher, &values, username, token)
}

fn is_unique_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .is_some_and(|db| db.is_unique_violation())
}

/// Stores a new host, or with `id` the changes of that host, and returns
/// its id. Without a new token, `clear_token` removes the stored one. The
/// inner `Err` carries the message key.
pub(super) async fn save_host(
    state: &AppState,
    id: Option<i64>,
    valid: &ValidHost,
    clear_token: bool,
) -> Result<Result<i64, &'static str>, AppError> {
    let pool = &state.pool;
    let (input, now) = (valid.input(), unix_now());
    let saved = match id {
        None => hosts::insert(pool, &input, now).await,
        Some(id) => hosts::update(pool, id, &input, now).await.map(|_| id),
    };
    let id = match saved {
        Ok(id) => id,
        Err(err) if is_unique_violation(&err) => return Ok(Err("hosts.exists")),
        Err(err) => return Err(err.into()),
    };
    hosts::set_ref_exclude(pool, id, valid.checked.ref_exclude.as_deref()).await?;
    hosts::set_max_concurrency(pool, id, valid.checked.max_concurrency).await?;
    if valid.token_enc.is_none() && clear_token {
        hosts::clear_token(pool, id).await?;
    }
    tracing::info!(host = %valid.checked.host, "host saved");
    Ok(Ok(id))
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
    if let Err(key) = save_host(&ctx.state, None, &valid, false).await? {
        return list_page(&ctx, fields, None, Some(key)).await;
    }
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
    let known_keys = ssh::known_keys(&ctx.state.config.known_hosts_of(&host.host))
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
    // A new token wins over the clear box.
    let clear = is_checked(form.clear_token.as_deref());
    if let Err(key) = save_host(&ctx.state, Some(id), &valid, clear).await? {
        return edit_page(&ctx, host, fields, Some(key), true).await;
    }
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
