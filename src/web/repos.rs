//! Repository list, creation, detail, settings, manual sync and deletion.

use askama::Template;
use axum::Form;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::AppState;
use super::auth::CurrentSession;
use super::form::{encrypt_token, is_checked, optional_number, optional_text};
use super::pages::{Layout, Nav, render};
use super::prefs::Prefs;
use crate::db::hosts::{self, Host};
use crate::db::repos::{self, NewRepo, Repo, RepoSettings, RepoStatus};
use crate::db::sync_runs::{self, SyncRun};
use crate::db::unix_now;
use crate::error::AppError;
use crate::git::mirror;
use crate::repo_url;
use crate::settings::INTERVAL_RANGE;
use crate::sync::credentials::{self, CredentialSource};

/// Sync runs listed on the detail page.
const HISTORY_LIMIT: i64 = 20;

/// Common inputs of a signed-in page.
pub struct PageContext {
    pub state: AppState,
    pub prefs: Prefs,
    pub session: CurrentSession,
    pub path: String,
    /// The `Host` header, which builds absolute URLs such as the clone URL.
    pub host: Option<String>,
}

impl PageContext {
    pub fn layout(&self, nav: Nav) -> Layout {
        Layout::private(&self.state, self.prefs, &self.session, nav, &self.path)
    }
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for PageContext
where
    AppState: axum::extract::FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let session = parts
            .extensions
            .get::<CurrentSession>()
            .cloned()
            .ok_or(AppError::Forbidden)?;
        Ok(Self {
            state: axum::extract::FromRef::from_ref(state),
            prefs: Prefs::from_headers(&parts.headers),
            session,
            path: parts.uri.path().to_owned(),
            host: parts
                .headers
                .get(axum::http::header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        })
    }
}

#[derive(Template)]
#[template(path = "repos.html")]
struct ListPage {
    layout: Layout,
    repos: Vec<Repo>,
    query: String,
    total: usize,
    busy: bool,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    q: Option<String>,
}

pub async fn list(
    ctx: PageContext,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<ListQuery>,
) -> Result<Response, AppError> {
    let query = query.q.unwrap_or_default();
    let repos = repos::list(&ctx.state.pool, Some(&query)).await?;
    let total = if query.trim().is_empty() {
        repos.len()
    } else {
        repos::list(&ctx.state.pool, None).await?.len()
    };
    let busy = repos
        .iter()
        .any(|r| matches!(r.status, RepoStatus::Pending | RepoStatus::Syncing));
    let mut layout = ctx.layout(Nav::Repos);
    layout.path = uri.to_string();
    let page = ListPage {
        layout,
        repos,
        query,
        total,
        busy,
    };
    Ok(render(&page)?.into_response())
}

#[derive(Template)]
#[template(path = "repo_new.html")]
struct NewPage {
    layout: Layout,
    url: String,
    wiki: bool,
    interval: String,
    username: String,
    error: Option<String>,
}

pub async fn new_form(ctx: PageContext) -> Result<Response, AppError> {
    let page = NewPage {
        layout: ctx.layout(Nav::Repos),
        url: String::new(),
        wiki: true,
        interval: String::new(),
        username: String::new(),
        error: None,
    };
    Ok(render(&page)?.into_response())
}

#[derive(Debug, Deserialize)]
pub struct NewRepoForm {
    url: String,
    wiki: Option<String>,
    interval: Option<String>,
    username: Option<String>,
    token: Option<String>,
}

pub async fn create(ctx: PageContext, Form(form): Form<NewRepoForm>) -> Result<Response, AppError> {
    let wiki = is_checked(form.wiki.as_deref());
    // The token is never sent back to the browser; it must be typed again.
    let invalid = |ctx: &PageContext, key: &str| {
        let layout = ctx.layout(Nav::Repos);
        let page = NewPage {
            error: Some(layout.t(key).to_owned()),
            layout,
            url: form.url.clone(),
            wiki,
            interval: form.interval.clone().unwrap_or_default(),
            username: form.username.clone().unwrap_or_default(),
        };
        Ok((StatusCode::UNPROCESSABLE_ENTITY, render(&page)?).into_response())
    };
    let location = match repo_url::parse(&form.url) {
        Ok(location) => location,
        Err(err) => return invalid(&ctx, err.message_key()),
    };
    let Ok(interval) = optional_number(form.interval.as_deref(), INTERVAL_RANGE) else {
        return invalid(&ctx, "form.interval_invalid");
    };
    let token_enc = encrypt_token(&ctx.state.cipher, form.token.as_deref())?;
    let repo = NewRepo {
        url: &location.url,
        host: &location.host,
        owner: &location.owner,
        name: &location.name,
        source_id: None,
        username: optional_text(form.username.as_deref()),
        token_enc: token_enc.as_deref(),
        interval_minutes: interval,
        wiki_enabled: wiki,
    };
    let Some(id) = repos::insert(&ctx.state.pool, &repo, unix_now()).await? else {
        return invalid(&ctx, "repo_new.exists");
    };
    tracing::info!(url = %location.url, "repository added");
    ctx.state.scheduler.wake();
    Ok(Redirect::to(&format!("/repos/{id}")).into_response())
}

#[derive(Template)]
#[template(path = "repo.html")]
struct DetailPage {
    layout: Layout,
    repo: Repo,
    runs: Vec<SyncRun>,
    interval: String,
    global_interval: String,
    global_minutes: i64,
    notice: Option<String>,
    error: Option<String>,
    full_name: String,
    /// The host entry of this repository, when there is one.
    host: Option<Host>,
    credential_source: CredentialSource,
    /// The active tab of `repo_tabs.html`.
    tab: &'static str,
    /// `git clone` URL of the archive on this site.
    clone_url: String,
}

impl DetailPage {
    fn busy(&self) -> bool {
        matches!(self.repo.status, RepoStatus::Pending | RepoStatus::Syncing)
    }

    /// The sentence that says where the token of this repository comes from.
    fn credential_text(&self) -> String {
        match self.credential_source {
            CredentialSource::Repo => self.layout.t("cred.source_repo").to_owned(),
            CredentialSource::Host => self
                .layout
                .tf("cred.source_host", &[("host", &self.repo.host)]),
            CredentialSource::Anonymous => self.layout.t("cred.source_none").to_owned(),
        }
    }

    fn has_own_token(&self) -> bool {
        self.credential_source == CredentialSource::Repo
    }
}

#[derive(Debug, Deserialize)]
pub struct NoticeQuery {
    pub notice: Option<String>,
}

async fn load_repo(state: &AppState, id: i64) -> Result<Repo, AppError> {
    repos::get(&state.pool, id).await?.ok_or(AppError::NotFound)
}

async fn detail_page(
    ctx: &PageContext,
    repo: Repo,
    notice_key: Option<&str>,
    error: Option<String>,
) -> Result<DetailPage, AppError> {
    let runs = sync_runs::list_for_repo(&ctx.state.pool, repo.id, HISTORY_LIMIT).await?;
    let global = crate::settings::effective(&ctx.state.pool, &ctx.state.config).await?;
    let host = hosts::find_by_host(&ctx.state.pool, &repo.host).await?;
    let credential_source = credentials::source(&repo, host.as_ref());
    let clone_url = clone_url(ctx, &repo);
    let layout = ctx.layout(Nav::Repos);
    let notice = notice_key.map(|key| layout.t(key).to_owned());
    let global_interval = layout.tf_number("repo.interval_global", &global.interval_minutes);
    Ok(DetailPage {
        interval: repo
            .interval_minutes
            .map(|m| m.to_string())
            .unwrap_or_default(),
        full_name: format!("{}/{}", repo.owner, repo.name),
        global_interval,
        global_minutes: global.interval_minutes,
        layout,
        repo,
        runs,
        notice,
        error,
        host,
        credential_source,
        tab: "overview",
        clone_url,
    })
}

/// `http(s)://<site>/git/<host>/<owner>/<name>.git`. The scheme follows the
/// secure-cookie setting, which is on when the site runs behind HTTPS.
fn clone_url(ctx: &PageContext, repo: &Repo) -> String {
    let scheme = if ctx.state.config.cookie_secure {
        "https"
    } else {
        "http"
    };
    let site = ctx.host.as_deref().unwrap_or("localhost");
    format!(
        "{scheme}://{site}/git/{}/{}/{}.git",
        repo.host, repo.owner, repo.name
    )
}

/// Maps the `notice` query value to a message key, ignoring anything else.
fn notice_key(notice: Option<&str>) -> Option<&'static str> {
    match notice? {
        "queued" => Some("repo.sync_queued"),
        "saved" => Some("repo.saved"),
        "credentials" => Some("cred.saved"),
        _ => None,
    }
}

pub async fn show(
    ctx: PageContext,
    Path(id): Path<i64>,
    Query(query): Query<NoticeQuery>,
) -> Result<Response, AppError> {
    let repo = load_repo(&ctx.state, id).await?;
    let page = detail_page(&ctx, repo, notice_key(query.notice.as_deref()), None).await?;
    Ok(render(&page)?.into_response())
}

pub async fn sync_now(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    if !repos::schedule_now(&state.pool, id, unix_now()).await? {
        return Err(AppError::NotFound);
    }
    state.scheduler.wake();
    Ok(Redirect::to(&format!("/repos/{id}?notice=queued")).into_response())
}

#[derive(Debug, Deserialize)]
pub struct RepoSettingsForm {
    wiki: Option<String>,
    interval: Option<String>,
}

pub async fn update(
    ctx: PageContext,
    Path(id): Path<i64>,
    Form(form): Form<RepoSettingsForm>,
) -> Result<Response, AppError> {
    let repo = load_repo(&ctx.state, id).await?;
    let Ok(interval) = optional_number(form.interval.as_deref(), INTERVAL_RANGE) else {
        let message = ctx.layout(Nav::Repos).t("form.interval_invalid").to_owned();
        let page = detail_page(&ctx, repo, None, Some(message)).await?;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, render(&page)?).into_response());
    };
    let settings = RepoSettings {
        interval_minutes: interval,
        wiki_enabled: is_checked(form.wiki.as_deref()),
    };
    repos::update_settings(&ctx.state.pool, id, settings).await?;
    Ok(Redirect::to(&format!("/repos/{id}?notice=saved")).into_response())
}

#[derive(Debug, Deserialize)]
pub struct CredentialsForm {
    username: Option<String>,
    token: Option<String>,
    clear_token: Option<String>,
}

pub async fn update_credentials(
    ctx: PageContext,
    Path(id): Path<i64>,
    Form(form): Form<CredentialsForm>,
) -> Result<Response, AppError> {
    load_repo(&ctx.state, id).await?;
    let token_enc = encrypt_token(&ctx.state.cipher, form.token.as_deref())?;
    let username = optional_text(form.username.as_deref());
    repos::update_credentials(&ctx.state.pool, id, username, token_enc.as_deref()).await?;
    // A new token wins over the clear box.
    if token_enc.is_none() && is_checked(form.clear_token.as_deref()) {
        repos::clear_token(&ctx.state.pool, id).await?;
    }
    Ok(Redirect::to(&format!("/repos/{id}?notice=credentials")).into_response())
}

#[derive(Debug, Deserialize)]
pub struct DeleteForm {
    confirm: String,
}

pub async fn delete(
    ctx: PageContext,
    Path(id): Path<i64>,
    Form(form): Form<DeleteForm>,
) -> Result<Response, AppError> {
    let repo = load_repo(&ctx.state, id).await?;
    let full_name = format!("{}/{}", repo.owner, repo.name);
    let refusal = if repo.status == RepoStatus::Syncing {
        Some("repo.delete_busy")
    } else if form.confirm.trim() != full_name {
        Some("repo.delete_mismatch")
    } else {
        None
    };
    if let Some(key) = refusal {
        let message = ctx.layout(Nav::Repos).t(key).to_owned();
        let page = detail_page(&ctx, repo, None, Some(message)).await?;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, render(&page)?).into_response());
    }
    let dir = repo_url::archive_dir(
        &ctx.state.config.data_dir,
        &repo.host,
        &repo.owner,
        &repo.name,
    );
    repos::delete(&ctx.state.pool, id).await?;
    remove_dir(&mirror::wiki_dir(&dir)).await?;
    remove_dir(&dir).await?;
    tracing::info!(url = %repo.url, "repository deleted with its archive");
    Ok(Redirect::to("/").into_response())
}

async fn remove_dir(path: &std::path::Path) -> Result<(), AppError> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(AppError::internal(format!(
            "cannot delete {}: {err}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_notices_are_shown() {
        assert_eq!(notice_key(Some("queued")), Some("repo.sync_queued"));
        assert_eq!(notice_key(Some("credentials")), Some("cred.saved"));
        assert_eq!(notice_key(Some("<script>")), None);
        assert_eq!(notice_key(None), None);
    }
}
