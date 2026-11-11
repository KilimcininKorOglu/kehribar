//! The JSON shapes of the API. Secrets appear only as `has_*` flags.

use serde::Serialize;

use crate::db::hosts::Host;
use crate::db::repos::Repo;
use crate::db::sources::Source;
use crate::db::sync_runs::SyncRun;

/// Lines of a stored pattern text.
fn lines(text: Option<&str>) -> Vec<String> {
    text.map(|t| t.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

#[derive(Debug, Serialize)]
pub struct RepoView {
    pub id: i64,
    pub url: String,
    pub host: String,
    pub owner: String,
    pub name: String,
    pub source_id: Option<i64>,
    pub status: &'static str,
    pub last_sync_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub next_sync_at: i64,
    pub last_error: Option<String>,
    pub size_bytes: i64,
    pub lfs_size_bytes: i64,
    pub default_branch: Option<String>,
    pub has_wiki: bool,
    pub interval_minutes: Option<i64>,
    pub wiki_enabled: bool,
    pub lfs_enabled: bool,
    pub ref_exclude: Vec<String>,
    pub username: Option<String>,
    pub has_token: bool,
    pub last_fsck_at: Option<i64>,
    pub fsck_error: Option<String>,
    pub last_repack_at: Option<i64>,
    pub created_at: i64,
}

impl From<Repo> for RepoView {
    fn from(repo: Repo) -> Self {
        Self {
            status: repo.status.code(),
            ref_exclude: lines(repo.ref_exclude.as_deref()),
            has_token: repo.token_enc.is_some(),
            id: repo.id,
            url: repo.url,
            host: repo.host,
            owner: repo.owner,
            name: repo.name,
            source_id: repo.source_id,
            last_sync_at: repo.last_sync_at,
            last_success_at: repo.last_success_at,
            next_sync_at: repo.next_sync_at,
            last_error: repo.last_error,
            size_bytes: repo.size_bytes,
            lfs_size_bytes: repo.lfs_size_bytes,
            default_branch: repo.default_branch,
            has_wiki: repo.has_wiki,
            interval_minutes: repo.interval_minutes,
            wiki_enabled: repo.wiki_enabled,
            lfs_enabled: repo.lfs_enabled,
            username: repo.username,
            last_fsck_at: repo.last_fsck_at,
            fsck_error: repo.fsck_error,
            last_repack_at: repo.last_repack_at,
            created_at: repo.created_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RunView {
    pub id: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// `None` while the sync runs.
    pub outcome: Option<&'static str>,
    pub refs_changed: i64,
    pub refs_rescued: i64,
    pub message: Option<String>,
}

impl From<SyncRun> for RunView {
    fn from(run: SyncRun) -> Self {
        Self {
            id: run.id,
            started_at: run.started_at,
            finished_at: run.finished_at,
            outcome: run.outcome.map(|outcome| outcome.code()),
            refs_changed: run.refs_changed,
            refs_rescued: run.refs_rescued,
            message: run.message,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct HostView {
    pub id: i64,
    pub host: String,
    pub provider: &'static str,
    pub api_base_url: Option<String>,
    pub username: Option<String>,
    pub has_token: bool,
    pub ref_exclude: Vec<String>,
    pub max_concurrency: Option<i64>,
    pub api_blocked_until: Option<i64>,
    pub has_ssh_key: bool,
    pub ssh_public_key: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<Host> for HostView {
    fn from(host: Host) -> Self {
        Self {
            provider: host.provider.code(),
            has_token: host.token_enc.is_some(),
            ref_exclude: lines(host.ref_exclude.as_deref()),
            has_ssh_key: host.ssh_key_enc.is_some(),
            id: host.id,
            host: host.host,
            api_base_url: host.api_base_url,
            username: host.username,
            max_concurrency: host.max_concurrency,
            api_blocked_until: host.api_blocked_until,
            ssh_public_key: host.ssh_public_key,
            created_at: host.created_at,
            updated_at: host.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SourceView {
    pub id: i64,
    pub host_id: i64,
    pub kind: &'static str,
    pub name: String,
    pub auto_add: bool,
    pub include_forks: bool,
    pub skip_archived: bool,
    pub use_ssh: bool,
    pub include_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub last_scan_at: Option<i64>,
    pub last_error: Option<String>,
    pub created_at: i64,
}

impl From<Source> for SourceView {
    fn from(source: Source) -> Self {
        Self {
            kind: source.kind.code(),
            include_patterns: lines(source.include_patterns.as_deref()),
            exclude_patterns: lines(source.exclude_patterns.as_deref()),
            id: source.id,
            host_id: source.host_id,
            name: source.name,
            auto_add: source.auto_add,
            include_forks: source.include_forks,
            skip_archived: source.skip_archived,
            use_ssh: source.use_ssh,
            last_scan_at: source.last_scan_at,
            last_error: source.last_error,
            created_at: source.created_at,
        }
    }
}
