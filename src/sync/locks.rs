//! One lock per repository, so a sync and a backup never run git on the
//! same repository at once (a repack could remove a pack a bundle reads).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[derive(Debug, Clone, Default)]
pub struct RepoLocks {
    locks: Arc<Mutex<HashMap<i64, Arc<AsyncMutex<()>>>>>,
}

impl RepoLocks {
    /// Waits until no other task holds the repository, then holds it until
    /// the guard is dropped.
    pub async fn lock(&self, repo_id: i64) -> OwnedMutexGuard<()> {
        let lock = {
            // A panic while the map was held leaves it usable: it holds only locks.
            let mut map = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
            map.entry(repo_id).or_default().clone()
        };
        lock.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_held_repository_waits_and_others_do_not() {
        let locks = RepoLocks::default();
        let first = locks.lock(1).await;
        // Another repository is free.
        let _other = locks.lock(2).await;
        let waiting = tokio::time::timeout(Duration::from_millis(50), locks.lock(1)).await;
        assert!(waiting.is_err());
        drop(first);
        let _again = tokio::time::timeout(Duration::from_secs(1), locks.lock(1))
            .await
            .unwrap();
    }
}
