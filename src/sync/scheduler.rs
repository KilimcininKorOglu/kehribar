//! Starts due syncs, never more at once than the concurrency setting.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::{SyncContext, worker};
use crate::db::{repos, unix_now};
use crate::settings;

/// How often the scheduler looks for due repositories without a wake-up.
pub const TICK: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct Scheduler {
    ctx: SyncContext,
    running: Arc<AtomicI64>,
    wake: Arc<Notify>,
}

/// Decrements the running count even when a sync task panics.
struct RunningGuard(Arc<AtomicI64>, Arc<Notify>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
        // A free slot may let the next due repository start now.
        self.1.notify_one();
    }
}

impl Scheduler {
    pub fn new(ctx: SyncContext) -> Self {
        Self {
            ctx,
            running: Arc::new(AtomicI64::new(0)),
            wake: Arc::new(Notify::new()),
        }
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
                tokio::select! {
                    () = tokio::time::sleep(TICK) => {},
                    () = self.wake.notified() => {},
                }
            }
        })
    }

    /// Starts every due repository that fits into the free slots and returns
    /// the started tasks.
    pub async fn tick(&self) -> sqlx::Result<Vec<JoinHandle<()>>> {
        let settings = settings::effective(&self.ctx.pool, &self.ctx.config).await?;
        let free = settings.concurrency - self.running();
        if free <= 0 {
            return Ok(Vec::new());
        }
        let now = unix_now();
        let mut started = Vec::new();
        for repo in repos::due(&self.ctx.pool, now, free).await? {
            // The claim fails when another tick or process took the repository.
            if !repos::claim(&self.ctx.pool, repo.id, now).await? {
                continue;
            }
            self.running.fetch_add(1, Ordering::SeqCst);
            let guard = RunningGuard(self.running.clone(), self.wake.clone());
            let ctx = self.ctx.clone();
            started.push(tokio::spawn(async move {
                let _guard = guard;
                if let Err(err) = worker::run(&ctx, &repo, settings).await {
                    tracing::error!(repo = %repo.url, %err, "cannot record sync result");
                }
            }));
        }
        Ok(started)
    }
}
