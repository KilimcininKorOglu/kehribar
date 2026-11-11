//! Notifications about state changes: a repository that starts failing or
//! recovers, a source scan that fails or works again, a failed backup.
//! Only changes are sent, so a lasting error is reported once.

mod channels;
pub mod settings;

use crate::db::repos::{Repo, RepoStatus};
use crate::db::unix_now;
use crate::i18n::Tr;
use crate::sync::SyncContext;
use channels::{RepoInfo, WebhookBody};

/// Prefix of every message, so the reader sees which system sent it.
const SENDER: &str = "git-archive";

#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    RepoFailed {
        repo: &'a Repo,
        previous: RepoStatus,
        status: RepoStatus,
        message: &'a str,
    },
    RepoRecovered {
        repo: &'a Repo,
        previous: RepoStatus,
    },
    SourceFailed {
        source: &'a str,
        message: &'a str,
    },
    SourceRecovered {
        source: &'a str,
    },
    BackupFailed {
        message: &'a str,
    },
    BackupRecovered,
    Test,
}

/// A change between two results of the same check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Failed,
    Recovered,
}

fn is_failure(status: RepoStatus) -> bool {
    matches!(status, RepoStatus::Error | RepoStatus::Unreachable)
}

/// A repository is reported when it enters a failed state, or another
/// failed state, and when it is up to date again after a failure.
pub fn repo_change(previous: RepoStatus, status: RepoStatus) -> Option<Change> {
    if is_failure(status) && status != previous {
        Some(Change::Failed)
    } else if status == RepoStatus::Ok && is_failure(previous) {
        Some(Change::Recovered)
    } else {
        None
    }
}

/// A check with an optional error (a source scan, a backup) is reported
/// when it starts failing and when it works again.
pub fn error_change(previous: Option<&str>, current: Option<&str>) -> Option<Change> {
    match (previous, current) {
        (None, Some(_)) => Some(Change::Failed),
        (Some(_), None) => Some(Change::Recovered),
        _ => None,
    }
}

impl Event<'_> {
    fn code(&self) -> &'static str {
        match self {
            Self::RepoFailed { .. } => "repo_failed",
            Self::RepoRecovered { .. } => "repo_recovered",
            Self::SourceFailed { .. } => "source_failed",
            Self::SourceRecovered { .. } => "source_recovered",
            Self::BackupFailed { .. } => "backup_failed",
            Self::BackupRecovered => "backup_recovered",
            Self::Test => "test",
        }
    }

    fn repo(&self) -> Option<&Repo> {
        match self {
            Self::RepoFailed { repo, .. } | Self::RepoRecovered { repo, .. } => Some(repo),
            _ => None,
        }
    }

    fn message(&self) -> Option<&str> {
        match self {
            Self::RepoFailed { message, .. }
            | Self::SourceFailed { message, .. }
            | Self::BackupFailed { message } => Some(message),
            _ => None,
        }
    }

    fn statuses(&self) -> (Option<RepoStatus>, Option<RepoStatus>) {
        match self {
            Self::RepoFailed {
                previous, status, ..
            } => (Some(*status), Some(*previous)),
            Self::RepoRecovered { previous, .. } => (Some(RepoStatus::Ok), Some(*previous)),
            _ => (None, None),
        }
    }

    /// The sentence of the notification in the language of `tr`.
    pub fn text(&self, tr: &Tr) -> String {
        let name = self.repo().map(Repo::full_name).unwrap_or_default();
        let status = self
            .statuses()
            .0
            .map(|s| tr.t(&format!("status.{}", s.code())).to_owned())
            .unwrap_or_default();
        let message = self.message().unwrap_or_default();
        let source = match self {
            Self::SourceFailed { source, .. } | Self::SourceRecovered { source } => source,
            _ => "",
        };
        let args = [
            ("repo", name.as_str()),
            ("status", status.as_str()),
            ("message", message),
            ("source", source),
        ];
        let key = format!("notify.{}", self.code());
        format!("{SENDER}: {}", tr.tf(&key, &args))
    }
}

/// Sends `event` to every channel in force. Every channel is tried; the
/// error names each one that failed. No channel at all is not an error.
pub async fn deliver(ctx: &SyncContext, event: &Event<'_>) -> Result<(), String> {
    let channels = settings::effective(&ctx.pool, &ctx.config, &ctx.cipher).await?;
    if channels.is_empty() {
        return Ok(());
    }
    let tr = Tr::new(ctx.catalog.clone(), channels.lang);
    let text = event.text(&tr);
    let link = event.repo().and_then(|repo| {
        let base = ctx.config.public_url.as_deref()?;
        Some(format!("{base}/repos/{}", repo.id))
    });
    let client = channels::client()?;
    let errors = [
        send_webhook(&client, &channels, event, &text, link.clone()).await,
        send_telegram(&client, &channels, &text, link.as_deref()).await,
    ];
    let errors: Vec<String> = errors.into_iter().flatten().collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Returns the error of the webhook, when there is one and it failed.
async fn send_webhook(
    client: &reqwest::Client,
    channels: &settings::Channels,
    event: &Event<'_>,
    text: &str,
    link: Option<String>,
) -> Option<String> {
    let url = channels.webhook_url.as_deref()?;
    let body = webhook_body(event, text, link);
    let result = channels::webhook(client, url, &body).await;
    result.err().map(|err| format!("webhook: {err}"))
}

/// Returns the error of Telegram, when it is set and failed. The link goes
/// on its own line.
async fn send_telegram(
    client: &reqwest::Client,
    channels: &settings::Channels,
    text: &str,
    link: Option<&str>,
) -> Option<String> {
    let telegram = channels.telegram.as_ref()?;
    let full = match link {
        Some(link) => format!("{text}\n{link}"),
        None => text.to_owned(),
    };
    let result = channels::telegram(client, telegram, &full).await;
    result.err().map(|err| format!("Telegram: {err}"))
}

fn webhook_body<'a>(event: &'a Event<'_>, text: &'a str, link: Option<String>) -> WebhookBody<'a> {
    let (status, previous) = event.statuses();
    WebhookBody {
        event: event.code(),
        text,
        content: text,
        repo: event.repo().map(|repo| RepoInfo {
            id: repo.id,
            name: repo.full_name(),
            url: &repo.url,
        }),
        source: match event {
            Event::SourceFailed { source, .. } | Event::SourceRecovered { source } => Some(source),
            _ => None,
        },
        status: status.map(|s| s.code()),
        previous_status: previous.map(|s| s.code()),
        message: event.message(),
        at: unix_now(),
        link,
    }
}

/// Delivers `event` and records the outcome for the settings page. A
/// failed delivery is logged; it never fails the work that caused it.
pub async fn send(ctx: &SyncContext, event: Event<'_>) {
    let result = deliver(ctx, &event).await;
    if let Err(err) = &result {
        tracing::warn!(event = event.code(), %err, "notification failed");
    }
    let error = result.err();
    if let Err(err) = settings::record_delivery(&ctx.pool, unix_now(), error.as_deref()).await {
        tracing::error!(%err, "cannot record the notification result");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use RepoStatus::{Error, Ok as Fine, Pending, Unreachable};

    #[test]
    fn repositories_are_reported_on_changes_only() {
        assert_eq!(repo_change(Fine, Error), Some(Change::Failed));
        assert_eq!(repo_change(Pending, Unreachable), Some(Change::Failed));
        assert_eq!(repo_change(Error, Unreachable), Some(Change::Failed));
        assert_eq!(repo_change(Error, Error), None);
        assert_eq!(repo_change(Error, Fine), Some(Change::Recovered));
        assert_eq!(repo_change(Unreachable, Fine), Some(Change::Recovered));
        assert_eq!(repo_change(Pending, Fine), None);
        assert_eq!(repo_change(Fine, Fine), None);
    }

    #[test]
    fn checks_with_an_error_are_reported_on_changes_only() {
        assert_eq!(error_change(None, Some("x")), Some(Change::Failed));
        assert_eq!(error_change(Some("x"), Some("y")), None);
        assert_eq!(error_change(Some("x"), None), Some(Change::Recovered));
        assert_eq!(error_change(None, None), None);
    }
}
