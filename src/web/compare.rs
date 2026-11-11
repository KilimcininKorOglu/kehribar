//! Comparing two revisions of a repository: the commits `head` has that
//! `base` lacks, and the diff `head` adds since their merge base.

use std::path::Path as FsPath;
use std::time::Duration;

use askama::Template;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::pages::{Layout, Nav, render};
use super::repos::PageContext;
use crate::db::repos::{self, Repo};
use crate::error::AppError;
use crate::git::Git;
use crate::git::read::{self, CommitSummary, FileChange, FileDiff, RefEntry, RefKind};
use crate::repo_url;

/// Commits listed at most; the page says when there are more.
pub const MAX_COMMITS: usize = 250;
/// Reads are local; a hung git process must not hold a request.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize)]
pub struct CompareQuery {
    base: Option<String>,
    head: Option<String>,
}

#[derive(Template)]
#[template(path = "compare.html")]
struct ComparePage {
    layout: Layout,
    repo: Repo,
    tab: &'static str,
    base: String,
    head: String,
    /// Branch and tag names for the input suggestions.
    names: Vec<String>,
    /// The revisions that name no commit of the archive.
    unknown: Vec<String>,
    result: Option<Comparison>,
}

/// What `head` adds to `base`.
struct Comparison {
    /// Both revisions name the same commit.
    same: bool,
    /// Commits in `head` that `base` lacks, newest first, at most [`MAX_COMMITS`].
    commits: Vec<CommitSummary>,
    /// All such commits, also the ones not listed.
    total: i64,
    files: Vec<FileDiff>,
    diff_cut: bool,
}

impl ComparePage {
    fn unknown_text(&self) -> String {
        self.layout
            .tf("compare.unknown", &[("revs", &self.unknown.join(", "))])
    }

    fn swap_url(&self) -> String {
        let mut pairs = url::form_urlencoded::Serializer::new(String::new());
        pairs.append_pair("base", &self.head);
        pairs.append_pair("head", &self.base);
        format!("/repos/{}/compare?{}", self.repo.id, pairs.finish())
    }

    fn short<'a>(&self, sha: &'a str) -> &'a str {
        sha.get(..10).unwrap_or(sha)
    }
}

impl Comparison {
    fn is_cut(&self) -> bool {
        usize::try_from(self.total).unwrap_or(usize::MAX) > self.commits.len()
    }
}

impl ComparePage {
    fn commits_text(&self, result: &Comparison) -> String {
        self.layout.tf_number("compare.commits", &result.total)
    }

    fn cut_text(&self, result: &Comparison) -> String {
        self.layout.tf(
            "compare.commits_cut",
            &[("shown", &result.commits.len().to_string())],
        )
    }

    fn files_text(&self, result: &Comparison) -> String {
        let count = i64::try_from(result.files.len()).unwrap_or(i64::MAX);
        self.layout.tf_number("browse.files_changed", &count)
    }
}

/// The two revisions, trimmed; `base` falls back to the default branch.
fn revisions(repo: &Repo, query: CompareQuery) -> (String, String) {
    let trimmed =
        |value: Option<String>| value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let base = trimmed(query.base)
        .or_else(|| repo.default_branch.clone())
        .unwrap_or_else(|| "HEAD".to_owned());
    (base, trimmed(query.head).unwrap_or_default())
}

/// The commit `rev` names, or `None` for a name that is unsafe or unknown.
async fn resolve(git: &Git, dir: &FsPath, rev: &str) -> Result<Option<String>, AppError> {
    if !read::valid_rev(rev) {
        return Ok(None);
    }
    Ok(read::resolve_commit(git, dir, rev).await?)
}

async fn compare(git: &Git, dir: &FsPath, base: &str, head: &str) -> Result<Comparison, AppError> {
    if base == head {
        return Ok(Comparison {
            same: true,
            commits: Vec::new(),
            total: 0,
            files: Vec::new(),
            diff_cut: false,
        });
    }
    let range = format!("{base}..{head}");
    let commits = read::log(git, dir, &range, None, 0, MAX_COMMITS).await?;
    let total = read::count_ahead(git, dir, base, head).await?;
    let (files, diff_cut) = read::compare_diff(git, dir, base, head).await?;
    Ok(Comparison {
        same: false,
        commits,
        total,
        files,
        diff_cut,
    })
}

/// Branch names, then tag names, for the input suggestions.
fn ref_names(refs: Vec<RefEntry>) -> Vec<String> {
    refs.into_iter()
        .filter(|r| matches!(r.kind, RefKind::Branch | RefKind::Tag))
        .map(|r| r.short)
        .collect()
}

pub async fn show(
    ctx: PageContext,
    Path(id): Path<i64>,
    Query(query): Query<CompareQuery>,
) -> Result<Response, AppError> {
    let repo = repos::get(&ctx.state.pool, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let dir = repo_url::archive_dir(
        &ctx.state.config.data_dir,
        &repo.host,
        &repo.owner,
        &repo.name,
    );
    let git = Git::new(READ_TIMEOUT);
    let archived = dir.join("HEAD").is_file();
    let (base, head) = revisions(&repo, query);
    let mut page = ComparePage {
        layout: ctx.layout(Nav::Repos),
        names: if archived {
            ref_names(read::refs(&git, &dir).await?)
        } else {
            Vec::new()
        },
        repo,
        tab: "compare",
        base,
        head,
        unknown: Vec::new(),
        result: None,
    };
    let status = if archived && !page.head.is_empty() {
        fill(&mut page, &git, &dir).await?
    } else {
        StatusCode::OK
    };
    Ok((status, render(&page)?).into_response())
}

/// Compares the two revisions of `page`, or lists the ones that name no
/// commit and answers 404.
async fn fill(page: &mut ComparePage, git: &Git, dir: &FsPath) -> Result<StatusCode, AppError> {
    let base_commit = resolve(git, dir, &page.base).await?;
    let head_commit = resolve(git, dir, &page.head).await?;
    let (Some(base_commit), Some(head_commit)) = (base_commit.as_ref(), head_commit.as_ref())
    else {
        page.unknown = [(&page.base, &base_commit), (&page.head, &head_commit)]
            .into_iter()
            .filter(|(_, commit)| commit.is_none())
            .map(|(name, _)| name.clone())
            .collect();
        return Ok(StatusCode::NOT_FOUND);
    };
    page.result = Some(compare(git, dir, base_commit, head_commit).await?);
    Ok(StatusCode::OK)
}
