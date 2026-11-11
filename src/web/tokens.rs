//! The access tokens page: clone tokens for /git/ and API tokens for
//! /api/v1. A new token is shown once; afterwards only its record is.

use std::collections::HashMap;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};

use super::auth::{hash_token, new_token};
use super::pages::{Layout, Nav, render};
use super::repos::{NoticeQuery, PageContext, site_url};
use crate::db::repos::{self, Repo};
use crate::db::tokens::{self, AccessToken, NewToken, TokenKind};
use crate::db::unix_now;
use crate::db::usage::DAY_SECONDS;
use crate::error::AppError;

/// The readable start of a clone token.
pub const CLONE_PREFIX: &str = "gac_";
/// The readable start of an API token.
pub const API_PREFIX: &str = "gaa_";
const MAX_NAME_CHARS: usize = 100;
/// The lifetimes a token can have, in days; an empty choice never expires.
const EXPIRY_DAYS: [i64; 3] = [30, 90, 365];

fn prefix(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::Clone => CLONE_PREFIX,
        TokenKind::Api => API_PREFIX,
    }
}

/// One listed token.
struct Row {
    token: AccessToken,
    /// `host/owner/name` of the repositories of a clone token.
    repos: Vec<String>,
}

/// A token created by this request, shown once.
struct Created {
    kind: TokenKind,
    name: String,
    token: String,
}

#[derive(Template)]
#[template(path = "tokens.html")]
struct TokensPage {
    layout: Layout,
    rows: Vec<Row>,
    repos: Vec<Repo>,
    created: Option<Created>,
    notice: Option<String>,
    error: Option<String>,
    site: String,
    now: i64,
}

impl TokensPage {
    fn expiry_days(&self) -> [i64; 3] {
        EXPIRY_DAYS
    }

    fn is_clone(&self, kind: &TokenKind) -> bool {
        *kind == TokenKind::Clone
    }

    /// What the token opens: its repositories, or what the API allows.
    fn access_text(&self, row: &Row) -> String {
        let token = &row.token;
        match token.kind {
            TokenKind::Clone if token.all_repos => self.layout.t("tokens.all_repos").to_owned(),
            TokenKind::Clone => row.repos.join(", "),
            TokenKind::Api if token.can_write => self.layout.t("tokens.read_write").to_owned(),
            TokenKind::Api => self.layout.t("tokens.read_only").to_owned(),
        }
    }

    fn is_expired(&self, token: &AccessToken) -> bool {
        token.is_expired(self.now)
    }
}

/// What differs between renderings of the page.
#[derive(Default)]
struct Extra {
    created: Option<Created>,
    notice: Option<String>,
    error: Option<String>,
}

async fn respond(
    ctx: &PageContext,
    status: StatusCode,
    extra: Extra,
) -> Result<Response, AppError> {
    let pool = &ctx.state.pool;
    let mut names: HashMap<i64, Vec<String>> = HashMap::new();
    for (token_id, name) in tokens::repo_names(pool).await? {
        names.entry(token_id).or_default().push(name);
    }
    let rows = tokens::list(pool)
        .await?
        .into_iter()
        .map(|token| Row {
            repos: names.remove(&token.id).unwrap_or_default(),
            token,
        })
        .collect();
    let page = TokensPage {
        layout: ctx.layout(Nav::Settings),
        rows,
        repos: repos::list(pool, None).await?,
        created: extra.created,
        notice: extra.notice,
        error: extra.error,
        site: site_url(ctx),
        now: unix_now(),
    };
    Ok((status, render(&page)?).into_response())
}

pub async fn show(
    ctx: PageContext,
    Query(query): Query<NoticeQuery>,
) -> Result<Response, AppError> {
    let notice = match query.notice.as_deref() {
        Some("revoked") => Some(ctx.layout(Nav::Settings).t("tokens.revoked").to_owned()),
        _ => None,
    };
    let extra = Extra {
        notice,
        ..Extra::default()
    };
    respond(&ctx, StatusCode::OK, extra).await
}

/// The fields of the create form. `repo` repeats, once per chosen
/// repository, which a plain `Form` cannot read.
#[derive(Debug, Default)]
struct TokenForm {
    kind: String,
    name: String,
    scope: String,
    permission: String,
    expiry: String,
    repos: Vec<i64>,
}

impl TokenForm {
    fn parse(body: &[u8]) -> Self {
        let mut form = Self::default();
        for (key, value) in url::form_urlencoded::parse(body) {
            let slot = match key.as_ref() {
                "kind" => &mut form.kind,
                "name" => &mut form.name,
                "scope" => &mut form.scope,
                "permission" => &mut form.permission,
                "expiry" => &mut form.expiry,
                "repo" => {
                    form.repos.extend(value.parse::<i64>().ok());
                    continue;
                }
                _ => continue,
            };
            *slot = value.into_owned();
        }
        form
    }
}

/// A form that passed the checks.
struct ValidToken {
    kind: TokenKind,
    name: String,
    all_repos: bool,
    can_write: bool,
    repo_ids: Vec<i64>,
    expires_at: Option<i64>,
}

/// The end of a token's life; the `Err` is the message key.
fn expiry(value: &str, now: i64) -> Result<Option<i64>, &'static str> {
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse::<i64>()
        .ok()
        .filter(|days| EXPIRY_DAYS.contains(days))
        .map(|days| Some(now + days * DAY_SECONDS))
        .ok_or("tokens.bad_expiry")
}

/// The trimmed name; the `Err` is the message key.
fn checked_name(name: &str) -> Result<&str, &'static str> {
    let name = name.trim();
    if name.is_empty() {
        return Err("tokens.name_missing");
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err("tokens.name_too_long");
    }
    Ok(name)
}

/// Whether a clone token opens every repository, and else the chosen ones
/// that exist. An API token has no repositories.
fn chosen_repos(
    kind: TokenKind,
    form: &TokenForm,
    repos: &[Repo],
) -> Result<(bool, Vec<i64>), &'static str> {
    if kind != TokenKind::Clone {
        return Ok((false, Vec::new()));
    }
    if form.scope != "selected" {
        return Ok((true, Vec::new()));
    }
    let ids: Vec<i64> = repos
        .iter()
        .map(|repo| repo.id)
        .filter(|id| form.repos.contains(id))
        .collect();
    if ids.is_empty() {
        return Err("tokens.repos_missing");
    }
    Ok((false, ids))
}

/// Checks the form against the existing repositories. The `Err` is the
/// message key to show.
fn validate(form: &TokenForm, repos: &[Repo], now: i64) -> Result<ValidToken, &'static str> {
    let kind = TokenKind::parse(&form.kind).ok_or("tokens.bad_kind")?;
    let name = checked_name(&form.name)?;
    let (all_repos, repo_ids) = chosen_repos(kind, form, repos)?;
    Ok(ValidToken {
        kind,
        name: name.to_owned(),
        all_repos,
        can_write: kind == TokenKind::Api && form.permission == "write",
        repo_ids,
        expires_at: expiry(&form.expiry, now)?,
    })
}

pub async fn create(ctx: PageContext, body: Bytes) -> Result<Response, AppError> {
    let form = TokenForm::parse(&body);
    let now = unix_now();
    let repos = repos::list(&ctx.state.pool, None).await?;
    let valid = match validate(&form, &repos, now) {
        Ok(valid) => valid,
        Err(key) => {
            let extra = Extra {
                error: Some(ctx.layout(Nav::Settings).t(key).to_owned()),
                ..Extra::default()
            };
            return respond(&ctx, StatusCode::UNPROCESSABLE_ENTITY, extra).await;
        }
    };
    let token = format!("{}{}", prefix(valid.kind), new_token()?);
    let hash = hash_token(&token);
    let new = NewToken {
        kind: valid.kind,
        name: &valid.name,
        hash: &hash,
        all_repos: valid.all_repos,
        can_write: valid.can_write,
        repo_ids: &valid.repo_ids,
        expires_at: valid.expires_at,
    };
    tokens::create(&ctx.state.pool, &new, now).await?;
    tracing::info!(kind = valid.kind.code(), name = %valid.name, "access token created");
    let extra = Extra {
        created: Some(Created {
            kind: valid.kind,
            name: valid.name,
            token,
        }),
        ..Extra::default()
    };
    respond(&ctx, StatusCode::OK, extra).await
}

pub async fn revoke(ctx: PageContext, Path(id): Path<i64>) -> Result<Response, AppError> {
    if !tokens::revoke(&ctx.state.pool, id).await? {
        return Err(AppError::NotFound);
    }
    tracing::info!(id, "access token revoked");
    Ok(Redirect::to("/settings/tokens?notice=revoked").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_repo_fields_are_all_read() {
        let form = TokenForm::parse(
            b"_csrf=x&kind=clone&name=CI+runner&scope=selected&repo=3&repo=7&repo=bad&expiry=30",
        );
        assert_eq!(form.kind, "clone");
        assert_eq!(form.name, "CI runner");
        assert_eq!(form.repos, [3, 7]);
        assert_eq!(form.expiry, "30");
    }

    #[test]
    fn expiry_accepts_only_the_offered_lifetimes() {
        assert_eq!(expiry("", 10), Ok(None));
        assert_eq!(expiry("90", 10), Ok(Some(10 + 90 * DAY_SECONDS)));
        assert_eq!(expiry("7", 10), Err("tokens.bad_expiry"));
        assert_eq!(expiry("x", 10), Err("tokens.bad_expiry"));
    }
}
