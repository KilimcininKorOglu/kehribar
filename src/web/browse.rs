//! Browsing an archived repository: files, history, commits, refs, blame.
//!
//! The revision and the path come from query parameters (`?ref=&path=`),
//! because both can contain `/`. Without `ref`, a page shows the default
//! branch, and its links leave `ref` out too.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use askama::Template;
use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;
use url::form_urlencoded;

use super::highlight;
use super::markdown::{self, LinkTarget};
use super::pages::{Layout, Nav, render};
use super::repos::PageContext;
use crate::db::ref_events;
use crate::db::repos::{self, Repo};
use crate::error::AppError;
use crate::git::read::{
    self, Blame, BlameChunk, CommitDetail, CommitSummary, EntryKind, FileChange, FileDiff,
    RefEntry, RefKind, TreeEntry,
};
use crate::git::{Git, lfs};
use crate::repo_url;

/// Commits on one page of the history.
const PAGE_SIZE: usize = 50;
/// Reads are local and short; a hung git process must not hold a request.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// README files, in the order of preference.
const README_NAMES: &[&str] = &[
    "readme.md",
    "readme.markdown",
    "readme",
    "readme.txt",
    "readme.rst",
];
const IMAGE_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
    ("ico", "image/x-icon"),
    ("bmp", "image/bmp"),
    ("avif", "image/avif"),
];
/// A raw file is shown in a sandbox: no script runs and nothing loads
/// except images of this site.
const RAW_CSP: &str =
    "sandbox; default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'";

#[derive(Debug, Default, Deserialize)]
pub struct BrowseQuery {
    #[serde(rename = "ref")]
    rev: Option<String>,
    path: Option<String>,
    q: Option<String>,
    page: Option<usize>,
    format: Option<String>,
}

/// The revision a page shows and the links it builds.
pub struct RepoView {
    pub id: i64,
    /// The revision name as the user gave it, or the default branch.
    pub rev: String,
    /// The commit `rev` names; `None` when the archive has no commit yet.
    pub commit: Option<String>,
    /// Set when no `ref` was given: links then leave it out.
    pub is_default: bool,
    dir: PathBuf,
    git: Git,
}

impl RepoView {
    fn query(&self, rev: Option<&str>, path: &str) -> String {
        let mut pairs = form_urlencoded::Serializer::new(String::new());
        if let Some(rev) = rev {
            pairs.append_pair("ref", rev);
        }
        if !path.is_empty() {
            pairs.append_pair("path", path);
        }
        let query = pairs.finish();
        if query.is_empty() {
            query
        } else {
            format!("?{query}")
        }
    }

    fn own_rev(&self) -> Option<&str> {
        (!self.is_default).then_some(self.rev.as_str())
    }

    fn page_url(&self, page: &str, path: &str) -> String {
        format!(
            "/repos/{}/{page}{}",
            self.id,
            self.query(self.own_rev(), path)
        )
    }

    pub fn tree_url(&self, path: &str) -> String {
        self.page_url("tree", path)
    }

    pub fn blob_url(&self, path: &str) -> String {
        self.page_url("blob", path)
    }

    pub fn raw_url(&self, path: &str) -> String {
        self.page_url("raw", path)
    }

    pub fn blame_url(&self, path: &str) -> String {
        self.page_url("blame", path)
    }

    pub fn commits_url(&self) -> String {
        self.page_url("commits", "")
    }

    /// Download of the whole tree as `zip` or `tar.gz`.
    pub fn archive_url(&self, format: &str) -> String {
        let mut pairs = form_urlencoded::Serializer::new(String::new());
        if let Some(rev) = self.own_rev() {
            pairs.append_pair("ref", rev);
        }
        pairs.append_pair("format", format);
        format!("/repos/{}/archive?{}", self.id, pairs.finish())
    }

    pub fn commit_url(&self, sha: &str) -> String {
        format!("/repos/{}/commit/{sha}", self.id)
    }

    /// The files of another revision, for the refs page.
    pub fn rev_tree_url(&self, rev: &str) -> String {
        format!("/repos/{}/tree{}", self.id, self.query(Some(rev), ""))
    }

    pub fn rev_commits_url(&self, rev: &str) -> String {
        format!("/repos/{}/commits{}", self.id, self.query(Some(rev), ""))
    }

    pub fn short<'a>(&self, sha: &'a str) -> &'a str {
        sha.get(..10).unwrap_or(sha)
    }

    /// The revision to show: an object id shortened, a name in full.
    pub fn rev_label(&self) -> &str {
        if is_object_id(&self.rev) {
            self.short(&self.rev)
        } else {
            &self.rev
        }
    }

    /// The links of each directory above `path`, from the root.
    pub fn crumbs(&self, path: &str) -> Vec<(String, String)> {
        let mut crumbs = Vec::new();
        let mut current = String::new();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(part);
            crumbs.push((part.to_owned(), self.tree_url(&current)));
        }
        crumbs
    }
}

impl LinkTarget for RepoView {
    fn file_url(&self, path: &str, raw: bool) -> String {
        if raw {
            self.raw_url(path)
        } else {
            self.blob_url(path)
        }
    }
}

async fn load_repo(ctx: &PageContext, id: i64) -> Result<Repo, AppError> {
    repos::get(&ctx.state.pool, id)
        .await?
        .ok_or(AppError::NotFound)
}

/// Resolves the revision of a page. An unknown or unsafe `ref` is a 404;
/// the default branch of an archive without commits gives `commit: None`.
async fn load_view(
    ctx: &PageContext,
    repo: &Repo,
    rev: Option<&str>,
) -> Result<RepoView, AppError> {
    let given = rev.map(str::trim).filter(|r| !r.is_empty());
    if given.is_some_and(|r| !read::valid_rev(r)) {
        return Err(AppError::NotFound);
    }
    let dir = repo_url::archive_dir(
        &ctx.state.config.data_dir,
        &repo.host,
        &repo.owner,
        &repo.name,
    );
    let git = Git::new(READ_TIMEOUT);
    let rev = given
        .map(str::to_owned)
        .or_else(|| repo.default_branch.clone())
        .unwrap_or_else(|| "HEAD".to_owned());
    let commit = if dir.join("HEAD").is_file() {
        read::resolve_commit(&git, &dir, &rev).await?
    } else {
        None
    };
    if given.is_some() && commit.is_none() {
        return Err(AppError::NotFound);
    }
    Ok(RepoView {
        id: repo.id,
        rev,
        commit,
        is_default: given.is_none(),
        dir,
        git,
    })
}

fn clean_path(query: &BrowseQuery) -> Result<String, AppError> {
    read::clean_path(query.path.as_deref().unwrap_or("")).ok_or(AppError::NotFound)
}

/// The commit of a page that needs one; an empty archive has nothing to show.
fn commit_of(view: &RepoView) -> Result<&str, AppError> {
    view.commit.as_deref().ok_or(AppError::NotFound)
}

#[derive(Template)]
#[template(path = "tree.html")]
struct TreePage {
    layout: Layout,
    repo: Repo,
    view: RepoView,
    tab: &'static str,
    path: String,
    entries: Vec<TreeEntry>,
    readme: Option<Readme>,
}

impl TreePage {
    /// The directory above the current one.
    fn parent(&self) -> &str {
        self.path.rsplit_once('/').map_or("", |(parent, _)| parent)
    }

    fn size_text(&self, size: &u64) -> String {
        self.layout.bytes(&i64::try_from(*size).unwrap_or(i64::MAX))
    }
}

pub struct Readme {
    pub name: String,
    /// Sanitised HTML.
    pub html: String,
}

pub async fn tree(
    ctx: PageContext,
    Path(id): Path<i64>,
    Query(query): Query<BrowseQuery>,
) -> Result<Response, AppError> {
    let repo = load_repo(&ctx, id).await?;
    let view = load_view(&ctx, &repo, query.rev.as_deref()).await?;
    let path = clean_path(&query)?;
    let (entries, readme) = match view.commit.as_deref() {
        Some(commit) => match read::object_kind(&view.git, &view.dir, commit, &path).await? {
            Some(EntryKind::Tree) => {
                let entries = read::tree(&view.git, &view.dir, commit, &path).await?;
                let readme = load_readme(&view, commit, &path, &entries).await?;
                (entries, readme)
            }
            Some(EntryKind::Blob) => return Ok(Redirect::to(&view.blob_url(&path)).into_response()),
            _ => return Err(AppError::NotFound),
        },
        None => (Vec::new(), None),
    };
    let page = TreePage {
        layout: ctx.layout(Nav::Repos),
        repo,
        view,
        tab: "files",
        path,
        entries,
        readme,
    };
    Ok(render(&page)?.into_response())
}

async fn load_readme(
    view: &RepoView,
    commit: &str,
    dir_path: &str,
    entries: &[TreeEntry],
) -> Result<Option<Readme>, AppError> {
    let found = README_NAMES.iter().find_map(|name| {
        entries
            .iter()
            .find(|e| e.kind == EntryKind::Blob && !e.symlink && e.name.eq_ignore_ascii_case(name))
    });
    let Some(entry) = found else {
        return Ok(None);
    };
    let blob = read::blob(
        &view.git,
        &view.dir,
        commit,
        &entry.path,
        read::MAX_BLOB_BYTES,
    )
    .await?;
    if blob.too_large || blob.is_binary() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&blob.data);
    let lower = entry.name.to_ascii_lowercase();
    let html = if lower.ends_with(".md") || lower.ends_with(".markdown") {
        markdown::render(&text, dir_path, view)
    } else {
        format!("<pre>{}</pre>", highlight::to_html("readme.txt", &text))
    };
    Ok(Some(Readme {
        name: entry.name.clone(),
        html,
    }))
}

#[derive(Template)]
#[template(path = "blob.html")]
struct BlobPage {
    layout: Layout,
    repo: Repo,
    view: RepoView,
    tab: &'static str,
    path: String,
    size: usize,
    /// Highlighted HTML of a text file.
    code: Option<String>,
    line_count: usize,
    is_image: bool,
    too_large: bool,
}

impl BlobPage {
    fn line_numbers(&self) -> std::ops::RangeInclusive<usize> {
        1..=self.line_count
    }

    fn lines_text(&self) -> String {
        let count = i64::try_from(self.line_count).unwrap_or(i64::MAX);
        self.layout.tf_number("browse.lines", &count)
    }

    fn size_text(&self) -> String {
        self.layout
            .bytes(&i64::try_from(self.size).unwrap_or(i64::MAX))
    }
}

fn image_type(path: &str) -> Option<&'static str> {
    let (_, ext) = path.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    IMAGE_TYPES
        .iter()
        .find_map(|(e, mime)| (*e == ext).then_some(*mime))
}

/// Loads a file of the page's revision, redirecting a directory to its tree.
async fn file_view(
    ctx: &PageContext,
    id: i64,
    query: &BrowseQuery,
) -> Result<Result<(Repo, RepoView, String), Response>, AppError> {
    let repo = load_repo(ctx, id).await?;
    let view = load_view(ctx, &repo, query.rev.as_deref()).await?;
    let path = clean_path(query)?;
    let commit = commit_of(&view)?;
    match read::object_kind(&view.git, &view.dir, commit, &path).await? {
        Some(EntryKind::Blob) if !path.is_empty() => Ok(Ok((repo, view, path))),
        Some(EntryKind::Tree) => Ok(Err(Redirect::to(&view.tree_url(&path)).into_response())),
        _ => Err(AppError::NotFound),
    }
}

pub async fn blob(
    ctx: PageContext,
    Path(id): Path<i64>,
    Query(query): Query<BrowseQuery>,
) -> Result<Response, AppError> {
    let (repo, view, path) = match file_view(&ctx, id, &query).await? {
        Ok(found) => found,
        Err(redirect) => return Ok(redirect),
    };
    let blob = file_blob(&repo, &view, &path).await?;
    let is_image = image_type(&path).is_some();
    let (code, line_count) = highlighted(&blob, &path).await?;
    let page = BlobPage {
        layout: ctx.layout(Nav::Repos),
        repo,
        view,
        tab: "files",
        path,
        size: blob.size,
        code,
        line_count,
        is_image,
        too_large: blob.too_large,
    };
    Ok(render(&page)?.into_response())
}

/// The file at `path` for the file view; an LFS file gives its object.
async fn file_blob(repo: &Repo, view: &RepoView, path: &str) -> Result<read::Blob, AppError> {
    let commit = commit_of(view)?;
    let blob = read::blob(&view.git, &view.dir, commit, path, read::MAX_BLOB_BYTES).await?;
    match lfs_object(repo, view, &blob.data).await? {
        Some((file, size)) => lfs_blob(&file, size, read::MAX_BLOB_BYTES).await,
        None => Ok(blob),
    }
}

/// The highlighted HTML of a text file and its line count; nothing for a
/// binary file or one too large to show.
async fn highlighted(blob: &read::Blob, path: &str) -> Result<(Option<String>, usize), AppError> {
    if blob.too_large || blob.is_binary() {
        return Ok((None, 0));
    }
    let text = String::from_utf8_lossy(&blob.data).into_owned();
    let lines = text.lines().count().max(1);
    let name = path.to_owned();
    let html = tokio::task::spawn_blocking(move || highlight::to_html(&name, &text))
        .await
        .map_err(AppError::internal)?;
    Ok((Some(html), lines))
}

/// The LFS object a file stands for, with its size: when the repository
/// fetches LFS files, `data` is a pointer file and the archive holds the
/// object. Otherwise the file is shown as it is, pointer or not.
async fn lfs_object(
    repo: &Repo,
    view: &RepoView,
    data: &[u8],
) -> Result<Option<(PathBuf, u64)>, AppError> {
    if !repo.lfs_enabled {
        return Ok(None);
    }
    let Some(pointer) = lfs::parse_pointer(data) else {
        return Ok(None);
    };
    let file = lfs::local_object(&view.dir, &pointer)
        .await
        .map_err(AppError::internal)?;
    Ok(file.map(|file| (file, pointer.size)))
}

/// The first `limit` bytes of `file`.
async fn read_head(file: &std::path::Path, limit: usize) -> Result<Vec<u8>, AppError> {
    let opened = tokio::fs::File::open(file)
        .await
        .map_err(AppError::internal)?;
    let mut data = Vec::new();
    opened
        .take(u64::try_from(limit).unwrap_or(u64::MAX))
        .read_to_end(&mut data)
        .await
        .map_err(AppError::internal)?;
    Ok(data)
}

/// An LFS object as a [`read::Blob`] of at most `limit` bytes.
async fn lfs_blob(file: &std::path::Path, size: u64, limit: usize) -> Result<read::Blob, AppError> {
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    if size > limit {
        return Ok(read::Blob {
            size,
            data: Vec::new(),
            too_large: true,
        });
    }
    Ok(read::Blob {
        size,
        data: read_head(file, limit).await?,
        too_large: false,
    })
}

/// The streamed content of a file and its size with its first bytes, which
/// decide between text and download. An LFS file streams its object.
async fn raw_body(
    repo: &Repo,
    view: &RepoView,
    path: &str,
) -> Result<(Body, read::Blob), AppError> {
    let commit = commit_of(view)?;
    let head = read::peek(&view.git, &view.dir, commit, path, 8000).await?;
    if let Some((file, size)) = lfs_object(repo, view, &head.data).await? {
        let opened = tokio::fs::File::open(&file)
            .await
            .map_err(AppError::internal)?;
        let head = read::Blob {
            size: usize::try_from(size).unwrap_or(usize::MAX),
            data: read_head(&file, 8000).await?,
            too_large: false,
        };
        return Ok((Body::from_stream(ReaderStream::new(opened)), head));
    }
    let spec = format!("{commit}:{path}");
    let stdout = view
        .git
        .spawn_stdout(&view.dir, &["cat-file", "blob", &spec])?;
    Ok((Body::from_stream(ReaderStream::new(stdout)), head))
}

/// The file as it is, streamed. Images keep their type; any other file is
/// plain text or a download, so archived HTML never runs as a page.
pub async fn raw(
    ctx: PageContext,
    Path(id): Path<i64>,
    Query(query): Query<BrowseQuery>,
) -> Result<Response, AppError> {
    let (repo, view, path) = match file_view(&ctx, id, &query).await? {
        Ok(found) => found,
        Err(redirect) => return Ok(redirect),
    };
    let (body, head) = raw_body(&repo, &view, &path).await?;
    let name = path.rsplit('/').next().unwrap_or(&path);
    let content_type = image_type(&path).unwrap_or(if head.is_binary() {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    });
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(RAW_CSP),
    );
    if let Ok(length) = HeaderValue::from_str(&head.size.to_string()) {
        headers.insert(header::CONTENT_LENGTH, length);
    }
    if content_type == "application/octet-stream" {
        let disposition = format!("attachment; filename*=UTF-8''{}", encode_filename(name));
        if let Ok(value) = HeaderValue::from_str(&disposition) {
            headers.insert(header::CONTENT_DISPOSITION, value);
        }
    }
    Ok(response)
}

/// The formats `git archive` writes: query value, git format, MIME type.
const ARCHIVE_FORMATS: &[(&str, &str, &str)] = &[
    ("zip", "zip", "application/zip"),
    ("tar.gz", "tar.gz", "application/gzip"),
];

/// The tree of a revision as a ZIP or tar.gz file, streamed from
/// `git archive`. Its files sit under `<name>-<revision>/`.
pub async fn archive(
    ctx: PageContext,
    Path(id): Path<i64>,
    Query(query): Query<BrowseQuery>,
) -> Result<Response, AppError> {
    let wanted = query.format.as_deref().unwrap_or("zip");
    let (ext, format, mime) = ARCHIVE_FORMATS
        .iter()
        .copied()
        .find(|(ext, ..)| *ext == wanted)
        .ok_or(AppError::NotFound)?;
    let repo = load_repo(&ctx, id).await?;
    let view = load_view(&ctx, &repo, query.rev.as_deref()).await?;
    let commit = commit_of(&view)?;
    let label = archive_label(&repo.name, view.rev_label());
    let format_arg = format!("--format={format}");
    let prefix_arg = format!("--prefix={label}/");
    // With LFS on, git-lfs writes the content of the objects into the file.
    let mut args: Vec<&str> = if repo.lfs_enabled {
        lfs::ARCHIVE_CONFIG.to_vec()
    } else {
        Vec::new()
    };
    args.extend(["archive", format_arg.as_str(), prefix_arg.as_str(), commit]);
    let stdout = view.git.spawn_stdout(&view.dir, &args)?;
    let mut response = Body::from_stream(ReaderStream::new(stdout)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    let disposition = format!("attachment; filename=\"{label}.{ext}\"");
    let value = HeaderValue::from_str(&disposition).map_err(AppError::internal)?;
    headers.insert(header::CONTENT_DISPOSITION, value);
    Ok(response)
}

/// `<name>-<revision>` with every character outside `[A-Za-z0-9._-]`
/// replaced, so it is safe as a file name and inside a header.
fn archive_label(name: &str, rev: &str) -> String {
    format!("{name}-{rev}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// RFC 5987 encoding of a file name for `Content-Disposition`.
fn encode_filename(name: &str) -> String {
    form_urlencoded::byte_serialize(name.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

#[derive(Template)]
#[template(path = "blame.html")]
struct BlamePage {
    layout: Layout,
    repo: Repo,
    view: RepoView,
    tab: &'static str,
    path: String,
    blame: Option<Blame>,
}

impl BlamePage {
    fn commit_info(&self, sha: &str) -> Option<&read::BlameCommit> {
        self.blame.as_ref()?.commits.get(sha)
    }

    /// The line numbers of a chunk in the file.
    fn line_range(&self, chunk: &BlameChunk) -> std::ops::Range<usize> {
        chunk.first_line..chunk.first_line + chunk.lines.len()
    }
}

pub async fn blame(
    ctx: PageContext,
    Path(id): Path<i64>,
    Query(query): Query<BrowseQuery>,
) -> Result<Response, AppError> {
    let (repo, view, path) = match file_view(&ctx, id, &query).await? {
        Ok(found) => found,
        Err(redirect) => return Ok(redirect),
    };
    let commit = commit_of(&view)?;
    // Blame of a huge or binary file is slow and unreadable.
    let head = read::blob(&view.git, &view.dir, commit, &path, read::MAX_BLOB_BYTES).await?;
    let blame = if head.too_large || head.is_binary() {
        None
    } else {
        Some(read::blame(&view.git, &view.dir, commit, &path).await?)
    };
    let page = BlamePage {
        layout: ctx.layout(Nav::Repos),
        repo,
        view,
        tab: "files",
        path,
        blame,
    };
    Ok(render(&page)?.into_response())
}

#[derive(Template)]
#[template(path = "commits.html")]
struct CommitsPage {
    layout: Layout,
    repo: Repo,
    view: RepoView,
    tab: &'static str,
    commits: Vec<CommitSummary>,
    search: String,
    page: usize,
    has_next: bool,
}

impl CommitsPage {
    /// Link to another page of the same list. Takes a value: askama passes
    /// the computed page number by value.
    fn page_url(&self, page: usize) -> String {
        let mut pairs = form_urlencoded::Serializer::new(String::new());
        if let Some(rev) = self.view.own_rev() {
            pairs.append_pair("ref", rev);
        }
        if !self.search.is_empty() {
            pairs.append_pair("q", &self.search);
        }
        if page > 1 {
            pairs.append_pair("page", &page.to_string());
        }
        let query = pairs.finish();
        let separator = if query.is_empty() { "" } else { "?" };
        format!("/repos/{}/commits{separator}{query}", self.view.id)
    }

    fn previous_page(&self) -> usize {
        self.page - 1
    }

    fn next_page(&self) -> usize {
        self.page + 1
    }
}

pub async fn commits(
    ctx: PageContext,
    Path(id): Path<i64>,
    Query(query): Query<BrowseQuery>,
) -> Result<Response, AppError> {
    let repo = load_repo(&ctx, id).await?;
    let view = load_view(&ctx, &repo, query.rev.as_deref()).await?;
    let search = query
        .q
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    let page = query.page.unwrap_or(1).clamp(1, 100_000);
    let mut commits = match view.commit.as_deref() {
        Some(commit) => {
            let filter = (!search.is_empty()).then_some(search.as_str());
            let skip = (page - 1) * PAGE_SIZE;
            read::log(&view.git, &view.dir, commit, filter, skip, PAGE_SIZE + 1).await?
        }
        None => Vec::new(),
    };
    let has_next = commits.len() > PAGE_SIZE;
    commits.truncate(PAGE_SIZE);
    let page = CommitsPage {
        layout: ctx.layout(Nav::Repos),
        repo,
        view,
        tab: "commits",
        commits,
        search,
        page,
        has_next,
    };
    Ok(render(&page)?.into_response())
}

#[derive(Template)]
#[template(path = "commit.html")]
struct CommitPage {
    layout: Layout,
    repo: Repo,
    view: RepoView,
    tab: &'static str,
    commit: CommitDetail,
    files: Vec<FileDiff>,
    diff_cut: bool,
}

impl CommitPage {
    fn subject(&self) -> &str {
        self.commit.message.lines().next().unwrap_or_default()
    }

    /// The message without its first line.
    fn body(&self) -> &str {
        self.commit
            .message
            .split_once('\n')
            .map_or("", |(_, rest)| rest.trim())
    }

    fn files_text(&self) -> String {
        let count = i64::try_from(self.files.len()).unwrap_or(i64::MAX);
        self.layout.tf_number("browse.files_changed", &count)
    }
}

pub fn is_object_id(text: &str) -> bool {
    (4..=64).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

pub async fn commit(
    ctx: PageContext,
    Path((id, sha)): Path<(i64, String)>,
) -> Result<Response, AppError> {
    if !is_object_id(&sha) {
        return Err(AppError::NotFound);
    }
    let repo = load_repo(&ctx, id).await?;
    let view = load_view(&ctx, &repo, Some(&sha)).await?;
    let full = commit_of(&view)?.to_owned();
    let detail = read::commit(&view.git, &view.dir, &full)
        .await?
        .ok_or(AppError::NotFound)?;
    let (files, diff_cut) = read::diff(&view.git, &view.dir, &full).await?;
    let page = CommitPage {
        layout: ctx.layout(Nav::Repos),
        repo,
        view,
        tab: "commits",
        commit: detail,
        files,
        diff_cut,
    };
    Ok(render(&page)?.into_response())
}

#[derive(Template)]
#[template(path = "refs.html")]
struct RefsPage {
    layout: Layout,
    repo: Repo,
    view: RepoView,
    tab: &'static str,
    refs: Vec<RefEntry>,
    /// Branches and tags the source no longer has, with the time it was seen.
    missing: BTreeMap<String, i64>,
}

impl RefsPage {
    fn missing_since(&self, entry: &RefEntry) -> Option<i64> {
        self.missing.get(&entry.name).copied()
    }

    /// The ref kinds in page order, with the message key of their title.
    fn sections(&self) -> [(&'static str, &'static str); 3] {
        [
            ("branch", "browse.branches"),
            ("tag", "browse.tags"),
            ("rescued", "browse.rescued"),
        ]
    }

    fn of_kind(&self, kind: &str) -> Vec<&RefEntry> {
        let kind = RefKind::parse(kind);
        self.refs.iter().filter(|r| Some(r.kind) == kind).collect()
    }

    fn is_default(&self, entry: &RefEntry) -> bool {
        entry.kind == RefKind::Branch
            && self.repo.default_branch.as_deref() == Some(entry.short.as_str())
    }
}

pub async fn refs(ctx: PageContext, Path(id): Path<i64>) -> Result<Response, AppError> {
    let repo = load_repo(&ctx, id).await?;
    let view = load_view(&ctx, &repo, None).await?;
    let refs = if view.dir.join("HEAD").is_file() {
        read::refs(&view.git, &view.dir).await?
    } else {
        Vec::new()
    };
    let page = RefsPage {
        layout: ctx.layout(Nav::Repos),
        repo,
        view,
        tab: "refs",
        refs,
        missing: ref_events::missing(&ctx.state.pool, id).await?,
    };
    Ok(render(&page)?.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(rev: Option<&str>) -> RepoView {
        RepoView {
            id: 3,
            rev: rev.unwrap_or("main").to_owned(),
            commit: None,
            is_default: rev.is_none(),
            dir: PathBuf::new(),
            git: Git::new(READ_TIMEOUT),
        }
    }

    #[test]
    fn links_keep_a_given_revision_and_encode_values() {
        let default = view(None);
        assert_eq!(default.tree_url(""), "/repos/3/tree");
        assert_eq!(
            default.blob_url("a b/c&d.rs"),
            "/repos/3/blob?path=a+b%2Fc%26d.rs"
        );
        assert_eq!(
            view(Some("feature/a-long-branch")).rev_label(),
            "feature/a-long-branch"
        );
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(view(Some(sha)).rev_label(), "0123456789");
        let tagged = view(Some("feature/x"));
        assert_eq!(tagged.commits_url(), "/repos/3/commits?ref=feature%2Fx");
        assert_eq!(
            tagged.crumbs("src/web"),
            vec![
                (
                    "src".to_owned(),
                    "/repos/3/tree?ref=feature%2Fx&path=src".to_owned()
                ),
                (
                    "web".to_owned(),
                    "/repos/3/tree?ref=feature%2Fx&path=src%2Fweb".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn object_ids_and_file_types_are_checked() {
        assert!(is_object_id("0a1b2c3d"));
        assert!(!is_object_id("main"));
        assert!(!is_object_id("abc"));
        assert_eq!(image_type("logo.SVG"), Some("image/svg+xml"));
        assert_eq!(image_type("index.html"), None);
        assert_eq!(encode_filename("a b\"c.bin"), "a%20b%22c.bin");
        assert_eq!(archive_label("proj", "feature/x\"y"), "proj-feature-x-y");
    }
}
