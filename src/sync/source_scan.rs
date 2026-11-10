//! Scans a watched user or organization and adds the repositories that are
//! not archived yet. A repository that disappears from the list stays in
//! the archive.

use super::SyncContext;
use super::credentials;
use crate::db::hosts::{self, Host};
use crate::db::repos::{self, NewRepo};
use crate::db::sources::{self, Source};
use crate::db::unix_now;
use crate::providers::{self, RemoteRepo};
use crate::repo_url;

/// What one scan did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Repositories the provider listed, forks included.
    pub listed: usize,
    pub added: usize,
    /// The error stored on the source, `None` after a clean scan.
    pub error: Option<String>,
}

/// Scans `source` and records the outcome on it. `Err` is a database error;
/// a provider error is part of the report.
pub async fn scan(ctx: &SyncContext, source: &Source) -> sqlx::Result<ScanReport> {
    let started = unix_now();
    let report = match hosts::get(&ctx.pool, source.host_id).await? {
        Some(host) => scan_host(ctx, source, &host).await?,
        None => failed(format!("host {} does not exist", source.host_id)),
    };
    sources::mark_scanned(&ctx.pool, source.id, started, report.error.as_deref()).await?;
    match &report.error {
        Some(err) => tracing::warn!(source = %source.name, %err, "source scan failed"),
        None => tracing::info!(source = %source.name, added = report.added, "source scanned"),
    }
    Ok(report)
}

fn failed(error: String) -> ScanReport {
    ScanReport {
        error: Some(error),
        ..ScanReport::default()
    }
}

async fn scan_host(ctx: &SyncContext, source: &Source, host: &Host) -> sqlx::Result<ScanReport> {
    let credentials = match credentials::for_host(&ctx.cipher, host) {
        Ok(credentials) => credentials,
        Err(err) => return Ok(failed(err)),
    };
    let listed =
        match providers::list_repos(host, credentials.as_ref(), source.kind, &source.name).await {
            Ok(listed) => listed,
            Err(err) => return Ok(failed(err.to_string())),
        };
    let mut report = ScanReport {
        listed: listed.len(),
        ..ScanReport::default()
    };
    let mut foreign = Vec::new();
    for remote in listed.iter().filter(|r| source.include_forks || !r.fork) {
        match add(ctx, source, host, remote).await? {
            Added::Yes => report.added += 1,
            Added::Exists => {}
            Added::Foreign => foreign.push(remote.clone_url.as_str()),
        }
    }
    if !foreign.is_empty() {
        // Shown on the sources page, so a wrong API URL does not lose repositories unnoticed.
        report.error = Some(format!(
            "{} repositories skipped, their clone URL is not on {}: {}",
            foreign.len(),
            host.host,
            foreign.join(", ")
        ));
    }
    Ok(report)
}

enum Added {
    Yes,
    Exists,
    /// The clone URL is invalid or on another host.
    Foreign,
}

async fn add(
    ctx: &SyncContext,
    source: &Source,
    host: &Host,
    remote: &RemoteRepo,
) -> sqlx::Result<Added> {
    let location = match repo_url::parse(&remote.clone_url) {
        Ok(location) if location.host.eq_ignore_ascii_case(&host.host) => location,
        _ => return Ok(Added::Foreign),
    };
    let repo = NewRepo {
        url: &location.url,
        host: &location.host,
        owner: &location.owner,
        name: &location.name,
        source_id: Some(source.id),
        username: None,
        token_enc: None,
        interval_minutes: None,
        wiki_enabled: true,
    };
    Ok(match repos::insert(&ctx.pool, &repo, unix_now()).await? {
        Some(_) => Added::Yes,
        None => Added::Exists,
    })
}
