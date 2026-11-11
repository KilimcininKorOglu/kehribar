//! Background synchronisation of the archived repositories.

pub mod assets;
pub mod credentials;
pub mod locks;
pub mod ref_log;
pub mod relocate;
pub mod scheduler;
pub mod source_scan;
pub mod worker;

use std::path::PathBuf;
use std::sync::Arc;

use sqlx::SqlitePool;

use crate::config::Config;
use crate::crypto::Cipher;
use crate::db::repos::Repo;
use crate::i18n::Catalog;
use crate::repo_url;

/// What a sync needs from the application.
#[derive(Clone)]
pub struct SyncContext {
    pub pool: SqlitePool,
    pub cipher: Cipher,
    pub config: Arc<Config>,
    /// Translations for the notifications.
    pub catalog: Arc<Catalog>,
    pub locks: locks::RepoLocks,
    /// The addresses an image download may connect to.
    pub guard: assets::AddressGuard,
}

impl SyncContext {
    pub fn repo_dir(&self, repo: &Repo) -> PathBuf {
        repo_url::archive_dir(&self.config.data_dir, &repo.host, &repo.owner, &repo.name)
    }
}
