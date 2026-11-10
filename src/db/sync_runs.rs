//! History of sync attempts per repository.

use sqlx::{FromRow, SqlitePool};

use super::repos::RepoStatus;

#[derive(Debug, Clone, FromRow)]
pub struct SyncRun {
    pub id: i64,
    pub repo_id: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub outcome: Option<RepoStatus>,
    pub refs_changed: i64,
    pub refs_rescued: i64,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunResult<'a> {
    pub finished_at: i64,
    /// One of `Ok`, `Error` or `Unreachable`.
    pub outcome: RepoStatus,
    pub refs_changed: i64,
    pub refs_rescued: i64,
    pub message: Option<&'a str>,
}

/// Number of runs kept per repository; older ones are deleted.
pub const KEEP_PER_REPO: i64 = 200;

pub async fn start(pool: &SqlitePool, repo_id: i64, now: i64) -> sqlx::Result<i64> {
    sqlx::query_scalar("INSERT INTO sync_runs (repo_id, started_at) VALUES (?, ?) RETURNING id")
        .bind(repo_id)
        .bind(now)
        .fetch_one(pool)
        .await
}

pub async fn finish(pool: &SqlitePool, id: i64, result: &RunResult<'_>) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE sync_runs SET finished_at = ?, outcome = ?, refs_changed = ?, \
         refs_rescued = ?, message = ? WHERE id = ?",
    )
    .bind(result.finished_at)
    .bind(result.outcome)
    .bind(result.refs_changed)
    .bind(result.refs_rescued)
    .bind(result.message)
    .bind(id)
    .execute(pool)
    .await?;
    sqlx::query(
        "DELETE FROM sync_runs WHERE repo_id = (SELECT repo_id FROM sync_runs WHERE id = ?1) \
         AND id NOT IN (SELECT id FROM sync_runs WHERE repo_id = \
         (SELECT repo_id FROM sync_runs WHERE id = ?1) ORDER BY started_at DESC, id DESC LIMIT ?2)",
    )
    .bind(id)
    .bind(KEEP_PER_REPO)
    .execute(pool)
    .await?;
    Ok(())
}

/// Newest runs first.
pub async fn list_for_repo(
    pool: &SqlitePool,
    repo_id: i64,
    limit: i64,
) -> sqlx::Result<Vec<SyncRun>> {
    sqlx::query_as(
        "SELECT id, repo_id, started_at, finished_at, outcome, refs_changed, refs_rescued, message \
         FROM sync_runs WHERE repo_id = ? ORDER BY started_at DESC, id DESC LIMIT ?",
    )
    .bind(repo_id)
    .bind(limit)
    .fetch_all(pool)
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

    fn ok_result(at: i64) -> RunResult<'static> {
        RunResult {
            finished_at: at,
            outcome: RepoStatus::Ok,
            refs_changed: 3,
            refs_rescued: 1,
            message: None,
        }
    }

    #[tokio::test]
    async fn run_is_recorded_and_listed_newest_first() {
        let (pool, repo_id) = pool_with_repo().await;
        let first = start(&pool, repo_id, 10).await.unwrap();
        finish(&pool, first, &ok_result(11)).await.unwrap();
        let second = start(&pool, repo_id, 20).await.unwrap();

        let runs = list_for_repo(&pool, repo_id, 10).await.unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, second);
        assert!(runs[0].outcome.is_none());
        assert_eq!(runs[1].outcome, Some(RepoStatus::Ok));
        assert_eq!(runs[1].refs_rescued, 1);
    }

    #[tokio::test]
    async fn old_runs_are_trimmed() {
        let (pool, repo_id) = pool_with_repo().await;
        for at in 0..(KEEP_PER_REPO + 5) {
            let id = start(&pool, repo_id, at).await.unwrap();
            finish(&pool, id, &ok_result(at)).await.unwrap();
        }
        let runs = list_for_repo(&pool, repo_id, 1000).await.unwrap();
        assert_eq!(runs.len() as i64, KEEP_PER_REPO);
        assert_eq!(runs.last().unwrap().started_at, 5);
    }

    #[tokio::test]
    async fn deleting_the_repository_deletes_its_runs() {
        let (pool, repo_id) = pool_with_repo().await;
        start(&pool, repo_id, 1).await.unwrap();
        repos::delete(&pool, repo_id).await.unwrap();
        assert!(list_for_repo(&pool, repo_id, 10).await.unwrap().is_empty());
    }
}
