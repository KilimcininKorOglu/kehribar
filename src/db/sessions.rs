//! Login sessions. Only a hash of the session token is stored.

use sqlx::{FromRow, SqlitePool};

#[derive(Debug, Clone, FromRow)]
pub struct Session {
    pub token_hash: Vec<u8>,
    pub csrf_token: String,
    pub created_at: i64,
    pub expires_at: i64,
}

pub async fn create(
    pool: &SqlitePool,
    token_hash: &[u8],
    csrf_token: &str,
    now: i64,
    expires_at: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO sessions (token_hash, csrf_token, created_at, expires_at) VALUES (?, ?, ?, ?)",
    )
    .bind(token_hash)
    .bind(csrf_token)
    .bind(now)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Returns the session when it exists and has not expired.
pub async fn find_valid(
    pool: &SqlitePool,
    token_hash: &[u8],
    now: i64,
) -> sqlx::Result<Option<Session>> {
    sqlx::query_as(
        "SELECT token_hash, csrf_token, created_at, expires_at FROM sessions \
         WHERE token_hash = ? AND expires_at > ?",
    )
    .bind(token_hash)
    .bind(now)
    .fetch_optional(pool)
    .await
}

pub async fn delete(pool: &SqlitePool, token_hash: &[u8]) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(token_hash)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn purge_expired(pool: &SqlitePool, now: i64) -> sqlx::Result<u64> {
    let result = sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(now)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;

    #[tokio::test]
    async fn session_is_valid_until_it_expires() {
        let pool = open_in_memory().await.unwrap();
        create(&pool, b"hash", "csrf", 100, 200).await.unwrap();
        let session = find_valid(&pool, b"hash", 199).await.unwrap().unwrap();
        assert_eq!(session.csrf_token, "csrf");
        assert!(find_valid(&pool, b"hash", 200).await.unwrap().is_none());
        assert!(find_valid(&pool, b"other", 150).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_and_purge_remove_sessions() {
        let pool = open_in_memory().await.unwrap();
        create(&pool, b"a", "x", 0, 10).await.unwrap();
        create(&pool, b"b", "y", 0, 100).await.unwrap();
        assert_eq!(purge_expired(&pool, 50).await.unwrap(), 1);
        delete(&pool, b"b").await.unwrap();
        assert!(find_valid(&pool, b"b", 1).await.unwrap().is_none());
    }
}
