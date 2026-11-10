//! Repository list, creation, detail, settings, manual sync and deletion.

use askama::Template;
use axum::Form;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::AppState;
use super::auth::CurrentSession;
use super::pages::{Layout, Nav, render};
use super::prefs::Prefs;
use crate::db::repos::{self, NewRepo, Repo, RepoSettings, RepoStatus};
use crate::db::sync_runs::{self, SyncRun};
use crate::db::unix_now;
use crate::error::AppError;
use crate::git::mirror;
use crate::repo_url;
use crate::settings::INTERVAL_RANGE;

/// Sync runs listed on the detail page.
const HISTORY_LIMIT: i64 = 20;

/// Common inputs of a signed-in page.
pub struct PageContext {
    pub state: AppState,
    pub prefs: Prefs,
    pub session: CurrentSession,
    pub path: String,
}

impl PageContext {
    fn layout(&self, nav: Nav) -> Layout {
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
        })
    }
}

/// A form field that is not a whole number in its allowed range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidNumber;

/// Parses an optional whole number in `range`; an empty field is `None`.
pub fn optional_number(
    text: Option<&str>,
    range: std::ops::RangeInclusive<i64>,
) -> Result<Option<i64>, InvalidNumber> {
    match text.map(str::trim).filter(|t| !t.is_empty()) {
        None => Ok(None),
        Some(text) => text
            .parse()
            .ok()
            .filter(|value| range.contains(value))
            .map(Some)
            .ok_or(InvalidNumber),
    }
}

fn is_checked(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty())
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
    error: Option<String>,
}

pub async fn new_form(ctx: PageContext) -> Result<Response, AppError> {
    let page = NewPage {
        layout: ctx.layout(Nav::Repos),
        url: String::new(),
        wiki: true,
        interval: String::new(),
        error: None,
    };
    Ok(render(&page)?.into_response())
}

#[derive(Debug, Deserialize)]
pub struct NewRepoForm {
    url: String,
    wiki: Option<String>,
    interval: Option<String>,
}

pub async fn create(ctx: PageContext, Form(form): Form<NewRepoForm>) -> Result<Response, AppError> {
    let wiki = is_checked(form.wiki.as_deref());
    let invalid = |ctx: &PageContext, key: &str| {
        let layout = ctx.layout(Nav::Repos);
        let page = NewPage {
            error: Some(layout.t(key).to_owned()),
            layout,
            url: form.url.clone(),
            wiki,
            interval: form.interval.clone().unwrap_or_default(),
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
    let repo = NewRepo {
        url: &location.url,
        host: &location.host,
        owner: &location.owner,
        name: &location.name,
        source_id: None,
        username: None,
        token_enc: None,
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
}

impl DetailPage {
    fn busy(&self) -> bool {
        matches!(self.repo.status, RepoStatus::Pending | RepoStatus::Syncing)
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
    })
}

/// Maps the `notice` query value to a message key, ignoring anything else.
fn notice_key(notice: Option<&str>) -> Option<&'static str> {
    match notice? {
        "queued" => Some("repo.sync_queued"),
        "saved" => Some("repo.saved"),
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
        username: repo.username.as_deref(),
        token_enc: None,
        interval_minutes: interval,
        wiki_enabled: is_checked(form.wiki.as_deref()),
    };
    repos::update_settings(&ctx.state.pool, id, &settings).await?;
    Ok(Redirect::to(&format!("/repos/{id}?notice=saved")).into_response())
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
    fn optional_number_accepts_blank_and_values_in_range() {
        assert_eq!(optional_number(None, 1..=10), Ok(None));
        assert_eq!(optional_number(Some("  "), 1..=10), Ok(None));
        assert_eq!(optional_number(Some(" 7 "), 1..=10), Ok(Some(7)));
        for bad in ["0", "11", "5m"] {
            assert_eq!(
                optional_number(Some(bad), 1..=10),
                Err(InvalidNumber),
                "{bad}"
            );
        }
    }

    #[test]
    fn only_known_notices_are_shown() {
        assert_eq!(notice_key(Some("queued")), Some("repo.sync_queued"));
        assert_eq!(notice_key(Some("<script>")), None);
        assert_eq!(notice_key(None), None);
    }
}
