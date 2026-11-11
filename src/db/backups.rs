//! The last bundle upload of each repository.

use sqlx::SqlitePool;

/// The ref hash of the last upload of the repository, when there was one.
pub async fn refs_hash(pool: &SqlitePool, repo_id: i64) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar("SELECT refs_hash FROM repo_backups WHERE repo_id = ?")
        .bind(repo_id)
        .fetch_optional(pool)
        .await
}

pub async fn record(pool: &SqlitePool, repo_id: i64, at: i64, refs_hash: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO repo_backups (repo_id, backed_up_at, refs_hash) VALUES (?, ?, ?) \
         ON CONFLICT (repo_id) DO UPDATE SET backed_up_at = excluded.backed_up_at, \
         refs_hash = excluded.refs_hash",
    )
    .bind(repo_id)
    .bind(at)
    .bind(refs_hash)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;
    use crate::db::repos::{self, NewRepo};

    #[tokio::test]
    async fn the_last_upload_replaces_the_one_before() {
        let pool = open_in_memory().await.unwrap();
        let new = NewRepo {
            url: "https://example.com/o/n",
            host: "example.com",
            owner: "o",
            name: "n",
            source_id: None,
            username: None,
            token_enc: None,
            interval_minutes: None,
            wiki_enabled: true,
        };
        let id = repos::insert(&pool, &new, 1).await.unwrap().unwrap();
        assert_eq!(refs_hash(&pool, id).await.unwrap(), None);
        record(&pool, id, 10, "aaa").await.unwrap();
        record(&pool, id, 20, "bbb").await.unwrap();
        assert_eq!(refs_hash(&pool, id).await.unwrap().as_deref(), Some("bbb"));
        // Deleting the repository removes its record.
        repos::delete(&pool, id).await.unwrap();
        assert_eq!(refs_hash(&pool, id).await.unwrap(), None);
    }
}
