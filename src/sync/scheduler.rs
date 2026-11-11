//! Starts due syncs, never more at once than the concurrency setting.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::{SyncContext, source_scan, worker};
use crate::backup;
use crate::db::repos::Repo;
use crate::db::sources::{self, Source};
use crate::db::{hosts, repos, unix_now, usage};
use crate::repo_url;
use crate::settings::{self, SyncSettings};

/// How often the scheduler looks for due repositories without a wake-up.
pub const TICK: Duration = Duration::from_secs(60);

/// Due repositories one tick looks through. A host at its own limit can
/// hold many of them, so the tick reads past those to find others.
const DUE_WINDOW: i64 = 1_000;

/// Running syncs per host name (lowercase, gists counted on github.com).
type HostCounts = Arc<Mutex<HashMap<String, i64>>>;

#[derive(Clone)]
pub struct Scheduler {
    ctx: SyncContext,
    running: Arc<AtomicI64>,
    per_host: HostCounts,
    wake: Arc<Notify>,
    /// Set while the scheduled source scan runs, so scans never overlap.
    scanning: Arc<AtomicBool>,
    /// Set while a backup runs, so backups never overlap.
    backing_up: Arc<AtomicBool>,
}

/// Clears a running flag even when its task panics.
struct ScanGuard(Arc<AtomicBool>);

impl Drop for ScanGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Decrements the running counts even when a sync task panics.
struct RunningGuard {
    running: Arc<AtomicI64>,
    per_host: HostCounts,
    host: String,
    wake: Arc<Notify>,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.running.fetch_sub(1, Ordering::SeqCst);
        let mut counts = lock(&self.per_host);
        if let Some(count) = counts.get_mut(&self.host) {
            *count -= 1;
            if *count <= 0 {
                counts.remove(&self.host);
            }
        }
        drop(counts);
        // A free slot may let the next due repository start now.
        self.wake.notify_one();
    }
}

/// The counts stay usable after a panic in another task: they are plain
/// numbers that no half-done update can break.
fn lock(counts: &HostCounts) -> MutexGuard<'_, HashMap<String, i64>> {
    counts.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The sync limit of every host that has one, by lowercase host name.
async fn host_limits(pool: &sqlx::SqlitePool) -> sqlx::Result<HashMap<String, i64>> {
    Ok(hosts::list(pool)
        .await?
        .into_iter()
        .filter_map(|host| Some((host.host.to_ascii_lowercase(), host.max_concurrency?)))
        .collect())
}

/// The host whose limit a repository counts against.
fn limit_key(repo: &Repo) -> String {
    repo_url::settings_host(&repo.host).to_ascii_lowercase()
}

impl Scheduler {
    pub fn new(ctx: SyncContext) -> Self {
        Self {
            ctx,
            running: Arc::new(AtomicI64::new(0)),
            per_host: HostCounts::default(),
            wake: Arc::new(Notify::new()),
            scanning: Arc::new(AtomicBool::new(false)),
            backing_up: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn context(&self) -> &SyncContext {
        &self.ctx
    }

    /// Makes the scheduler look for due repositories now.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    pub fn running(&self) -> i64 {
        self.running.load(Ordering::SeqCst)
    }

    /// Runs the scheduling loop until the process ends.
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                if let Err(err) = self.tick().await {
                    tracing::error!(%err, "scheduler tick failed");
                }
                if let Err(err) = self.scan_sources().await {
                    tracing::error!(%err, "source scan could not start");
                }
                if let Err(err) = self.backup_if_due().await {
                    tracing::error!(%err, "backup could not start");
                }
                if let Err(err) = usage::record_day(&self.ctx.pool, unix_now()).await {
                    tracing::error!(%err, "cannot record the disk usage of the day");
                }
                tokio::select! {
                    () = tokio::time::sleep(TICK) => {},
                    () = self.wake.notified() => {},
                }
            }
        })
    }

    /// Starts every due repository that fits into the free slots, globally
    /// and on its host, and returns the started tasks.
    pub async fn tick(&self) -> sqlx::Result<Vec<JoinHandle<()>>> {
        let settings = settings::effective(&self.ctx.pool, &self.ctx.config).await?;
        let free = usize::try_from(settings.concurrency - self.running()).unwrap_or(0);
        if free == 0 {
            return Ok(Vec::new());
        }
        let now = unix_now();
        let limits = host_limits(&self.ctx.pool).await?;
        let mut started = Vec::new();
        for repo in repos::due(&self.ctx.pool, now, DUE_WINDOW).await? {
            if started.len() == free {
                break;
            }
            let host = limit_key(&repo);
            let full = limits
                .get(&host)
                .is_some_and(|limit| self.running_on(&host) >= *limit);
            // The claim fails when another tick or process took the repository.
            if full || !repos::claim(&self.ctx.pool, repo.id, now).await? {
                continue;
            }
            started.push(self.start(repo, host, settings));
        }
        Ok(started)
    }

    /// Syncs on `host` that run now.
    pub fn running_on(&self, host: &str) -> i64 {
        lock(&self.per_host).get(host).copied().unwrap_or(0)
    }

    fn start(&self, repo: Repo, host: String, settings: SyncSettings) -> JoinHandle<()> {
        self.running.fetch_add(1, Ordering::SeqCst);
        *lock(&self.per_host).entry(host.clone()).or_insert(0) += 1;
        let guard = RunningGuard {
            running: self.running.clone(),
            per_host: self.per_host.clone(),
            host,
            wake: self.wake.clone(),
        };
        let ctx = self.ctx.clone();
        tokio::spawn(async move {
            let _guard = guard;
            if let Err(err) = worker::run(&ctx, &repo, settings).await {
                tracing::error!(repo = %repo.url, %err, "cannot record sync result");
            }
        })
    }

    /// Starts one task that scans every watched source whose last scan is
    /// older than the global sync interval, unless such a task still runs.
    pub async fn scan_sources(&self) -> sqlx::Result<Option<JoinHandle<()>>> {
        if self.scanning.swap(true, Ordering::SeqCst) {
            return Ok(None);
        }
        let guard = ScanGuard(self.scanning.clone());
        let settings = settings::effective(&self.ctx.pool, &self.ctx.config).await?;
        let before = unix_now() - settings.interval_minutes * 60;
        let due = sources::due_for_scan(&self.ctx.pool, before).await?;
        if due.is_empty() {
            return Ok(None);
        }
        let this = self.clone();
        Ok(Some(tokio::spawn(async move {
            let _guard = guard;
            for source in due {
                this.scan_one(&source).await;
            }
        })))
    }

    /// Starts a backup when one is configured and its interval has passed.
    pub async fn backup_if_due(&self) -> sqlx::Result<Option<JoinHandle<()>>> {
        if self.backing_up.load(Ordering::SeqCst) || !backup::is_due(&self.ctx).await? {
            return Ok(None);
        }
        Ok(self.start_backup())
    }

    /// Starts a backup now, unless one runs. `None` means one runs already.
    pub fn start_backup(&self) -> Option<JoinHandle<()>> {
        if self.backing_up.swap(true, Ordering::SeqCst) {
            return None;
        }
        let guard = ScanGuard(self.backing_up.clone());
        let ctx = self.ctx.clone();
        Some(tokio::spawn(async move {
            let _guard = guard;
            if let Err(err) = backup::run(&ctx, backup::s3::PART_SIZE).await {
                tracing::error!(%err, "cannot record the backup");
            }
        }))
    }

    pub fn is_backing_up(&self) -> bool {
        self.backing_up.load(Ordering::SeqCst)
    }

    /// Scans one source now, then wakes the loop for the repositories it added.
    pub fn scan_now(&self, source: Source) -> JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move { this.scan_one(&source).await })
    }

    async fn scan_one(&self, source: &Source) {
        match source_scan::scan(&self.ctx, source).await {
            Ok(report) if report.added > 0 => self.wake(),
            Ok(_) => {}
            Err(err) => tracing::error!(source = %source.name, %err, "cannot record source scan"),
        }
    }
}
