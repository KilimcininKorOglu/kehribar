//! Browsing the archived wiki of a repository: the pages at the wiki's
//! `HEAD`, rendered like the hosts render them. Markdown, AsciiDoc and
//! reStructuredText become sanitised HTML; other formats show as text.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use askama::Template;
use axum::body::Body;
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tokio_util::io::ReaderStream;

use super::browse::{asset_urls, encode_filename, raw_response};
use super::markdown::{self, AssetUrls, LinkTarget};
use super::pages::{Layout, Nav, render};
use super::repos::PageContext;
use crate::db::repos::{self, Repo};
use crate::error::AppError;
use crate::git::{Git, mirror, read};
use crate::repo_url;

/// Reads are local and short; a hung git process must not hold a request.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// The page a wiki opens with, as the hosts name it.
const HOME: &str = "Home";

/// How a page file is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    AsciiDoc,
    Rst,
    /// MediaWiki, Org, Textile and the other formats a host accepts.
    Text,
}

/// The extensions of page files, with their format. Any other file of the
/// wiki, such as an image, is not a page.
const PAGE_EXTENSIONS: &[(&str, Format)] = &[
    ("md", Format::Markdown),
    ("markdown", Format::Markdown),
    ("mdown", Format::Markdown),
    ("mkdn", Format::Markdown),
    ("mkd", Format::Markdown),
    ("adoc", Format::AsciiDoc),
    ("asciidoc", Format::AsciiDoc),
    ("asc", Format::AsciiDoc),
    ("rst", Format::Rst),
    ("rest", Format::Rst),
    ("mediawiki", Format::Text),
    ("wiki", Format::Text),
    ("org", Format::Text),
    ("textile", Format::Text),
    ("rdoc", Format::Text),
    ("creole", Format::Text),
    ("pod", Format::Text),
    ("txt", Format::Text),
];

fn format_of(file_name: &str) -> Option<(&str, Format)> {
    let (stem, ext) = file_name.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    PAGE_EXTENSIONS
        .iter()
        .find_map(|(e, format)| (*e == ext).then_some((stem, *format)))
}

/// Whether `path` names a Markdown page of a wiki.
pub fn is_markdown_page(path: &str) -> bool {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    format_of(file_name).is_some_and(|(_, format)| format == Format::Markdown)
}

/// One page of the wiki.
#[derive(Debug, Clone)]
struct WikiEntry {
    /// The file name without its extension: the page name in links.
    name: String,
    title: String,
    path: String,
    format: Format,
    url: String,
}

/// The URL of the page `name`.
fn page_url(id: i64, name: &str) -> String {
    format!("/repos/{id}/wiki/{}", encode_filename(name))
}

/// Hosts show a page name with spaces where the file name has dashes.
fn title_of(name: &str) -> String {
    name.replace('-', " ")
}

/// The pages among the files of the wiki, sorted by title. Hosts address a
/// page by its file name alone, so a second file with the same name in
/// another directory is left out.
fn pages_of(id: i64, files: &[read::TreeEntry]) -> Vec<WikiEntry> {
    let mut seen = HashSet::new();
    let mut pages: Vec<WikiEntry> = files
        .iter()
        .filter(|file| !file.symlink)
        .filter_map(|file| {
            let file_name = file.path.rsplit('/').next().unwrap_or(&file.path);
            let (stem, format) = format_of(file_name)?;
            seen.insert(stem.to_owned()).then(|| WikiEntry {
                name: stem.to_owned(),
                title: title_of(stem),
                path: file.path.clone(),
                format,
                url: page_url(id, stem),
            })
        })
        .collect();
    pages.sort_by_key(|page| page.title.to_lowercase());
    pages
}

/// The page a link names: exactly, then with spaces as dashes, then
/// without regard to case, as the hosts resolve links.
fn find<'a>(pages: &'a [WikiEntry], name: &str) -> Option<&'a WikiEntry> {
    let wanted = name.trim().replace(' ', "-");
    pages
        .iter()
        .find(|page| page.name == name || page.name.replace(' ', "-") == wanted)
        .or_else(|| {
            pages
                .iter()
                .find(|page| page.name.replace(' ', "-").eq_ignore_ascii_case(&wanted))
        })
}

/// Links inside a wiki page: other pages and files of the wiki, and the
/// archived copies of external images.
struct WikiLinks {
    id: i64,
    /// Paths of the files that are not pages.
    files: HashSet<String>,
    assets: Option<AssetUrls>,
}

impl LinkTarget for WikiLinks {
    fn file_url(&self, path: &str, raw: bool) -> String {
        if raw || self.files.contains(path) {
            return file_url(self.id, path);
        }
        let last = path.rsplit('/').next().unwrap_or(path);
        let name = format_of(last).map_or(last, |(stem, _)| stem);
        page_url(self.id, name)
    }

    fn assets(&self) -> Option<AssetUrls> {
        self.assets.clone()
    }
}

fn file_url(id: i64, path: &str) -> String {
    format!("/repos/{id}/wiki-file?path={}", encode_filename(path))
}

/// Turns `[[Page]]` and `[[Text|Page]]` into Markdown links, outside
/// fenced code blocks.
fn wiki_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        }
        if in_fence {
            out.push_str(line);
        } else {
            push_wiki_links(&mut out, line);
        }
    }
    out
}

fn push_wiki_links(out: &mut String, mut line: &str) {
    while let Some(start) = line.find("[[") {
        let Some(len) = line[start + 2..].find("]]") else {
            break;
        };
        let inner = &line[start + 2..start + 2 + len];
        out.push_str(&line[..start]);
        let (text, target) = inner.split_once('|').unwrap_or((inner, inner));
        let target = target.trim().replace(' ', "-");
        out.push_str(&format!("[{}](<{target}>)", text.trim()));
        line = &line[start + 4 + len..];
    }
    out.push_str(line);
}

/// The HTML of an AsciiDoc page before sanitising. Includes and other file
/// access stay off.
fn asciidoc_html(text: &str) -> Result<String, String> {
    use asciidork_core::{JobSettings, SafeMode};
    use asciidork_parser::prelude::{Bump, Parser, SourceFile};

    let bump = Bump::new();
    let mut parser = Parser::from_str(text, SourceFile::Tmp, &bump);
    let mut settings = JobSettings::embedded();
    settings.safe_mode = SafeMode::Secure;
    settings.strict = false;
    parser.apply_job_settings(settings);
    let parsed = parser
        .parse()
        .map_err(|errors| format!("{} parse errors", errors.len()))?;
    asciidork_dr_html_backend::convert(parsed.document).map_err(|err| err.to_string())
}

/// The HTML of a reStructuredText page before sanitising.
fn rst_html(text: &str) -> Result<String, String> {
    let document = rst_parser::parse(text).map_err(|err| err.to_string())?;
    let mut out = Vec::new();
    rst_renderer::render_html(&document, &mut out, false).map_err(|err| err.to_string())?;
    String::from_utf8(out).map_err(|err| err.to_string())
}

/// Sanitised HTML of a page; `Err` with the reason when its renderer
/// rejects the text.
fn to_html(
    format: Format,
    text: &str,
    base_dir: &str,
    links: &WikiLinks,
) -> Result<String, String> {
    let unsafe_html = match format {
        Format::Markdown => return Ok(markdown::render(&wiki_links(text), base_dir, links)),
        Format::AsciiDoc => asciidoc_html(text)?,
        Format::Rst => rst_html(text)?,
        Format::Text => return Err("not a rendered format".to_owned()),
    };
    Ok(markdown::clean(&unsafe_html, base_dir, links))
}

/// The shown page.
struct Shown {
    title: String,
    path: String,
    /// The rendered page; `None` shows `text`.
    html: Option<String>,
    text: String,
    /// The renderer failed, so the page shows as text.
    failed: bool,
    too_large: bool,
}

#[derive(Template)]
#[template(path = "wiki.html")]
struct WikiPage {
    layout: Layout,
    repo: Repo,
    tab: &'static str,
    pages: Vec<WikiEntry>,
    /// The name of the shown page, to mark it in the list.
    current: String,
    shown: Option<Shown>,
}

impl WikiPage {
    fn is_current(&self, page: &WikiEntry) -> bool {
        page.name == self.current
    }

    fn file_url(&self) -> String {
        self.shown
            .as_ref()
            .map_or_else(String::new, |shown| file_url(self.repo.id, &shown.path))
    }
}

/// The wiki repository and its `HEAD` commit; `None` when it has no commit.
struct Wiki {
    git: Git,
    dir: PathBuf,
    commit: Option<String>,
}

async fn open(ctx: &PageContext, id: i64) -> Result<(Repo, Wiki), AppError> {
    let repo = repos::get(&ctx.state.pool, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if !repo.has_wiki {
        return Err(AppError::NotFound);
    }
    let dir = mirror::wiki_dir(&repo_url::archive_dir(
        &ctx.state.config.data_dir,
        &repo.host,
        &repo.owner,
        &repo.name,
    ));
    let git = Git::new(READ_TIMEOUT);
    let commit = if dir.join("HEAD").is_file() {
        read::resolve_commit(&git, &dir, "HEAD").await?
    } else {
        None
    };
    Ok((repo, Wiki { git, dir, commit }))
}

async fn files_of(wiki: &Wiki) -> Result<Vec<read::TreeEntry>, AppError> {
    match wiki.commit.as_deref() {
        Some(commit) => Ok(read::files(&wiki.git, &wiki.dir, commit).await?),
        None => Ok(Vec::new()),
    }
}

/// The wiki's home page, or the page list when it has none.
pub async fn index(ctx: PageContext, Path(id): Path<i64>) -> Result<Response, AppError> {
    show(ctx, id, HOME, true).await
}

pub async fn page(
    ctx: PageContext,
    Path((id, name)): Path<(i64, String)>,
) -> Result<Response, AppError> {
    show(ctx, id, &name, false).await
}

async fn show(ctx: PageContext, id: i64, name: &str, optional: bool) -> Result<Response, AppError> {
    let (repo, wiki) = open(&ctx, id).await?;
    let files = files_of(&wiki).await?;
    let pages = pages_of(repo.id, &files);
    let found = find(&pages, name).cloned();
    if found.is_none() && !optional {
        return Err(AppError::NotFound);
    }
    let shown = match (&found, wiki.commit.as_deref()) {
        (Some(entry), Some(commit)) => {
            let links = WikiLinks {
                id: repo.id,
                files: files
                    .iter()
                    .filter(|file| format_of(&file.path).is_none())
                    .map(|file| file.path.clone())
                    .collect(),
                assets: Some(asset_urls(&ctx, repo.id).await?),
            };
            Some(load(&wiki, commit, entry, repo.id, links).await?)
        }
        _ => None,
    };
    let page = WikiPage {
        layout: ctx.layout(Nav::Repos),
        repo,
        tab: "wiki",
        pages,
        current: found.map(|entry| entry.name).unwrap_or_default(),
        shown,
    };
    Ok(render(&page)?.into_response())
}

/// Reads and renders one page. Rendering runs on a blocking thread, and a
/// renderer that panics on the text leaves the page as text.
async fn load(
    wiki: &Wiki,
    commit: &str,
    entry: &WikiEntry,
    repo_id: i64,
    links: WikiLinks,
) -> Result<Shown, AppError> {
    let blob = read::blob(
        &wiki.git,
        &wiki.dir,
        commit,
        &entry.path,
        read::MAX_BLOB_BYTES,
    )
    .await?;
    let mut shown = Shown {
        title: entry.title.clone(),
        path: entry.path.clone(),
        html: None,
        text: String::new(),
        failed: false,
        too_large: blob.too_large,
    };
    if blob.too_large {
        return Ok(shown);
    }
    shown.text = String::from_utf8_lossy(&blob.data).into_owned();
    if entry.format == Format::Text {
        return Ok(shown);
    }
    let base_dir = entry
        .path
        .rsplit_once('/')
        .map_or("", |(dir, _)| dir)
        .to_owned();
    let (format, text) = (entry.format, shown.text.clone());
    let rendered =
        tokio::task::spawn_blocking(move || to_html(format, &text, &base_dir, &links)).await;
    match rendered {
        Ok(Ok(html)) => shown.html = Some(html),
        Ok(Err(reason)) => {
            tracing::warn!(repo = repo_id,path = %entry.path, %reason, "wiki page not rendered");
            shown.failed = true;
        }
        Err(err) => {
            tracing::warn!(repo = repo_id,path = %entry.path, %err, "wiki renderer panicked");
            shown.failed = true;
        }
    }
    Ok(shown)
}

#[derive(Debug, Deserialize)]
pub struct FileQuery {
    path: Option<String>,
}

/// A file of the wiki as it is, such as an image a page shows.
pub async fn file(
    ctx: PageContext,
    Path(id): Path<i64>,
    Query(query): Query<FileQuery>,
) -> Result<Response, AppError> {
    let (_, wiki) = open(&ctx, id).await?;
    let path = read::clean_path(query.path.as_deref().unwrap_or("")).ok_or(AppError::NotFound)?;
    let commit = wiki.commit.as_deref().ok_or(AppError::NotFound)?;
    if path.is_empty()
        || read::object_kind(&wiki.git, &wiki.dir, commit, &path).await?
            != Some(read::EntryKind::Blob)
    {
        return Err(AppError::NotFound);
    }
    let head = read::peek(&wiki.git, &wiki.dir, commit, &path, 8000).await?;
    let spec = format!("{commit}:{path}");
    let stdout = wiki
        .git
        .spawn_stdout(&wiki.dir, &["cat-file", "blob", &spec])?;
    Ok(raw_response(
        Body::from_stream(ReaderStream::new(stdout)),
        &head,
        &path,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> read::TreeEntry {
        read::TreeEntry {
            kind: read::EntryKind::Blob,
            name: path.to_owned(),
            path: path.to_owned(),
            size: Some(1),
            symlink: false,
        }
    }

    #[test]
    fn pages_are_the_files_with_a_page_extension() {
        let files = [
            entry("Home.md"),
            entry("Getting-Started.adoc"),
            entry("images/logo.png"),
            entry("docs/Home.rst"),
            entry("old/Notes.mediawiki"),
        ];
        let pages = pages_of(7, &files);
        let shape: Vec<_> = pages
            .iter()
            .map(|p| (p.title.as_str(), p.path.as_str(), p.format))
            .collect();
        assert_eq!(
            shape,
            [
                ("Getting Started", "Getting-Started.adoc", Format::AsciiDoc),
                ("Home", "Home.md", Format::Markdown),
                ("Notes", "old/Notes.mediawiki", Format::Text),
            ]
        );
        assert_eq!(pages[0].url, "/repos/7/wiki/Getting-Started");
        assert_eq!(
            find(&pages, "getting started").map(|p| p.path.as_str()),
            Some("Getting-Started.adoc")
        );
        assert!(find(&pages, "Missing").is_none());
    }

    #[test]
    fn wiki_links_become_markdown_links_outside_code() {
        let text = "See [[Getting Started]] and [[the notes|Notes]].\n```\n[[Kept]]\n```\n[[open";
        assert_eq!(
            wiki_links(text),
            "See [Getting Started](<Getting-Started>) and [the notes](<Notes>).\n```\n[[Kept]]\n```\n[[open"
        );
    }

    #[test]
    fn links_point_to_pages_or_files() {
        let links = WikiLinks {
            id: 3,
            files: HashSet::from(["images/logo.png".to_owned(), "guide.pdf".to_owned()]),
            assets: None,
        };
        assert_eq!(
            links.file_url("Other-Page", false),
            "/repos/3/wiki/Other-Page"
        );
        assert_eq!(links.file_url("sub/Setup.md", false), "/repos/3/wiki/Setup");
        assert_eq!(
            links.file_url("Release-1.2", false),
            "/repos/3/wiki/Release-1.2"
        );
        assert_eq!(
            links.file_url("guide.pdf", false),
            "/repos/3/wiki-file?path=guide.pdf"
        );
        assert_eq!(
            links.file_url("images/logo.png", true),
            "/repos/3/wiki-file?path=images%2Flogo.png"
        );
    }

    #[test]
    fn asciidoc_and_rst_render_without_scripts() {
        let links = WikiLinks {
            id: 1,
            files: HashSet::new(),
            assets: None,
        };
        let adoc = "= Title\n\n== Part\n\nSome *bold* text.\n\n++++\n<script>alert(1)</script>\n++++\n\nlink:Other[next]\n";
        let html = to_html(Format::AsciiDoc, adoc, "", &links).unwrap();
        assert!(html.contains("<strong>bold</strong>"), "{html}");
        assert!(html.contains("href=\"/repos/1/wiki/Other\""), "{html}");
        assert!(!html.contains("<script"), "{html}");

        let rst = "Title\n=====\n\nSome **bold** text and `a link <Other>`_.\n\n.. raw:: html\n\n   <img src=x onerror=alert(1)>\n";
        let html = to_html(Format::Rst, rst, "", &links).unwrap();
        assert!(html.contains("<strong>bold</strong>"), "{html}");
        assert!(!html.contains("onerror"), "{html}");
    }
}
