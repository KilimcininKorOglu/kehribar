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
use crate::repo_url::{self, RepoLocation};

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
    let mut no_ssh = Vec::new();
    for remote in &listed {
        match add(ctx, source, host, remote).await? {
            Added::Yes => report.added += 1,
            Added::Exists => {}
            Added::Filtered => report.filtered += 1,
            Added::Foreign => foreign.push(remote.clone_url.as_str()),
            Added::NoSsh => no_ssh.push(remote.clone_url.as_str()),
        }
    }
    // Shown on the sources page, so a wrong API URL or a missing SSH URL
    // does not lose repositories unnoticed.
    let reasons = [
        skipped(&foreign, &format!("the clone URL is not on {}", host.host)),
        skipped(&no_ssh, &format!("no SSH URL on {}", host.host)),
    ];
    let messages: Vec<String> = reasons.into_iter().flatten().collect();
    if !messages.is_empty() {
        report.error = Some(messages.join("; "));
    }
    Ok(report)
}

/// `N repositories skipped, <reason>: <urls>`, or `None` for no URL.
fn skipped(urls: &[&str], reason: &str) -> Option<String> {
    let noun = match urls.len() {
        0 => return None,
        1 => "repository",
        _ => "repositories",
    };
    Some(format!(
        "{} {noun} skipped, {reason}: {}",
        urls.len(),
        urls.join(", ")
    ))
}

enum Added {
    Yes,
    Exists,
    /// The source options leave the repository out.
    Filtered,
    /// The clone URL is invalid or on another host.
    Foreign,
    /// The source asks for SSH, but the provider names no SSH URL on the
    /// same server.
    NoSsh,
}

/// The URL a scanned repository is fetched from: the HTTPS clone URL, or
/// with `use_ssh` the SSH URL the provider names. That URL must be on the
/// same server, or on its `ssh.` name as Azure DevOps uses, because the
/// SSH key of the host goes to it.
fn fetch_url(source: &Source, remote: &RemoteRepo, https: &RepoLocation) -> Option<String> {
    if !source.use_ssh {
        return Some(https.url.clone());
    }
    let ssh = repo_url::parse(remote.ssh_url.as_deref()?).ok()?;
    let server = https.host.split(':').next().unwrap_or_default();
    let same_server = ssh.host == server || ssh.host == format!("ssh.{server}");
    (repo_url::is_ssh(&ssh.url) && same_server).then_some(ssh.url)
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
    let Some(url) = fetch_url(source, remote, &location) else {
        return Ok(Added::NoSsh);
    };
    let repo = NewRepo {
        url: &url,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sources::SourceKind;

    fn source(use_ssh: bool) -> Source {
        Source {
            id: 1,
            host_id: 1,
            kind: SourceKind::Org,
            name: "o".into(),
            auto_add: true,
            include_forks: false,
            last_scan_at: None,
            last_error: None,
            created_at: 0,
            include_patterns: None,
            exclude_patterns: None,
            skip_archived: false,
            use_ssh,
        }
    }

    fn remote(https: &str, ssh: Option<&str>) -> (RemoteRepo, RepoLocation) {
        let remote = RemoteRepo {
            clone_url: https.into(),
            ssh_url: ssh.map(Into::into),
            fork: false,
            archived: false,
        };
        let location = repo_url::parse(https).unwrap();
        (remote, location)
    }

    #[test]
    fn ssh_urls_must_stay_on_the_server_of_the_host() {
        let url = |use_ssh, https, ssh| {
            let (remote, location) = remote(https, ssh);
            fetch_url(&source(use_ssh), &remote, &location)
        };
        let azure = "https://dev.azure.com/org/proj/_git/app";
        let azure_ssh = Some("git@ssh.dev.azure.com:v3/org/proj/app");
        assert_eq!(url(false, azure, azure_ssh).as_deref(), Some(azure));
        assert_eq!(url(true, azure, azure_ssh).as_deref(), azure_ssh);
        // The port of the HTTP server does not count; SSH has its own.
        assert_eq!(
            url(
                true,
                "http://gitea.local:3000/o/r.git",
                Some("ssh://git@gitea.local:2222/o/r.git")
            )
            .as_deref(),
            Some("ssh://git@gitea.local:2222/o/r.git")
        );
        assert_eq!(url(true, azure, None), None);
        assert_eq!(url(true, azure, Some("git@evil.example:o/r.git")), None);
        assert_eq!(url(true, azure, Some("https://dev.azure.com/o/r")), None);
    }
}
