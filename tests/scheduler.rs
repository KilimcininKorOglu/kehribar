//! Scheduling and recording of syncs against local source repositories.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use git_archive::crypto::Cipher;
use git_archive::db::repos::{self, NewRepo, RepoStatus};
use git_archive::db::{self, sync_runs, unix_now};
use git_archive::settings::{self, Overrides};
use git_archive::sync::SyncContext;
use git_archive::sync::scheduler::Scheduler;
use sqlx::SqlitePool;

struct Setup {
    scheduler: Scheduler,
    ctx: SyncContext,
    dir: tempfile::TempDir,
}

async fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let config = common::test_config(&dir.path().join("data"));
    let ctx = SyncContext {
        pool: db::open_in_memory().await.unwrap(),
        cipher: Cipher::new(&config.secret_key),
        config: Arc::new(config),
    };
    Setup {
        scheduler: Scheduler::new(ctx.clone()),
        ctx,
        dir,
    }
}

/// Adds a repository whose URL is a local path; the database accepts any
/// URL, only the web form restricts them to http(s).
async fn add_repo(pool: &SqlitePool, url: &Path, name: &str, interval: Option<i64>) -> i64 {
    let url = url.to_string_lossy();
    let repo = NewRepo {
        url: &url,
        host: "local",
        owner: "team",
        name,
        source_id: None,
        username: None,
        token_enc: None,
        interval_minutes: interval,
        wiki_enabled: true,
    };
    repos::insert(pool, &repo, unix_now())
        .await
        .unwrap()
        .unwrap()
}

async fn run_tick(scheduler: &Scheduler) -> usize {
    let handles = scheduler.tick().await.unwrap();
    let started = handles.len();
    for handle in handles {
        handle.await.unwrap();
    }
    started
}

#[tokio::test]
async fn due_repository_is_synced_and_planned_with_its_own_interval() {
    let s = setup().await;
    let source = common::source_repo(s.dir.path(), "project");
    let id = add_repo(&s.ctx.pool, &source, "project", Some(30)).await;

    assert_eq!(run_tick(&s.scheduler).await, 1);

    let repo = repos::get(&s.ctx.pool, id).await.unwrap().unwrap();
    assert_eq!(repo.status, RepoStatus::Ok, "{:?}", repo.last_error);
    assert_eq!(repo.default_branch.as_deref(), Some("main"));
    assert!(!repo.has_wiki);
    assert!(repo.size_bytes > 0);
    let finished = repo.last_success_at.unwrap();
    assert_eq!(repo.next_sync_at, finished + 30 * 60);
    assert!(s.ctx.repo_dir(&repo).join("HEAD").is_file());

    let runs = sync_runs::list_for_repo(&s.ctx.pool, id, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, Some(RepoStatus::Ok));
    assert_eq!(runs[0].refs_changed, 1);

    // Not due again until its interval passes.
    assert_eq!(run_tick(&s.scheduler).await, 0);
}

#[tokio::test]
async fn repository_without_interval_uses_the_saved_global_interval() {
    let s = setup().await;
    let overrides = Overrides {
        interval_minutes: Some(15),
        ..Overrides::default()
    };
    settings::save(&s.ctx.pool, &overrides).await.unwrap();
    let source = common::source_repo(s.dir.path(), "project");
    let id = add_repo(&s.ctx.pool, &source, "project", None).await;

    run_tick(&s.scheduler).await;

    let repo = repos::get(&s.ctx.pool, id).await.unwrap().unwrap();
    assert_eq!(repo.next_sync_at, repo.last_success_at.unwrap() + 15 * 60);
}

#[tokio::test]
async fn concurrency_limits_the_syncs_started_at_once() {
    let s = setup().await;
    let overrides = Overrides {
        concurrency: Some(1),
        ..Overrides::default()
    };
    settings::save(&s.ctx.pool, &overrides).await.unwrap();
    for name in ["a", "b"] {
        let source = common::source_repo(s.dir.path(), name);
        add_repo(&s.ctx.pool, &source, name, None).await;
    }

    let first = s.scheduler.tick().await.unwrap();
    assert_eq!(first.len(), 1);
    // The slot is taken until the running sync ends.
    assert!(s.scheduler.tick().await.unwrap().is_empty());
    for handle in first {
        handle.await.unwrap();
    }
    assert_eq!(s.scheduler.running(), 0);
    assert_eq!(run_tick(&s.scheduler).await, 1);
}

#[tokio::test]
async fn claimed_repository_is_not_started_twice() {
    let s = setup().await;
    let source = common::source_repo(s.dir.path(), "project");
    let id = add_repo(&s.ctx.pool, &source, "project", None).await;
    assert!(repos::claim(&s.ctx.pool, id, unix_now()).await.unwrap());

    assert_eq!(run_tick(&s.scheduler).await, 0);
    let repo = repos::get(&s.ctx.pool, id).await.unwrap().unwrap();
    assert_eq!(repo.status, RepoStatus::Syncing);
    assert!(
        sync_runs::list_for_repo(&s.ctx.pool, id, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn missing_source_marks_the_repository_unreachable() {
    let s = setup().await;
    let missing = s.dir.path().join("deleted-project");
    let id = add_repo(&s.ctx.pool, &missing, "deleted-project", None).await;

    run_tick(&s.scheduler).await;

    let repo = repos::get(&s.ctx.pool, id).await.unwrap().unwrap();
    assert_eq!(repo.status, RepoStatus::Unreachable);
    assert!(repo.last_success_at.is_none());
    assert!(
        repo.last_error
            .unwrap()
            .contains("does not appear to be a git repository")
    );
    let runs = sync_runs::list_for_repo(&s.ctx.pool, id, 10).await.unwrap();
    assert_eq!(runs[0].outcome, Some(RepoStatus::Unreachable));
}

/// Makes the repository due now and runs one tick.
async fn sync_again(s: &Setup, id: i64) -> repos::Repo {
    repos::schedule_now(&s.ctx.pool, id, unix_now())
        .await
        .unwrap();
    assert_eq!(run_tick(&s.scheduler).await, 1);
    repos::get(&s.ctx.pool, id).await.unwrap().unwrap()
}

#[tokio::test]
async fn first_sync_checks_and_repacks_the_archive() {
    let s = setup().await;
    let source = common::source_repo(s.dir.path(), "project");
    let id = add_repo(&s.ctx.pool, &source, "project", None).await;
    run_tick(&s.scheduler).await;

    let repo = repos::get(&s.ctx.pool, id).await.unwrap().unwrap();
    assert_eq!(repo.status, RepoStatus::Ok, "{:?}", repo.last_error);
    assert!(repo.last_fsck_at.is_some());
    assert_eq!(repo.fsck_error, None);
    assert!(repo.last_repack_at.is_some());
    assert!(repo.repack_saved_bytes.is_some());
    // The repack left no loose objects.
    let counts = common::git_in(&s.ctx.repo_dir(&repo), &["count-objects", "-v"]);
    assert!(counts.lines().any(|l| l == "count: 0"), "{counts}");

    // Not due again within the interval: the times stay.
    let again = sync_again(&s, id).await;
    assert_eq!(again.last_fsck_at, repo.last_fsck_at);
    assert_eq!(again.last_repack_at, repo.last_repack_at);
}

#[tokio::test]
async fn zero_days_turn_the_maintenance_off() {
    let s = setup().await;
    let overrides = Overrides {
        fsck_interval_days: Some(0),
        repack_interval_days: Some(0),
        ..Overrides::default()
    };
    settings::save(&s.ctx.pool, &overrides).await.unwrap();
    let source = common::source_repo(s.dir.path(), "project");
    let id = add_repo(&s.ctx.pool, &source, "project", None).await;
    run_tick(&s.scheduler).await;

    let repo = repos::get(&s.ctx.pool, id).await.unwrap().unwrap();
    assert_eq!(repo.status, RepoStatus::Ok);
    assert_eq!((repo.last_fsck_at, repo.last_repack_at), (None, None));
}

#[tokio::test]
async fn damaged_archive_fails_the_integrity_check_until_repaired() {
    let s = setup().await;
    let source = common::source_repo(s.dir.path(), "project");
    let id = add_repo(&s.ctx.pool, &source, "project", None).await;
    run_tick(&s.scheduler).await;
    let repo = repos::get(&s.ctx.pool, id).await.unwrap().unwrap();

    // A loose object whose content is not a valid zlib stream.
    let objects = s.ctx.repo_dir(&repo).join("objects/ab");
    std::fs::create_dir_all(&objects).unwrap();
    let broken = objects.join("cdef0123456789abcdef0123456789abcdef01");
    std::fs::write(&broken, b"not an object").unwrap();
    // Make the check due.
    repos::record_fsck(&s.ctx.pool, id, 0, None).await.unwrap();

    let failed = sync_again(&s, id).await;
    assert_eq!(failed.status, RepoStatus::Error);
    let message = failed.last_error.clone().unwrap();
    assert!(message.starts_with("integrity check failed: "), "{message}");
    assert!(failed.fsck_error.is_some());
    let runs = sync_runs::list_for_repo(&s.ctx.pool, id, 1).await.unwrap();
    assert_eq!(runs[0].outcome, Some(RepoStatus::Error));
    // A damaged archive is not repacked.
    assert_eq!(failed.last_repack_at, repo.last_repack_at);

    // The check repeats on the next sync although its interval has not passed.
    let still = sync_again(&s, id).await;
    assert_eq!(still.status, RepoStatus::Error);

    std::fs::remove_file(&broken).unwrap();
    let repaired = sync_again(&s, id).await;
    assert_eq!(repaired.status, RepoStatus::Ok, "{:?}", repaired.last_error);
    assert_eq!(repaired.fsck_error, None);
}

#[tokio::test]
async fn archive_survives_when_the_source_disappears() {
    let s = setup().await;
    let source = common::source_repo(s.dir.path(), "project");
    let id = add_repo(&s.ctx.pool, &source, "project", Some(1)).await;
    run_tick(&s.scheduler).await;
    let before = repos::get(&s.ctx.pool, id).await.unwrap().unwrap();

    std::fs::remove_dir_all(&source).unwrap();
    repos::schedule_now(&s.ctx.pool, id, unix_now())
        .await
        .unwrap();
    run_tick(&s.scheduler).await;

    let after = repos::get(&s.ctx.pool, id).await.unwrap().unwrap();
    assert_eq!(after.status, RepoStatus::Unreachable);
    assert_eq!(after.last_success_at, before.last_success_at);
    assert_eq!(after.default_branch.as_deref(), Some("main"));
    let archive = s.ctx.repo_dir(&after);
    common::git_in(&archive, &["rev-parse", "--verify", "refs/heads/main"]);
}
