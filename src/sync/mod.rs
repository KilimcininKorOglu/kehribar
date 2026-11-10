//! Background synchronisation of the archived repositories.

pub mod credentials;
pub mod scheduler;
pub mod source_scan;
pub mod worker;

use std::path::PathBuf;
use std::sync::Arc;

use sqlx::SqlitePool;

use crate::config::Config;
use crate::crypto::Cipher;
use crate::db::repos::Repo;
use crate::repo_url;

/// What a sync needs from the application.
#[derive(Clone)]
pub struct SyncContext {
    pub pool: SqlitePool,
    pub cipher: Cipher,
    pub config: Arc<Config>,
}

impl SyncContext {
    pub fn repo_dir(&self, repo: &Repo) -> PathBuf {
        repo_url::archive_dir(&self.config.data_dir, &repo.host, &repo.owner, &repo.name)
    }
}
