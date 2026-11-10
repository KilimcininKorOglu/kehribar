//! One sync of one repository: fetch, record the outcome, plan the next run.

use std::path::Path;
use std::time::Duration;

use super::{SyncContext, credentials};
use crate::db::hosts::{self, Host, Provider};
use crate::db::repos::{self, Repo, RepoStatus, SyncOutcome};
use crate::db::sync_runs::{self, RunResult};
use crate::db::unix_now;
use crate::git::mirror::{self, SyncReport};
use crate::git::{Credentials, Git, GitError, GitErrorKind};
use crate::settings::SyncSettings;

/// The result of the Git work of one sync.
#[derive(Debug, Default)]
struct Mirrored {
    report: SyncReport,
    has_wiki: bool,
}

/// A failed sync: the status to show and the message.
#[derive(Debug)]
struct Failure {
    status: RepoStatus,
    message: String,
}

impl From<GitError> for Failure {
    fn from(err: GitError) -> Self {
        let status = if err.kind == GitErrorKind::NotFound {
            RepoStatus::Unreachable
        } else {
            RepoStatus::Error
        };
        Self {
            status,
            message: err.message,
        }
    }
}

/// Syncs a repository that the caller has claimed and records the result.
pub async fn run(ctx: &SyncContext, repo: &Repo, settings: SyncSettings) -> sqlx::Result<()> {
    let started = unix_now();
    let run_id = sync_runs::start(&ctx.pool, repo.id, started).await?;
    tracing::info!(repo = %repo.url, "sync started");

    let git = Git::new(Duration::from_secs(
        u64::try_from(settings.git_timeout_seconds).unwrap_or(u64::MAX),
    ));
    let dir = ctx.repo_dir(repo);
    let host = hosts::find_by_host(&ctx.pool, &repo.host).await?;
    let job = Job {
        git: &git,
        repo,
        host: host.as_ref(),
        dir: &dir,
        now: started,
    };
    let result = mirror_repo(ctx, &job).await;
    let finished = unix_now();
    let interval = repo.interval_minutes.unwrap_or(settings.interval_minutes);
    let size_bytes = archive_size(&dir).await;

    let (status, message, mirrored) = match result {
        Ok(mirrored) => (RepoStatus::Ok, None, mirrored),
        Err(failure) => (failure.status, Some(failure.message), Mirrored::default()),
    };
    log_outcome(repo, status, message.as_deref(), &mirrored.report);
    let outcome = SyncOutcome {
        status,
        finished_at: finished,
        next_sync_at: finished + interval * 60,
        error: message.as_deref(),
        size_bytes,
        default_branch: mirrored.report.default_branch.as_deref(),
        has_wiki: mirrored.has_wiki || (status != RepoStatus::Ok && repo.has_wiki),
    };
    repos::finish(&ctx.pool, repo.id, &outcome).await?;
    let run = RunResult {
        finished_at: finished,
        outcome: status,
        refs_changed: i64::try_from(mirrored.report.refs_changed).unwrap_or(i64::MAX),
        refs_rescued: i64::try_from(mirrored.report.refs_rescued).unwrap_or(i64::MAX),
        message: message.as_deref(),
    };
    sync_runs::finish(&ctx.pool, run_id, &run).await
}

/// The inputs of the Git work of one sync.
struct Job<'a> {
    git: &'a Git,
    repo: &'a Repo,
    host: Option<&'a Host>,
    dir: &'a Path,
    now: i64,
}

impl Job<'_> {
    /// Bitbucket serves the wiki under `<repo>.git/wiki`; every other
    /// server under `<repo>.wiki.git`.
    fn is_bitbucket(&self) -> bool {
        self.host.is_some_and(|h| h.provider == Provider::Bitbucket)
            || self.repo.host.eq_ignore_ascii_case("bitbucket.org")
    }
}

async fn mirror_repo(ctx: &SyncContext, job: &Job<'_>) -> Result<Mirrored, Failure> {
    let credentials = credentials::resolve(&ctx.cipher, job.repo, job.host)
        .map_err(|message| Failure {
            status: RepoStatus::Error,
            message,
        })?
        .map(|c| credentials::for_git(c, job.is_bitbucket()));
    let credentials = credentials.as_ref();
    let report = mirror::sync(job.git, &job.repo.url, job.dir, credentials, job.now).await?;
    let has_wiki = if job.repo.wiki_enabled {
        mirror_wiki(job, credentials).await?
    } else {
        false
    };
    Ok(Mirrored { report, has_wiki })
}

async fn mirror_wiki(job: &Job<'_>, credentials: Option<&Credentials>) -> Result<bool, Failure> {
    let url = mirror::wiki_url(&job.repo.url, job.is_bitbucket());
    let wiki_dir = mirror::wiki_dir(job.dir);
    let wiki = mirror::sync_wiki(job.git, &url, &wiki_dir, credentials, job.now)
        .await
        .map_err(|err| Failure {
            // The repository itself is fine; only the wiki failed.
            status: RepoStatus::Error,
            message: format!("wiki: {}", err.message),
        })?;
    Ok(wiki.is_some())
}

/// Size of the repository and its wiki. A missing directory counts as zero.
async fn archive_size(dir: &Path) -> i64 {
    let mut total = 0u64;
    for path in [dir.to_owned(), mirror::wiki_dir(dir)] {
        match mirror::disk_usage(&path).await {
            Ok(bytes) => total += bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(path = %path.display(), %err, "cannot measure archive size"),
        }
    }
    i64::try_from(total).unwrap_or(i64::MAX)
}

fn log_outcome(repo: &Repo, status: RepoStatus, message: Option<&str>, report: &SyncReport) {
    match message {
        None => tracing::info!(
            repo = %repo.url,
            changed = report.refs_changed,
            rescued = report.refs_rescued,
            "sync finished"
        ),
        Some(message) => tracing::warn!(
            repo = %repo.url,
            status = status.code(),
            %message,
            "sync failed"
        ),
    }
}
