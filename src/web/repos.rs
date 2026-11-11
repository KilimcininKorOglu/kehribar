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
use super::hooks;
use super::pages::{Layout, Nav, render};
use super::prefs::Prefs;
use crate::db::assets::{self, Check};
use crate::db::hosts::{self, Host};
use crate::db::ref_events::{self, RefEvent};
use crate::db::repos::{self, ListFilter, NewRepo, Repo, RepoSettings, RepoSort, RepoStatus};
use crate::db::sync_runs::{self, SyncRun};
use crate::db::unix_now;
use crate::error::AppError;
use crate::git::mirror;
use crate::settings::INTERVAL_RANGE;
use crate::sync::assets::{self as sync_assets, MAX_IMAGES};
use crate::sync::credentials::{self, CredentialSource};
use crate::{ref_filter, repo_url};

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

/// Repositories on one page of the list.
const PAGE_SIZE: i64 = 50;

#[derive(Template)]
#[template(path = "repos.html")]
struct ListPage {
    layout: Layout,
    repos: Vec<Repo>,
    view: ListView,
    /// Repositories in the archive, whatever the filter.
    total: i64,
    chips: Vec<Chip>,
    heads: Vec<Head>,
    pager: Option<Pager>,
    busy: bool,
}

/// A status filter link above the list.
struct Chip {
    label: String,
    url: String,
    count: i64,
    current: bool,
}

/// A column head; a sortable column links to its sort.
struct Head {
    label: String,
    url: Option<String>,
    /// `aria-sort`: `ascending`, `descending` or `none`.
    aria: &'static str,
    /// The arrow after the label of the sorted column.
    arrow: &'static str,
    numeric: bool,
}

/// The page links of a list on several pages.
struct Pager {
    note: String,
    prev: Option<String>,
    next: Option<String>,
    links: Vec<PageLink>,
}

/// One page link; a link without a number stands for skipped pages.
struct PageLink {
    number: Option<i64>,
    url: String,
    current: bool,
}

/// The filter, sort and page of the list as the links of the page need them.
#[derive(Debug, Clone)]
pub struct ListView {
    pub query: String,
    pub status: Option<RepoStatus>,
    pub sort: RepoSort,
    pub descending: bool,
    pub page: i64,
}

impl ListView {
    fn from_query(query: ListQuery) -> Self {
        let sort = query
            .sort
            .as_deref()
            .and_then(RepoSort::parse)
            .unwrap_or_default();
        Self {
            query: query.q.unwrap_or_default(),
            status: query.status.as_deref().and_then(RepoStatus::parse),
            sort,
            descending: query.dir.as_deref() == Some("desc"),
            page: query
                .page
                .as_deref()
                .and_then(|p| p.parse().ok())
                .unwrap_or(1)
                .max(1),
        }
    }

    /// The list URL with this view's values and the given changes.
    fn url(
        &self,
        status: Option<RepoStatus>,
        sort: RepoSort,
        descending: bool,
        page: i64,
    ) -> String {
        let mut out = url::form_urlencoded::Serializer::new(String::new());
        if !self.query.is_empty() {
            out.append_pair("q", &self.query);
        }
        if let Some(status) = status {
            out.append_pair("status", status.code());
        }
        if sort != RepoSort::default() || descending {
            out.append_pair("sort", sort.code());
            out.append_pair("dir", if descending { "desc" } else { "asc" });
        }
        if page > 1 {
            out.append_pair("page", &page.to_string());
        }
        let query = out.finish();
        if query.is_empty() {
            String::from("/")
        } else {
            format!("/?{query}")
        }
    }

    fn page_url(&self, page: i64) -> String {
        self.url(self.status, self.sort, self.descending, page)
    }

    /// The "all" link and one link per status that has repositories or is
    /// the current filter. `found` holds the counts of the search.
    fn chips(&self, layout: &Layout, found: &[(RepoStatus, i64)]) -> Vec<Chip> {
        let all = Chip {
            label: layout.t("repos.status_all").to_owned(),
            url: self.url(None, self.sort, self.descending, 1),
            count: found.iter().map(|(_, n)| n).sum(),
            current: self.status.is_none(),
        };
        let per_status = RepoStatus::ALL.iter().filter_map(|status| {
            let count = count_of(found, *status);
            let current = self.status == Some(*status);
            (count > 0 || current).then(|| Chip {
                label: layout.status_label(status).to_owned(),
                url: self.url(Some(*status), self.sort, self.descending, 1),
                count,
                current,
            })
        });
        std::iter::once(all).chain(per_status).collect()
    }

    /// The column heads in table order; "next sync" is not sortable.
    fn heads(&self, layout: &Layout) -> Vec<Head> {
        let columns = [
            ("repos.col_repo", Some(RepoSort::Name)),
            ("repos.col_status", Some(RepoSort::Status)),
            ("repos.col_last_sync", Some(RepoSort::Synced)),
            ("repos.col_next", None),
            ("repos.col_size", Some(RepoSort::Size)),
        ];
        columns
            .into_iter()
            .map(|(key, sort)| self.head(layout.t(key), sort))
            .collect()
    }

    /// One column head; the column sorted already turns its direction.
    fn head(&self, label: &str, sort: Option<RepoSort>) -> Head {
        let sorted = sort == Some(self.sort);
        let (aria, arrow) = match (sorted, self.descending) {
            (false, _) => ("none", ""),
            (true, false) => ("ascending", " ↑"),
            (true, true) => ("descending", " ↓"),
        };
        Head {
            label: label.to_owned(),
            url: sort.map(|s| self.url(self.status, s, sorted && !self.descending, 1)),
            aria,
            arrow,
            numeric: sort == Some(RepoSort::Size),
        }
    }

    /// The pager of a list on `pages` pages: the first and the last page and
    /// the ones around the current page, with a gap for the pages between.
    fn pager(&self, layout: &Layout, pages: i64, shown: usize, matching: i64) -> Option<Pager> {
        if pages <= 1 {
            return None;
        }
        let current = self.page;
        let mut links = Vec::new();
        let mut last = 0;
        for n in (1..=pages).filter(|n| *n == 1 || *n == pages || (n - current).abs() <= 2) {
            if n > last + 1 {
                links.push(PageLink {
                    number: None,
                    url: String::new(),
                    current: false,
                });
            }
            links.push(PageLink {
                number: Some(n),
                url: self.page_url(n),
                current: n == current,
            });
            last = n;
        }
        let from = (current - 1) * PAGE_SIZE + 1;
        let to = from + shown as i64 - 1;
        let note = layout.tf(
            "repos.showing",
            &[
                ("from", &from.to_string()),
                ("to", &to.to_string()),
                ("n", &matching.to_string()),
            ],
        );
        Some(Pager {
            note,
            prev: (current > 1).then(|| self.page_url(current - 1)),
            next: (current < pages).then(|| self.page_url(current + 1)),
            links,
        })
    }
}

/// The number of repositories with `status` in `found`.
fn count_of(found: &[(RepoStatus, i64)], status: RepoStatus) -> i64 {
    found
        .iter()
        .find_map(|(s, n)| (*s == status).then_some(*n))
        .unwrap_or(0)
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    q: Option<String>,
    status: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
    page: Option<String>,
}

pub async fn list(
    ctx: PageContext,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<ListQuery>,
) -> Result<Response, AppError> {
    let mut view = ListView::from_query(query);
    let pool = &ctx.state.pool;
    let found = repos::status_counts(pool, &view.query).await?;
    let matching = match view.status {
        Some(status) => count_of(&found, status),
        None => found.iter().map(|(_, n)| n).sum(),
    };
    let pages = ((matching + PAGE_SIZE - 1) / PAGE_SIZE).max(1);
    view.page = view.page.min(pages);
    let filter = ListFilter {
        search: &view.query,
        status: view.status,
        sort: view.sort,
        descending: view.descending,
        limit: PAGE_SIZE,
        offset: (view.page - 1) * PAGE_SIZE,
    };
    let repos = repos::page(pool, &filter).await?;
    let busy = repos
        .iter()
        .any(|r| matches!(r.status, RepoStatus::Pending | RepoStatus::Syncing));
    let mut layout = ctx.layout(Nav::Repos);
    layout.path = uri.to_string();
    let page = ListPage {
        chips: view.chips(&layout, &found),
        heads: view.heads(&layout),
        pager: view.pager(&layout, pages, repos.len(), matching),
        total: repos::count(pool).await?,
        layout,
        repos,
        view,
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
    /// The URL list of the bulk form; after a run, the lines that failed.
    bulk_urls: String,
    bulk_rows: Vec<BulkRow>,
    bulk_error: Option<String>,
}

impl NewPage {
    fn blank(layout: Layout) -> Self {
        Self {
            layout,
            url: String::new(),
            wiki: true,
            interval: String::new(),
            username: String::new(),
            error: None,
            bulk_urls: String::new(),
            bulk_rows: Vec::new(),
            bulk_error: None,
        }
    }

    fn bulk_added(&self) -> i64 {
        let added = self
            .bulk_rows
            .iter()
            .filter(|row| matches!(row.outcome, Added::Yes(_)))
            .count();
        i64::try_from(added).unwrap_or(i64::MAX)
    }
}

/// One line of the bulk form and what became of it.
pub struct BulkRow {
    pub url: String,
    pub outcome: Added,
}

/// What adding one URL did.
pub enum Added {
    /// Added under this id.
    Yes(i64),
    /// The URL, or the host/owner/name path, is in the archive already.
    Exists,
    /// Not a valid repository URL; the message key says why.
    Invalid(&'static str),
}

/// The settings a new repository gets besides its URL.
struct AddSettings<'a> {
    interval: Option<i64>,
    wiki: bool,
    username: Option<&'a str>,
    token_enc: Option<&'a [u8]>,
}

/// Adds the repository at `url` to the archive, due for an immediate sync.
async fn add_repo(
    state: &AppState,
    url: &str,
    settings: &AddSettings<'_>,
) -> Result<Added, AppError> {
    let location = match repo_url::parse(url) {
        Ok(location) => location,
        Err(err) => return Ok(Added::Invalid(err.message_key())),
    };
    let repo = NewRepo {
        url: &location.url,
        host: &location.host,
        owner: &location.owner,
        name: &location.name,
        source_id: None,
        username: settings.username,
        token_enc: settings.token_enc,
        interval_minutes: settings.interval,
        wiki_enabled: settings.wiki,
    };
    let Some(id) = repos::insert(&state.pool, &repo, unix_now()).await? else {
        return Ok(Added::Exists);
    };
    tracing::info!(url = %location.url, "repository added");
    Ok(Added::Yes(id))
}

pub async fn new_form(ctx: PageContext) -> Result<Response, AppError> {
    let page = NewPage::blank(ctx.layout(Nav::Repos));
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
        let mut page = NewPage::blank(ctx.layout(Nav::Repos));
        page.error = Some(page.layout.t(key).to_owned());
        page.url = form.url.clone();
        page.wiki = wiki;
        page.interval = form.interval.clone().unwrap_or_default();
        page.username = form.username.clone().unwrap_or_default();
        Ok((StatusCode::UNPROCESSABLE_ENTITY, render(&page)?).into_response())
    };
    let Ok(interval) = optional_number(form.interval.as_deref(), INTERVAL_RANGE) else {
        return invalid(&ctx, "form.interval_invalid");
    };
    let token_enc = encrypt_token(&ctx.state.cipher, form.token.as_deref())?;
    let settings = AddSettings {
        interval,
        wiki,
        username: optional_text(form.username.as_deref()),
        token_enc: token_enc.as_deref(),
    };
    match add_repo(&ctx.state, &form.url, &settings).await? {
        Added::Yes(id) => {
            ctx.state.scheduler.wake();
            Ok(Redirect::to(&format!("/repos/{id}")).into_response())
        }
        Added::Exists => invalid(&ctx, "repo_new.exists"),
        Added::Invalid(key) => invalid(&ctx, key),
    }
}

#[derive(Debug, Deserialize)]
pub struct BulkForm {
    urls: String,
    wiki: Option<String>,
    interval: Option<String>,
}

/// Adds one repository per line. The page lists the outcome of every line
/// and keeps the lines that failed in the field, to be corrected.
pub async fn create_bulk(
    ctx: PageContext,
    Form(form): Form<BulkForm>,
) -> Result<Response, AppError> {
    let mut page = NewPage::blank(ctx.layout(Nav::Repos));
    page.wiki = is_checked(form.wiki.as_deref());
    page.interval = form.interval.clone().unwrap_or_default();
    let Ok(interval) = optional_number(form.interval.as_deref(), INTERVAL_RANGE) else {
        page.bulk_urls = form.urls;
        page.bulk_error = Some(page.layout.t("form.interval_invalid").to_owned());
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, render(&page)?).into_response());
    };
    let settings = AddSettings {
        interval,
        wiki: page.wiki,
        username: None,
        token_enc: None,
    };
    for url in form.urls.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let outcome = add_repo(&ctx.state, url, &settings).await?;
        page.bulk_rows.push(BulkRow {
            url: url.to_owned(),
            outcome,
        });
    }
    page.bulk_urls = page
        .bulk_rows
        .iter()
        .filter(|row| matches!(row.outcome, Added::Invalid(_)))
        .map(|row| row.url.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if page.bulk_added() > 0 {
        ctx.state.scheduler.wake();
    }
    Ok(render(&page)?.into_response())
}

#[derive(Template)]
#[template(path = "repo.html")]
struct DetailPage {
    layout: Layout,
    repo: Repo,
    runs: Vec<SyncRun>,
    /// The ref events of the listed runs.
    events: Vec<RefEvent>,
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
    /// Text of the ref pattern field, one pattern per line.
    ref_exclude: String,
    /// Archived images of the Markdown files.
    image_count: usize,
    image_check: Option<Check>,
    /// The push webhook URL, when the repository has one.
    webhook_url: Option<String>,
}

impl DetailPage {
    /// "12 archived" for the images of the Markdown files.
    fn images_text(&self) -> String {
        self.layout.tf_number(
            "repo.images_archived",
            &i64::try_from(self.image_count).unwrap_or(i64::MAX),
        )
    }

    /// The warning of a check that found more images than the limit.
    fn images_skipped(&self) -> Option<String> {
        let check = self.image_check.filter(|check| check.skipped > 0)?;
        Some(self.layout.tf(
            "repo.images_skipped",
            &[
                ("found", &check.found.to_string()),
                ("max", &MAX_IMAGES.to_string()),
            ],
        ))
    }

    /// "<size> freed" for the last repack, when it freed space.
    fn repack_saved(&self) -> Option<String> {
        let saved = self.repo.repack_saved_bytes.filter(|bytes| *bytes > 0)?;
        Some(
            self.layout
                .tf("repo.repack_saved", &[("size", &self.layout.bytes(&saved))]),
        )
    }

    /// The host's ref patterns, which apply to this repository too.
    fn host_ref_exclude(&self) -> Option<String> {
        let patterns = self.host.as_ref()?.ref_exclude.as_deref()?;
        Some(patterns.lines().collect::<Vec<_>>().join(", "))
    }

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

    fn events_of(&self, run: &SyncRun) -> Vec<&RefEvent> {
        self.events
            .iter()
            .filter(|e| e.run_id == Some(run.id))
            .collect()
    }

    /// "3 ref events" for the summary of a run's details.
    fn events_label(&self, count: usize) -> String {
        self.layout
            .tf_number("repo.run_events", &i64::try_from(count).unwrap_or(i64::MAX))
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
    let events = ref_events::of_recent_runs(&ctx.state.pool, repo.id, HISTORY_LIMIT).await?;
    let global = crate::settings::effective(&ctx.state.pool, &ctx.state.config).await?;
    let host = hosts::find_by_host(&ctx.state.pool, repo_url::settings_host(&repo.host)).await?;
    let credential_source = credentials::source(&repo, host.as_ref());
    let clone_url = clone_url(ctx, &repo);
    let image_count = assets::for_repo(&ctx.state.pool, repo.id).await?.len();
    let image_check = assets::last_check(&ctx.state.pool, repo.id).await?;
    let webhook_url = hooks::current_token(ctx, repo.id)
        .await?
        .map(|token| hooks::webhook_url(ctx, &token));
    let layout = ctx.layout(Nav::Repos);
    let notice = notice_key.map(|key| layout.t(key).to_owned());
    let global_interval = layout.tf_number("repo.interval_global", &global.interval_minutes);
    Ok(DetailPage {
        interval: repo
            .interval_minutes
            .map(|m| m.to_string())
            .unwrap_or_default(),
        full_name: format!("{}/{}", repo.owner, repo.name),
        ref_exclude: repo.ref_exclude.clone().unwrap_or_default(),
        global_interval,
        global_minutes: global.interval_minutes,
        layout,
        repo,
        runs,
        events,
        notice,
        error,
        host,
        credential_source,
        tab: "overview",
        clone_url,
        image_count,
        image_check,
        webhook_url,
    })
}

/// `http(s)://<site>`, as the browser reached the site. The scheme follows
/// the secure-cookie setting, which is on when the site runs behind HTTPS.
pub(super) fn site_url(ctx: &PageContext) -> String {
    let scheme = if ctx.state.config.cookie_secure {
        "https"
    } else {
        "http"
    };
    let site = ctx.host.as_deref().unwrap_or("localhost");
    format!("{scheme}://{site}")
}

/// `http(s)://<site>/git/<host>/<owner>/<name>.git`.
fn clone_url(ctx: &PageContext, repo: &Repo) -> String {
    format!(
        "{}/git/{}/{}/{}.git",
        site_url(ctx),
        repo.host,
        repo.owner,
        repo.name
    )
}

/// Maps the `notice` query value to a message key, ignoring anything else.
fn notice_key(notice: Option<&str>) -> Option<&'static str> {
    match notice? {
        "queued" => Some("repo.sync_queued"),
        "saved" => Some("repo.saved"),
        "credentials" => Some("cred.saved"),
        "webhook_created" => Some("webhook.created"),
        "webhook_removed" => Some("webhook.removed"),
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
    lfs: Option<String>,
    interval: Option<String>,
    ref_exclude: Option<String>,
}

/// Shows the repository page again with the submitted pattern text and the
/// error of `message_key`.
async fn settings_error(
    ctx: &PageContext,
    repo: Repo,
    form: &RepoSettingsForm,
    message_key: &str,
) -> Result<Response, AppError> {
    let message = ctx.layout(Nav::Repos).t(message_key).to_owned();
    let mut page = detail_page(ctx, repo, None, Some(message)).await?;
    page.ref_exclude = form.ref_exclude.clone().unwrap_or_default();
    Ok((StatusCode::UNPROCESSABLE_ENTITY, render(&page)?).into_response())
}

pub async fn update(
    ctx: PageContext,
    Path(id): Path<i64>,
    Form(form): Form<RepoSettingsForm>,
) -> Result<Response, AppError> {
    let repo = load_repo(&ctx.state, id).await?;
    let Ok(interval) = optional_number(form.interval.as_deref(), INTERVAL_RANGE) else {
        return settings_error(&ctx, repo, &form, "form.interval_invalid").await;
    };
    let Ok(ref_exclude) = ref_filter::normalize(form.ref_exclude.as_deref().unwrap_or("")) else {
        return settings_error(&ctx, repo, &form, "refs.invalid").await;
    };
    let settings = RepoSettings {
        interval_minutes: interval,
        wiki_enabled: is_checked(form.wiki.as_deref()),
        ref_exclude: ref_exclude.as_deref(),
        lfs_enabled: is_checked(form.lfs.as_deref()),
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
    remove_repo(&ctx.state, &repo).await?;
    tracing::info!(url = %repo.url, "repository deleted with its archive");
    Ok(Redirect::to("/").into_response())
}

/// Deletes the record of a repository, its archive with the wiki, and the
/// stored images that no repository uses any more.
async fn remove_repo(state: &AppState, repo: &Repo) -> Result<(), AppError> {
    let dir = repo_url::archive_dir(&state.config.data_dir, &repo.host, &repo.owner, &repo.name);
    repos::delete(&state.pool, repo.id).await?;
    remove_dir(&mirror::wiki_dir(&dir)).await?;
    remove_dir(&dir).await?;
    sync_assets::remove_unused(&state.pool, &state.config).await?;
    Ok(())
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
