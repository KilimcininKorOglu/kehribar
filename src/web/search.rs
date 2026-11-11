//! Code search in one repository: `git grep` over one revision.

use std::time::Duration;

use askama::Template;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use url::form_urlencoded;

use super::form::is_checked;
use super::markdown;
use super::pages::{Layout, Nav, render};
use super::repos::PageContext;
use crate::db::repos::{self, Repo};
use crate::error::AppError;
use crate::git::read::{self, GrepMatch, GrepOptions};
use crate::git::{Git, GitErrorKind};
use crate::repo_url;

/// Matching lines shown at most; the page says when there are more.
pub const MAX_MATCHES: usize = 500;
/// Shorter queries match too much to be useful.
pub const MIN_QUERY_CHARS: usize = 2;
/// A search reads every file of a revision; a hung git process must not
/// hold a request longer than this.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    q: Option<String>,
    #[serde(rename = "ref")]
    rev: Option<String>,
    regex: Option<String>,
    case: Option<String>,
}

/// The matches of one file.
struct FileMatches {
    path: String,
    matches: Vec<GrepMatch>,
}

#[derive(Template)]
#[template(path = "search.html")]
struct SearchPage {
    layout: Layout,
    repo: Repo,
    tab: &'static str,
    query: String,
    /// The revision as given; empty for the default branch.
    rev: String,
    regex: bool,
    case_sensitive: bool,
    /// `None` when no search ran.
    files: Option<Vec<FileMatches>>,
    total: usize,
    cut: bool,
    /// The message key of a search that could not run.
    problem: Option<&'static str>,
}

impl SearchPage {
    /// The file page at the line of a match, in the searched revision. A
    /// Markdown file opens as source, where the lines are numbered.
    fn line_url(&self, path: &str, line: &u64) -> String {
        let mut pairs = form_urlencoded::Serializer::new(String::new());
        if !self.rev.is_empty() {
            pairs.append_pair("ref", &self.rev);
        }
        pairs.append_pair("path", path);
        if markdown::is_markdown(path) {
            pairs.append_pair("view", "source");
        }
        format!("/repos/{}/blob?{}#L{line}", self.repo.id, pairs.finish())
    }

    fn summary(&self) -> String {
        let total = i64::try_from(self.total).unwrap_or(i64::MAX);
        let files = self.files.as_ref().map_or(0, Vec::len);
        self.layout.tf(
            "search.summary",
            &[
                ("matches", &self.layout.tf_number("search.matches", &total)),
                (
                    "files",
                    &self.layout.tf_number("search.files", &(files as i64)),
                ),
            ],
        )
    }

    fn cut_text(&self) -> String {
        self.layout
            .tf("search.cut", &[("n", &MAX_MATCHES.to_string())])
    }

    fn min_text(&self) -> String {
        self.layout
            .tf("search.too_short", &[("n", &MIN_QUERY_CHARS.to_string())])
    }
}

/// Puts the matches of each file together, in the order git found them.
fn by_file(matches: Vec<GrepMatch>) -> Vec<FileMatches> {
    let mut files: Vec<FileMatches> = Vec::new();
    for found in matches {
        match files.last_mut() {
            Some(file) if file.path == found.path => file.matches.push(found),
            _ => files.push(FileMatches {
                path: found.path.clone(),
                matches: vec![found],
            }),
        }
    }
    files
}

pub async fn show(
    ctx: PageContext,
    Path(id): Path<i64>,
    Query(query): Query<SearchQuery>,
) -> Result<Response, AppError> {
    let repo = repos::get(&ctx.state.pool, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut page = SearchPage {
        layout: ctx.layout(Nav::Repos),
        tab: "search",
        query: query.q.unwrap_or_default(),
        rev: query.rev.map(|r| r.trim().to_owned()).unwrap_or_default(),
        regex: is_checked(query.regex.as_deref()),
        case_sensitive: is_checked(query.case.as_deref()),
        files: None,
        total: 0,
        cut: false,
        problem: None,
        repo,
    };
    // git takes no NUL byte in an argument.
    let status = if page.query.contains('\0') {
        page.problem = Some("search.nul_byte");
        StatusCode::BAD_REQUEST
    } else if page.query.trim().chars().count() >= MIN_QUERY_CHARS {
        search(&ctx, &mut page).await?
    } else {
        StatusCode::OK
    };
    Ok((status, render(&page)?).into_response())
}

/// Runs the search of `page` and fills in its result or its problem.
async fn search(ctx: &PageContext, page: &mut SearchPage) -> Result<StatusCode, AppError> {
    let repo = &page.repo;
    let dir = repo_url::archive_dir(
        &ctx.state.config.data_dir,
        &repo.host,
        &repo.owner,
        &repo.name,
    );
    if !dir.join("HEAD").is_file() {
        page.problem = Some("browse.empty");
        return Ok(StatusCode::OK);
    }
    let git = Git::new(SEARCH_TIMEOUT);
    let rev = match page.rev.as_str() {
        "" => repo.default_branch.as_deref().unwrap_or("HEAD"),
        given => given,
    };
    let commit = if read::valid_rev(rev) {
        read::resolve_commit(&git, &dir, rev).await?
    } else {
        None
    };
    let Some(commit) = commit else {
        page.problem = Some("search.unknown_rev");
        return Ok(StatusCode::NOT_FOUND);
    };
    let options = GrepOptions {
        pattern: page.query.trim(),
        regex: page.regex,
        ignore_case: !page.case_sensitive,
    };
    match read::grep(&git, &dir, &commit, options, MAX_MATCHES).await {
        Ok((matches, cut)) => {
            page.total = matches.len();
            page.cut = cut;
            page.files = Some(by_file(matches));
            Ok(StatusCode::OK)
        }
        Err(err) if err.kind == GitErrorKind::Timeout => {
            page.problem = Some("search.timeout");
            Ok(StatusCode::OK)
        }
        Err(err) if page.regex && err.exit_code == Some(128) => {
            page.problem = Some("search.bad_regex");
            Ok(StatusCode::UNPROCESSABLE_ENTITY)
        }
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(path: &str, line: u64) -> GrepMatch {
        GrepMatch {
            path: path.to_owned(),
            line,
            text: String::new(),
        }
    }

    #[test]
    fn matches_are_grouped_by_file_in_order() {
        let files = by_file(vec![found("a", 1), found("a", 5), found("b", 2)]);
        let shape: Vec<_> = files
            .iter()
            .map(|f| {
                (
                    f.path.as_str(),
                    f.matches.iter().map(|m| m.line).collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(shape, [("a", vec![1, 5]), ("b", vec![2])]);
    }
}
