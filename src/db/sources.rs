//! Watched users and organizations whose repositories are imported.

use sqlx::{FromRow, SqlitePool};

use crate::name_filter::NameFilter;

text_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
    #[sqlx(type_name = "TEXT", rename_all = "lowercase")]
    pub enum SourceKind {
        User => "user",
        Org => "org",
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Source {
    pub id: i64,
    pub host_id: i64,
    pub kind: SourceKind,
    pub name: String,
    /// Scheduled scans add the repositories that appeared since the last one.
    pub auto_add: bool,
    /// Scans import forks too.
    pub include_forks: bool,
    pub last_scan_at: Option<i64>,
    pub last_error: Option<String>,
    pub created_at: i64,
    /// Repository name globs, one per line; see [`crate::name_filter`].
    pub include_patterns: Option<String>,
    pub exclude_patterns: Option<String>,
    /// Scans leave out repositories the provider marks as archived.
    pub skip_archived: bool,
}

impl Source {
    pub fn name_filter(&self) -> NameFilter<'_> {
        NameFilter {
            include: self.include_patterns.as_deref(),
            exclude: self.exclude_patterns.as_deref(),
        }
    }

    pub fn has_filter(&self) -> bool {
        self.include_patterns.is_some() || self.exclude_patterns.is_some() || self.skip_archived
    }
}

/// The values a source is created with.
#[derive(Debug, Clone)]
pub struct NewSource<'a> {
    pub host_id: i64,
    pub kind: SourceKind,
    pub name: &'a str,
    pub options: SourceOptions,
}

/// The options of a source the user can change later.
#[derive(Debug, Clone, Default)]
pub struct SourceOptions {
    pub auto_add: bool,
    pub include_forks: bool,
    pub skip_archived: bool,
    pub include_patterns: Option<String>,
    pub exclude_patterns: Option<String>,
}

/// Builds a `SELECT` of every source column at compile time.
macro_rules! select_sources {
    ($tail:literal) => {
        concat!(
            "SELECT id, host_id, kind, name, auto_add, include_forks, last_scan_at, \
             last_error, created_at, include_patterns, exclude_patterns, skip_archived \
             FROM sources ",
            $tail
        )
    };
}

/// Inserts a source. Returns `None` when the host already has a source of
/// that kind and name.
pub async fn insert(
    pool: &SqlitePool,
    source: &NewSource<'_>,
    now: i64,
) -> sqlx::Result<Option<i64>> {
    let options = &source.options;
    sqlx::query_scalar(
        "INSERT INTO sources (host_id, kind, name, auto_add, include_forks, skip_archived, \
         include_patterns, exclude_patterns, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(source.host_id)
    .bind(source.kind)
    .bind(source.name)
    .bind(options.auto_add)
    .bind(options.include_forks)
    .bind(options.skip_archived)
    .bind(&options.include_patterns)
    .bind(&options.exclude_patterns)
    .bind(now)
    .fetch_optional(pool)
    .await
}

pub async fn get(pool: &SqlitePool, id: i64) -> sqlx::Result<Option<Source>> {
    sqlx::query_as(select_sources!("WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn list(pool: &SqlitePool) -> sqlx::Result<Vec<Source>> {
    sqlx::query_as(select_sources!("ORDER BY name"))
        .fetch_all(pool)
        .await
}

/// Sources with `auto_add` whose last scan started at or before `before`,
/// or that were never scanned.
pub async fn due_for_scan(pool: &SqlitePool, before: i64) -> sqlx::Result<Vec<Source>> {
    sqlx::query_as(select_sources!(
        "WHERE auto_add = 1 AND (last_scan_at IS NULL OR last_scan_at <= ?) ORDER BY id"
    ))
    .bind(before)
    .fetch_all(pool)
    .await
}

pub async fn update_options(
    pool: &SqlitePool,
    id: i64,
    options: &SourceOptions,
) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "UPDATE sources SET auto_add = ?, include_forks = ?, skip_archived = ?, \
         include_patterns = ?, exclude_patterns = ? WHERE id = ?",
    )
    .bind(options.auto_add)
    .bind(options.include_forks)
    .bind(options.skip_archived)
    .bind(&options.include_patterns)
    .bind(&options.exclude_patterns)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Records the outcome of a scan. `error` is `None` after a successful scan.
pub async fn mark_scanned(
    pool: &SqlitePool,
    id: i64,
    now: i64,
    error: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE sources SET last_scan_at = ?, last_error = ? WHERE id = ?")
        .bind(now)
        .bind(error)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The number of archived repositories each source added, by source id.
pub async fn repo_counts(pool: &SqlitePool) -> sqlx::Result<Vec<(i64, i64)>> {
    sqlx::query_as(
        "SELECT source_id, COUNT(*) FROM repos WHERE source_id IS NOT NULL GROUP BY source_id",
    )
    .fetch_all(pool)
    .await
}

/// Deletes the source. Its repositories stay in the archive.
pub async fn delete(pool: &SqlitePool, id: i64) -> sqlx::Result<bool> {
    let result = sqlx::query("DELETE FROM sources WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::hosts::{self, HostInput, Provider};
    use crate::db::open_in_memory;
    use crate::db::repos::{self, NewRepo};

    async fn pool_with_host() -> (SqlitePool, i64) {
        let pool = open_in_memory().await.unwrap();
        let host = HostInput {
            host: "codeberg.org",
            provider: Provider::Forgejo,
            api_base_url: None,
            username: None,
            token_enc: None,
        };
        let host_id = hosts::insert(&pool, &host, 1).await.unwrap();
        (pool, host_id)
    }

    fn source(host_id: i64, kind: SourceKind, name: &str) -> NewSource<'_> {
        NewSource {
            host_id,
            kind,
            name,
            options: SourceOptions {
                auto_add: true,
                ..SourceOptions::default()
            },
        }
    }

    #[tokio::test]
    async fn scan_state_and_options_are_stored() {
        let (pool, host_id) = pool_with_host().await;
        let id = insert(&pool, &source(host_id, SourceKind::Org, "forgejo"), 5)
            .await
            .unwrap()
            .unwrap();
        // Never scanned: due at any time.
        assert_eq!(due_for_scan(&pool, 0).await.unwrap().len(), 1);

        mark_scanned(&pool, id, 9, Some("rate limited"))
            .await
            .unwrap();
        let stored = get(&pool, id).await.unwrap().unwrap();
        assert_eq!(stored.last_scan_at, Some(9));
        assert_eq!(stored.last_error.as_deref(), Some("rate limited"));
        assert!(!stored.include_forks);
        assert!(due_for_scan(&pool, 8).await.unwrap().is_empty());
        assert_eq!(due_for_scan(&pool, 9).await.unwrap().len(), 1);

        assert!(!stored.has_filter());
        let options = SourceOptions {
            auto_add: false,
            include_forks: true,
            skip_archived: true,
            include_patterns: Some("api-*".into()),
            exclude_patterns: Some("*-old".into()),
        };
        assert!(update_options(&pool, id, &options).await.unwrap());
        let stored = get(&pool, id).await.unwrap().unwrap();
        assert!(!stored.auto_add && stored.include_forks && stored.skip_archived);
        assert!(stored.has_filter());
        assert!(stored.name_filter().allows("api-x") && !stored.name_filter().allows("api-old"));
        // A source without auto_add is never scanned on schedule.
        assert!(due_for_scan(&pool, 100).await.unwrap().is_empty());
        assert_eq!(list(&pool).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deleting_a_source_keeps_its_repositories() {
        let (pool, host_id) = pool_with_host().await;
        let id = insert(&pool, &source(host_id, SourceKind::User, "alice"), 5)
            .await
            .unwrap()
            .unwrap();
        let repo = NewRepo {
            url: "https://codeberg.org/alice/a",
            host: "codeberg.org",
            owner: "alice",
            name: "a",
            source_id: Some(id),
            username: None,
            token_enc: None,
            interval_minutes: None,
            wiki_enabled: true,
        };
        let repo_id = repos::insert(&pool, &repo, 5).await.unwrap().unwrap();
        assert_eq!(repo_counts(&pool).await.unwrap(), vec![(id, 1)]);

        assert!(delete(&pool, id).await.unwrap());
        let kept = repos::get(&pool, repo_id).await.unwrap().unwrap();
        assert_eq!(kept.source_id, None);
    }

    #[tokio::test]
    async fn deleting_the_host_deletes_its_sources() {
        let (pool, host_id) = pool_with_host().await;
        let id = insert(&pool, &source(host_id, SourceKind::User, "alice"), 5)
            .await
            .unwrap()
            .unwrap();
        hosts::delete(&pool, host_id).await.unwrap();
        assert!(get(&pool, id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn duplicate_source_is_ignored() {
        let (pool, host_id) = pool_with_host().await;
        let first = insert(&pool, &source(host_id, SourceKind::User, "alice"), 5).await;
        assert!(first.unwrap().is_some());
        let again = insert(&pool, &source(host_id, SourceKind::User, "ALICE"), 5).await;
        assert_eq!(again.unwrap(), None);
    }
}
