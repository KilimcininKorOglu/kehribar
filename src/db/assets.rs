//! Archived copies of the external images that Markdown files show.

use std::collections::HashSet;

use sqlx::{FromRow, SqlitePool};

/// One archived image of a repository.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Asset {
    /// The image URL as the Markdown file names it.
    pub url: String,
    pub sha256: String,
    pub content_type: String,
    pub bytes: i64,
    pub fetched_at: i64,
}

/// The result of the last image check of a repository.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, FromRow)]
pub struct Check {
    pub found: i64,
    /// Images left out because of the per-repository limit.
    pub skipped: i64,
    pub checked_at: i64,
}

pub async fn for_repo(pool: &SqlitePool, repo_id: i64) -> sqlx::Result<Vec<Asset>> {
    sqlx::query_as(
        "SELECT url, sha256, content_type, bytes, fetched_at FROM repo_assets \
         WHERE repo_id = ? ORDER BY url",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await
}

/// Adds or replaces the image of `asset.url`.
pub async fn upsert(pool: &SqlitePool, repo_id: i64, asset: &Asset) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO repo_assets (repo_id, url, sha256, content_type, bytes, fetched_at) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT (repo_id, url) DO UPDATE SET sha256 = excluded.sha256, \
         content_type = excluded.content_type, bytes = excluded.bytes, \
         fetched_at = excluded.fetched_at",
    )
    .bind(repo_id)
    .bind(&asset.url)
    .bind(&asset.sha256)
    .bind(&asset.content_type)
    .bind(asset.bytes)
    .bind(asset.fetched_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn remove(pool: &SqlitePool, repo_id: i64, url: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM repo_assets WHERE repo_id = ? AND url = ?")
        .bind(repo_id)
        .bind(url)
        .execute(pool)
        .await?;
    Ok(())
}

/// The content type of an image of the repository, by its hash; `None`
/// when the repository has no image with that content.
pub async fn content_type(
    pool: &SqlitePool,
    repo_id: i64,
    sha256: &str,
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT content_type FROM repo_assets WHERE repo_id = ? AND sha256 = ? LIMIT 1",
    )
    .bind(repo_id)
    .bind(sha256)
    .fetch_optional(pool)
    .await
}

/// The hashes that any repository still uses.
pub async fn used_hashes(pool: &SqlitePool) -> sqlx::Result<HashSet<String>> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT DISTINCT sha256 FROM repo_assets")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().collect())
}

pub async fn record_check(pool: &SqlitePool, repo_id: i64, check: Check) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO repo_asset_checks (repo_id, found, skipped, checked_at) VALUES (?, ?, ?, ?) \
         ON CONFLICT (repo_id) DO UPDATE SET found = excluded.found, \
         skipped = excluded.skipped, checked_at = excluded.checked_at",
    )
    .bind(repo_id)
    .bind(check.found)
    .bind(check.skipped)
    .bind(check.checked_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn last_check(pool: &SqlitePool, repo_id: i64) -> sqlx::Result<Option<Check>> {
    sqlx::query_as("SELECT found, skipped, checked_at FROM repo_asset_checks WHERE repo_id = ?")
        .bind(repo_id)
        .fetch_optional(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;
    use crate::db::repos::{self, NewRepo};

    async fn add_repo(pool: &SqlitePool, name: &str) -> i64 {
        let url = format!("https://example.com/a/{name}");
        let new = NewRepo {
            url: &url,
            host: "example.com",
            owner: "a",
            name,
            source_id: None,
            username: None,
            token_enc: None,
            interval_minutes: None,
            wiki_enabled: false,
        };
        repos::insert(pool, &new, 1).await.unwrap().unwrap()
    }

    fn asset(url: &str, sha256: &str, at: i64) -> Asset {
        Asset {
            url: url.to_owned(),
            sha256: sha256.to_owned(),
            content_type: "image/png".to_owned(),
            bytes: 3,
            fetched_at: at,
        }
    }

    #[tokio::test]
    async fn assets_are_replaced_by_url_and_go_with_their_repository() {
        let pool = open_in_memory().await.unwrap();
        let one = add_repo(&pool, "one").await;
        let two = add_repo(&pool, "two").await;
        upsert(&pool, one, &asset("https://x/a.png", "aa", 1))
            .await
            .unwrap();
        upsert(&pool, one, &asset("https://x/a.png", "bb", 2))
            .await
            .unwrap();
        upsert(&pool, two, &asset("https://x/c.png", "cc", 1))
            .await
            .unwrap();
        assert_eq!(
            for_repo(&pool, one).await.unwrap(),
            [asset("https://x/a.png", "bb", 2)]
        );
        assert_eq!(
            content_type(&pool, one, "bb").await.unwrap().as_deref(),
            Some("image/png")
        );
        // Another repository's image is not served under this one.
        assert_eq!(content_type(&pool, one, "cc").await.unwrap(), None);

        record_check(
            &pool,
            one,
            Check {
                found: 3,
                skipped: 1,
                checked_at: 5,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            last_check(&pool, one).await.unwrap().map(|c| c.skipped),
            Some(1)
        );

        repos::delete(&pool, one).await.unwrap();
        assert_eq!(
            used_hashes(&pool).await.unwrap(),
            HashSet::from(["cc".to_owned()])
        );
        assert_eq!(last_check(&pool, one).await.unwrap(), None);
        remove(&pool, two, "https://x/c.png").await.unwrap();
        assert!(used_hashes(&pool).await.unwrap().is_empty());
    }
}
