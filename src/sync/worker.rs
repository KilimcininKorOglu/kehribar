//! One sync of one repository: fetch, record the outcome, plan the next run.

use std::path::Path;
use std::time::Duration;

use super::{SyncContext, credentials, ref_log};
use crate::db::hosts::{self, Host, Provider};
use crate::db::repos::{self, Repo, RepoStatus, SyncOutcome};
use crate::db::sync_runs::{self, RunOutcome, RunResult};
use crate::db::unix_now;
use crate::git::mirror::{self, SyncReport};
use crate::git::ssh::KeyFile;
use crate::git::{Access, Git, GitError, GitErrorKind};
use crate::git::{lfs, maintenance};
use crate::notify::{self, Change, Event};
use crate::providers::RATE_LIMIT_WAIT_SECONDS;
use crate::ref_filter;
use crate::repo_url;
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
    /// A rate limit stopped the sync: it is postponed, not failed.
    deferred: bool,
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
            deferred: err.kind == GitErrorKind::RateLimited,
            message: err.message,
        }
    }
}

impl Failure {
    fn error(message: String) -> Self {
        Self {
            status: RepoStatus::Error,
            message,
            deferred: false,
        }
    }
}

/// Syncs a repository that the caller has claimed and records the result.
pub async fn run(ctx: &SyncContext, repo: &Repo, settings: SyncSettings) -> sqlx::Result<()> {
    // A backup may be reading the repository; the sync waits for it.
    let _lock = ctx.locks.lock(repo.id).await;
    let started = unix_now();
    let run_id = sync_runs::start(&ctx.pool, repo.id, started).await?;
    tracing::info!(repo = %repo.url, "sync started");

    let git = Git::new(Duration::from_secs(
        u64::try_from(settings.git_timeout_seconds).unwrap_or(u64::MAX),
    ));
    let dir = ctx.repo_dir(repo);
    let host = hosts::find_by_host(&ctx.pool, repo_url::settings_host(&repo.host)).await?;
    let job = Job {
        git: &git,
        repo,
        host: host.as_ref(),
        dir: &dir,
        now: started,
        run_id,
    };
    let (status, message, mirrored) = match sync_and_maintain(ctx, &job, settings).await? {
        Ok(done) => done,
        Err(message) => return defer(ctx, repo, run_id, &message).await,
    };
    let finished = unix_now();
    let interval = repo.interval_minutes.unwrap_or(settings.interval_minutes);
    let size_bytes = archive_size(&dir).await;
    let lfs_size_bytes = lfs_size(&dir).await;

    log_outcome(repo, status, message.as_deref(), &mirrored.report);
    let outcome = SyncOutcome {
        status,
        finished_at: finished,
        next_sync_at: finished + interval * 60,
        error: message.as_deref(),
        size_bytes,
        lfs_size_bytes,
        default_branch: mirrored.report.default_branch.as_deref(),
        has_wiki: mirrored.has_wiki || (status != RepoStatus::Ok && repo.has_wiki),
    };
    repos::finish(&ctx.pool, repo.id, &outcome).await?;
    report_change(ctx, repo, status, message.as_deref()).await;
    let run = RunResult {
        finished_at: finished,
        outcome: status.into(),
        refs_changed: i64::try_from(mirrored.report.refs_changed).unwrap_or(i64::MAX),
        refs_rescued: i64::try_from(mirrored.report.refs_rescued).unwrap_or(i64::MAX),
        message: message.as_deref(),
    };
    sync_runs::finish(&ctx.pool, run_id, &run).await
}

/// Records a sync that a rate limit stopped: the repository keeps its state
/// and tries again after [`RATE_LIMIT_WAIT_SECONDS`], without a notification;
/// git does not pass on the server's own retry time.
async fn defer(ctx: &SyncContext, repo: &Repo, run_id: i64, message: &str) -> sqlx::Result<()> {
    let finished = unix_now();
    tracing::info!(repo = %repo.url, %message, "sync postponed by a rate limit");
    repos::defer(
        &ctx.pool,
        repo.id,
        repo.status,
        finished + RATE_LIMIT_WAIT_SECONDS,
    )
    .await?;
    let run = RunResult {
        finished_at: finished,
        outcome: RunOutcome::Deferred,
        refs_changed: 0,
        refs_rescued: 0,
        message: Some(message),
    };
    sync_runs::finish(&ctx.pool, run_id, &run).await
}

/// Notifies when the repository starts failing or recovers. `repo.status`
/// is the state before this sync.
async fn report_change(ctx: &SyncContext, repo: &Repo, status: RepoStatus, message: Option<&str>) {
    let previous = repo.status;
    let event = match notify::repo_change(previous, status) {
        Some(Change::Failed) => Event::RepoFailed {
            repo,
            previous,
            status,
            message: message.unwrap_or_default(),
        },
        Some(Change::Recovered) => Event::RepoRecovered { repo, previous },
        None => return,
    };
    notify::send(ctx, event).await;
}

/// The status, the message and the Git work of a sync that ran to its end.
type Synced = (RepoStatus, Option<String>, Mirrored);

/// Fetches the repository, then runs the maintenance that is due. A failed
/// integrity check fails the sync, but keeps the fetch report. The inner
/// `Err` is the message of a sync that a rate limit stopped.
async fn sync_and_maintain(
    ctx: &SyncContext,
    job: &Job<'_>,
    settings: SyncSettings,
) -> sqlx::Result<Result<Synced, String>> {
    let mirrored = match mirror_repo(ctx, job).await {
        Ok(mirrored) => mirrored,
        Err(failure) if failure.deferred => return Ok(Err(failure.message)),
        Err(failure) => {
            return Ok(Ok((
                failure.status,
                Some(failure.message),
                Mirrored::default(),
            )));
        }
    };
    let upkeep = maintain(job, settings).await;
    upkeep.record(&ctx.pool, job.repo.id, job.now).await?;
    Ok(Ok(match upkeep.failure() {
        Some(message) => (RepoStatus::Error, Some(message), mirrored),
        None => (RepoStatus::Ok, None, mirrored),
    }))
}

/// What the maintenance of one sync did; `None` means a task was not due.
#[derive(Debug, Default)]
struct Upkeep {
    fsck: Option<Result<(), String>>,
    repack: Option<Result<i64, String>>,
}

impl Upkeep {
    async fn record(&self, pool: &sqlx::SqlitePool, id: i64, now: i64) -> sqlx::Result<()> {
        if let Some(result) = &self.fsck {
            repos::record_fsck(pool, id, now, result.as_ref().err().map(String::as_str)).await?;
        }
        if let Some(Ok(saved)) = self.repack {
            repos::record_repack(pool, id, now, saved).await?;
        }
        Ok(())
    }

    /// The message of a failed task. Git's own errors stay in English, like
    /// the fetch errors next to them.
    fn failure(&self) -> Option<String> {
        if let Some(Err(err)) = &self.fsck {
            return Some(format!("integrity check failed: {err}"));
        }
        if let Some(Err(err)) = &self.repack {
            return Some(format!("repack failed: {err}"));
        }
        None
    }
}

/// Whether a task that last ran at `last` is due again after `days` days.
/// Zero days turns the task off.
fn is_due(last: Option<i64>, days: i64, now: i64) -> bool {
    days > 0 && last.is_none_or(|at| now - at >= days * 86_400)
}

/// Runs the integrity check and the repack when they are due. A failed
/// check repeats on every sync until it passes; a damaged repository is
/// not repacked.
async fn maintain(job: &Job<'_>, settings: SyncSettings) -> Upkeep {
    let repo = job.repo;
    let mut upkeep = Upkeep::default();
    let fsck_days = settings.fsck_interval_days;
    if (fsck_days > 0 && repo.fsck_error.is_some()) || is_due(repo.last_fsck_at, fsck_days, job.now)
    {
        let check = async |git: &Git, dir: &Path| maintenance::fsck(git, dir).await.map(|()| 0);
        upkeep.fsck = Some(for_each_dir(job, check).await.map(|_| ()));
    }
    let damaged = matches!(upkeep.fsck, Some(Err(_)));
    if !damaged && is_due(repo.last_repack_at, settings.repack_interval_days, job.now) {
        upkeep.repack = Some(for_each_dir(job, maintenance::repack).await);
    }
    upkeep
}

/// Runs `task` on the repository and on its wiki when there is one, and
/// adds up the numbers it returns. The first error stops the run.
async fn for_each_dir<F>(job: &Job<'_>, task: F) -> Result<i64, String>
where
    F: AsyncFn(&Git, &Path) -> Result<i64, GitError>,
{
    let wiki = mirror::wiki_dir(job.dir);
    let mut total = 0;
    for dir in [job.dir, wiki.as_path()] {
        if !dir.join("HEAD").is_file() {
            continue;
        }
        total += task(job.git, dir).await.map_err(|err| err.message)?;
    }
    Ok(total)
}

/// The inputs of the Git work of one sync.
struct Job<'a> {
    git: &'a Git,
    repo: &'a Repo,
    host: Option<&'a Host>,
    dir: &'a Path,
    now: i64,
    run_id: i64,
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
    // The key file must live until the last git command of the sync.
    let (access, _key_file) = access_for(ctx, job)?;
    let access = access.as_ref();
    let exclude = ref_filter::combine(
        job.host.and_then(|h| h.ref_exclude.as_deref()),
        job.repo.ref_exclude.as_deref(),
    );
    let report = mirror::sync(job.git, &job.repo.url, job.dir, access, job.now, &exclude).await?;
    // Written before the wiki and LFS fetch: their failure must not lose
    // the history of a fetch that already happened.
    let fetch = ref_log::Fetch {
        repo_id: job.repo.id,
        run_id: job.run_id,
        now: job.now,
        report: &report,
    };
    ref_log::record(&ctx.pool, job.git, job.dir, &exclude, &fetch)
        .await
        .map_err(|err| Failure::error(format!("cannot record the ref history: {err}")))?;
    let has_wiki = if job.repo.wiki_enabled {
        mirror_wiki(job, access).await?
    } else {
        false
    };
    if job.repo.lfs_enabled {
        lfs::fetch(job.git, job.dir, access)
            .await
            .map_err(|err| part_failure("LFS", err))?;
    }
    Ok(Mirrored { report, has_wiki })
}

/// A failure of the wiki or of the LFS objects: the repository itself was
/// fetched, so the source is reachable; a rate limit still postpones.
fn part_failure(part: &str, err: GitError) -> Failure {
    if err.kind == GitErrorKind::RateLimited {
        return Failure::from(err);
    }
    Failure::error(format!("{part}: {}", err.message))
}

/// What the fetch proves itself with: for an `ssh://` URL the SSH key of
/// the host, written to a private file for the time of the sync; else the
/// repository or host token.
fn access_for(
    ctx: &SyncContext,
    job: &Job<'_>,
) -> Result<(Option<Access>, Option<KeyFile>), Failure> {
    if !repo_url::is_ssh(&job.repo.url) {
        let credentials = credentials::resolve(&ctx.cipher, job.repo, job.host)
            .map_err(Failure::error)?
            .map(|c| credentials::for_git(c, job.is_bitbucket()));
        return Ok((credentials.map(Access::Token), None));
    }
    let key_file = credentials::ssh_key(&ctx.cipher, job.host)
        .map_err(Failure::error)?
        .map(|key| KeyFile::write(&ctx.config.tmp_dir(), &key))
        .transpose()
        .map_err(|err| Failure::error(format!("cannot write the SSH key: {err}")))?;
    let known_hosts = ctx.config.known_hosts();
    if let Some(dir) = known_hosts.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|err| Failure::error(format!("cannot create {}: {err}", dir.display())))?;
    }
    let access = Access::Ssh {
        key_file: key_file.as_ref().map(|file| file.path().to_owned()),
        known_hosts,
    };
    Ok((Some(access), key_file))
}

async fn mirror_wiki(job: &Job<'_>, access: Option<&Access>) -> Result<bool, Failure> {
    let url = mirror::wiki_url(&job.repo.url, job.is_bitbucket());
    let wiki_dir = mirror::wiki_dir(job.dir);
    let wiki = mirror::sync_wiki(job.git, &url, &wiki_dir, access, job.now)
        .await
        .map_err(|err| part_failure("wiki", err))?;
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

/// Bytes of the LFS objects; also counted when LFS was turned off later.
async fn lfs_size(dir: &Path) -> i64 {
    match lfs::objects_size(dir).await {
        Ok(bytes) => i64::try_from(bytes).unwrap_or(i64::MAX),
        Err(err) => {
            tracing::warn!(dir = %dir.display(), %err, "cannot measure the LFS objects");
            0
        }
    }
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
