//! The history of the branches and tags of each repository, and the ones
//! the source no longer has.

use std::collections::BTreeMap;

use sqlx::{FromRow, SqliteExecutor, SqlitePool};

text_enum! {
    /// What happened to a ref.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
    #[sqlx(type_name = "TEXT", rename_all = "snake_case")]
    pub enum EventKind {
        /// The state when the recording began.
        Baseline => "baseline",
        Created => "created",
        /// Moved forward: the new commit contains the old one.
        Moved => "moved",
        /// Force-pushed; the old commit is kept in the archive.
        Rewritten => "rewritten",
        DeletedAtSource => "deleted_at_source",
        BackAtSource => "back_at_source",
    }
}

/// An event to write.
#[derive(Debug, Clone, Copy)]
pub struct NewEvent<'a> {
    pub name: &'a str,
    pub old: Option<&'a str>,
    pub new: Option<&'a str>,
    pub kind: EventKind,
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct RefEvent {
    pub id: i64,
    pub run_id: Option<i64>,
    #[sqlx(rename = "ref")]
    pub name: String,
    pub old_oid: Option<String>,
    /// `None` only for a `rewritten` event written from a kept commit, when
    /// the commit that replaced it is unknown.
    pub new_oid: Option<String>,
    pub kind: EventKind,
    pub at: i64,
}

impl RefEvent {
    /// The name without `refs/heads/` or `refs/tags/`.
    pub fn short(&self) -> &str {
        short_name(&self.name)
    }

    pub fn is_tag(&self) -> bool {
        self.name.starts_with("refs/tags/")
    }

    pub fn old_short(&self) -> Option<&str> {
        self.old_oid.as_deref().map(short_oid)
    }

    pub fn new_short(&self) -> Option<&str> {
        self.new_oid.as_deref().map(short_oid)
    }
}

/// The first 10 characters of an object id.
pub fn short_oid(id: &str) -> &str {
    id.get(..10).unwrap_or(id)
}

/// `main` for `refs/heads/main`, `v1` for `refs/tags/v1`.
pub fn short_name(name: &str) -> &str {
    name.strip_prefix("refs/heads/")
        .or_else(|| name.strip_prefix("refs/tags/"))
        .unwrap_or(name)
}

/// A `SELECT` of every event column from `$from`, built at compile time.
macro_rules! select_events {
    ($from:literal) => {
        concat!(
            "SELECT id, run_id, ref, old_oid, new_oid, kind, at FROM ",
            $from
        )
    };
}

pub async fn has_any<'e>(db: impl SqliteExecutor<'e>, repo_id: i64) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM ref_events WHERE repo_id = ?)")
        .bind(repo_id)
        .fetch_one(db)
        .await
}

pub async fn insert<'e>(
    db: impl SqliteExecutor<'e>,
    repo_id: i64,
    run_id: Option<i64>,
    event: &NewEvent<'_>,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO ref_events (repo_id, run_id, ref, old_oid, new_oid, kind, at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(repo_id)
    .bind(run_id)
    .bind(event.name)
    .bind(event.old)
    .bind(event.new)
    .bind(event.kind)
    .bind(event.at)
    .execute(db)
    .await?;
    Ok(())
}

/// The refs the source no longer has, with the time this was first seen.
pub async fn missing<'e>(
    db: impl SqliteExecutor<'e>,
    repo_id: i64,
) -> sqlx::Result<BTreeMap<String, i64>> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT ref, since FROM source_missing_refs WHERE repo_id = ?")
            .bind(repo_id)
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().collect())
}

pub async fn add_missing<'e>(
    db: impl SqliteExecutor<'e>,
    repo_id: i64,
    name: &str,
    since: i64,
) -> sqlx::Result<()> {
    sqlx::query("INSERT OR IGNORE INTO source_missing_refs (repo_id, ref, since) VALUES (?, ?, ?)")
        .bind(repo_id)
        .bind(name)
        .bind(since)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn remove_missing<'e>(
    db: impl SqliteExecutor<'e>,
    repo_id: i64,
    name: &str,
) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM source_missing_refs WHERE repo_id = ? AND ref = ?")
        .bind(repo_id)
        .bind(name)
        .execute(db)
        .await?;
    Ok(())
}

/// The events of the `runs` newest sync runs of a repository.
pub async fn of_recent_runs(
    pool: &SqlitePool,
    repo_id: i64,
    runs: i64,
) -> sqlx::Result<Vec<RefEvent>> {
    sqlx::query_as(select_events!(
        "ref_events WHERE run_id IN (SELECT id FROM sync_runs \
         WHERE repo_id = ?1 ORDER BY started_at DESC, id DESC LIMIT ?2) ORDER BY ref, id"
    ))
    .bind(repo_id)
    .bind(runs)
    .fetch_all(pool)
    .await
}

/// The last event of each ref up to `at` that names a commit: what each
/// branch and tag pointed to at that time, as far as the history knows.
pub async fn state_at(pool: &SqlitePool, repo_id: i64, at: i64) -> sqlx::Result<Vec<RefEvent>> {
    sqlx::query_as(select_events!(
        "(SELECT *, ROW_NUMBER() OVER \
         (PARTITION BY ref ORDER BY at DESC, id DESC) AS n FROM ref_events \
         WHERE repo_id = ? AND at <= ? AND new_oid IS NOT NULL) WHERE n = 1 ORDER BY ref"
    ))
    .bind(repo_id)
    .bind(at)
    .fetch_all(pool)
    .await
}

/// For each ref, the first rewrite after `at` whose new commit is unknown:
/// its old commit is what the ref pointed to just before the rewrite.
pub async fn next_rewrites(
    pool: &SqlitePool,
    repo_id: i64,
    at: i64,
) -> sqlx::Result<Vec<RefEvent>> {
    sqlx::query_as(select_events!(
        "(SELECT *, ROW_NUMBER() OVER \
         (PARTITION BY ref ORDER BY at, id) AS n FROM ref_events \
         WHERE repo_id = ? AND at > ? AND kind = 'rewritten' AND new_oid IS NULL) \
         WHERE n = 1 ORDER BY ref"
    ))
    .bind(repo_id)
    .bind(at)
    .fetch_all(pool)
    .await
}

/// When the recording began: the time of the first `baseline` event.
pub async fn recording_since(pool: &SqlitePool, repo_id: i64) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar("SELECT MIN(at) FROM ref_events WHERE repo_id = ? AND kind = 'baseline'")
        .bind(repo_id)
        .fetch_one(pool)
        .await
}

/// Events up to `at`, newest first.
pub async fn list_until(
    pool: &SqlitePool,
    repo_id: i64,
    at: i64,
    limit: i64,
    offset: i64,
) -> sqlx::Result<Vec<RefEvent>> {
    sqlx::query_as(select_events!(
        "ref_events WHERE repo_id = ? AND at <= ? \
         ORDER BY at DESC, id DESC LIMIT ? OFFSET ?"
    ))
    .bind(repo_id)
    .bind(at)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

pub async fn count_until(pool: &SqlitePool, repo_id: i64, at: i64) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM ref_events WHERE repo_id = ? AND at <= ?")
        .bind(repo_id)
        .bind(at)
        .fetch_one(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;
    use crate::db::repos::{self, NewRepo};

    async fn pool_with_repo() -> (SqlitePool, i64) {
        let pool = open_in_memory().await.unwrap();
        let repo = NewRepo {
            url: "https://example.com/o/n",
            host: "example.com",
            owner: "o",
            name: "n",
            source_id: None,
            username: None,
            token_enc: None,
            interval_minutes: None,
            wiki_enabled: false,
        };
        let id = repos::insert(&pool, &repo, 1).await.unwrap().unwrap();
        (pool, id)
    }

    fn event<'a>(name: &'a str, new: Option<&'a str>, kind: EventKind, at: i64) -> NewEvent<'a> {
        NewEvent {
            name,
            old: None,
            new,
            kind,
            at,
        }
    }

    #[tokio::test]
    async fn state_at_gives_the_last_known_commit_of_each_ref() {
        let (pool, repo) = pool_with_repo().await;
        assert!(!has_any(&pool, repo).await.unwrap());
        let events = [
            event("refs/heads/main", None, EventKind::Rewritten, 50),
            event("refs/heads/main", Some("a"), EventKind::Baseline, 100),
            event("refs/tags/v1", Some("t"), EventKind::Baseline, 100),
            event("refs/heads/main", Some("b"), EventKind::Moved, 200),
            event("refs/heads/dev", Some("d"), EventKind::Created, 300),
        ];
        for e in &events {
            insert(&pool, repo, None, e).await.unwrap();
        }
        assert!(has_any(&pool, repo).await.unwrap());
        let at = |t| {
            let pool = pool.clone();
            async move {
                state_at(&pool, repo, t)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|e| (e.short().to_owned(), e.new_oid.unwrap()))
                    .collect::<Vec<_>>()
            }
        };
        assert!(at(99).await.is_empty());
        assert_eq!(
            at(150).await,
            [("main".into(), "a".into()), ("v1".into(), "t".into())]
        );
        assert_eq!(
            at(300).await,
            [
                ("dev".into(), "d".into()),
                ("main".into(), "b".into()),
                ("v1".into(), "t".into())
            ]
        );
        assert_eq!(recording_since(&pool, repo).await.unwrap(), Some(100));
        let rewrites = next_rewrites(&pool, repo, 10).await.unwrap();
        assert_eq!(rewrites.len(), 1);
        assert_eq!(rewrites[0].at, 50);
        assert!(next_rewrites(&pool, repo, 50).await.unwrap().is_empty());
        assert_eq!(count_until(&pool, repo, 200).await.unwrap(), 4);
        let page = list_until(&pool, repo, 300, 2, 0).await.unwrap();
        assert_eq!(page.iter().map(|e| e.at).collect::<Vec<_>>(), [300, 200]);
    }

    #[tokio::test]
    async fn missing_refs_are_added_once_and_removed() {
        let (pool, repo) = pool_with_repo().await;
        add_missing(&pool, repo, "refs/heads/old", 10)
            .await
            .unwrap();
        add_missing(&pool, repo, "refs/heads/old", 20)
            .await
            .unwrap();
        assert_eq!(
            missing(&pool, repo).await.unwrap(),
            [("refs/heads/old".to_owned(), 10)].into()
        );
        remove_missing(&pool, repo, "refs/heads/old").await.unwrap();
        assert!(missing(&pool, repo).await.unwrap().is_empty());
        assert_eq!(short_name("refs/tags/v1"), "v1");
        assert_eq!(short_name("refs/notes/x"), "refs/notes/x");
    }
}
