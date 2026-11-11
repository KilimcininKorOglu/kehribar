//! Scans a watched user or organization and adds the repositories that are
//! not archived yet. A repository that disappears from the list stays in
//! the archive.

use super::SyncContext;
use super::credentials;
use crate::db::hosts::{self, Host};
use crate::db::repos::{self, NewRepo};
use crate::db::sources::{self, Source};
use crate::db::unix_now;
use crate::notify::{self, Change, Event};
use crate::providers::{self, ProviderError, RemoteRepo};
use crate::repo_url;

/// What one scan did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Repositories the provider listed, forks included.
    pub listed: usize,
    pub added: usize,
    /// Listed repositories the fork, archived or name options left out.
    pub filtered: usize,
    /// The error stored on the source, `None` after a clean scan.
    pub error: Option<String>,
    /// Set when the host's API rate limit postponed the scan to this time;
    /// such a scan leaves the source as it was.
    pub deferred_until: Option<i64>,
}

/// Scans `source` and records the outcome on it. `Err` is a database error;
/// a provider error is part of the report. A host that waits out a rate
/// limit gets no request; the source is scanned again after the wait.
pub async fn scan(ctx: &SyncContext, source: &Source) -> sqlx::Result<ScanReport> {
    let started = unix_now();
    let report = match hosts::get(&ctx.pool, source.host_id).await? {
        Some(host) if host.api_blocked(started) => deferred(host.api_blocked_until),
        Some(host) => scan_host(ctx, source, &host).await?,
        None => failed(format!("host {} does not exist", source.host_id)),
    };
    if let Some(until) = report.deferred_until {
        tracing::info!(source = %source.name, until, "source scan postponed by a rate limit");
        return Ok(report);
    }
    sources::mark_scanned(&ctx.pool, source.id, started, report.error.as_deref()).await?;
    match &report.error {
        Some(err) => tracing::warn!(source = %source.name, %err, "source scan failed"),
        None => tracing::info!(
            source = %source.name,
            added = report.added,
            filtered = report.filtered,
            "source scanned"
        ),
    }
    report_change(ctx, source, report.error.as_deref()).await;
    Ok(report)
}

/// Notifies when the scans of `source` start failing or work again.
/// `source.last_error` is the result of the scan before this one.
async fn report_change(ctx: &SyncContext, source: &Source, error: Option<&str>) {
    let event = match notify::error_change(source.last_error.as_deref(), error) {
        Some(Change::Failed) => Event::SourceFailed {
            source: &source.name,
            message: error.unwrap_or_default(),
        },
        Some(Change::Recovered) => Event::SourceRecovered {
            source: &source.name,
        },
        None => return,
    };
    notify::send(ctx, event).await;
}

fn failed(error: String) -> ScanReport {
    ScanReport {
        error: Some(error),
        ..ScanReport::default()
    }
}

fn deferred(until: Option<i64>) -> ScanReport {
    ScanReport {
        deferred_until: until,
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
            Err(ProviderError::RateLimited { until }) => {
                hosts::block_api(&ctx.pool, host.id, until).await?;
                return Ok(deferred(Some(until)));
            }
            Err(err) => return Ok(failed(err.to_string())),
        };
    let mut report = ScanReport {
        listed: listed.len(),
        ..ScanReport::default()
    };
    let mut foreign = Vec::new();
    for remote in &listed {
        match add(ctx, source, host, remote).await? {
            Added::Yes => report.added += 1,
            Added::Exists => {}
            Added::Filtered => report.filtered += 1,
            Added::Foreign => foreign.push(remote.clone_url.as_str()),
        }
    }
    if !foreign.is_empty() {
        // Shown on the sources page, so a wrong API URL does not lose repositories unnoticed.
        let noun = if foreign.len() == 1 {
            "repository"
        } else {
            "repositories"
        };
        report.error = Some(format!(
            "{} {noun} skipped, the clone URL is not on {}: {}",
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
    /// The source options leave the repository out.
    Filtered,
    /// The clone URL is invalid or on another host.
    Foreign,
}

/// Whether the fork, archived and name options of `source` let `name` in.
fn accepts(source: &Source, remote: &RemoteRepo, name: &str) -> bool {
    (source.include_forks || !remote.fork)
        && !(source.skip_archived && remote.archived)
        && source.name_filter().allows(name)
}

async fn add(
    ctx: &SyncContext,
    source: &Source,
    host: &Host,
    remote: &RemoteRepo,
) -> sqlx::Result<Added> {
    let location = match repo_url::parse(&remote.clone_url) {
        // Gists of a github.com source live on gist.github.com.
        Ok(location)
            if repo_url::settings_host(&location.host).eq_ignore_ascii_case(&host.host) =>
        {
            location
        }
        _ => return Ok(Added::Foreign),
    };
    if !accepts(source, remote, &location.name) {
        return Ok(Added::Filtered);
    }
    let repo = NewRepo {
        url: &location.url,
        host: &location.host,
        owner: &location.owner,
        name: &location.name,
        source_id: Some(source.id),
        username: None,
        token_enc: None,
        interval_minutes: None,
        // A gist has no wiki.
        wiki_enabled: location.host != repo_url::GIST_HOST,
    };
    Ok(match repos::insert(&ctx.pool, &repo, unix_now()).await? {
        Some(_) => Added::Yes,
        None => Added::Exists,
    })
}
