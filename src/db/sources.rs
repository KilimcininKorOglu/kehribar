//! Watched users and organizations whose repositories are imported.

use sqlx::{FromRow, SqlitePool};

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
    pub auto_add: bool,
    pub last_scan_at: Option<i64>,
    pub last_error: Option<String>,
    pub created_at: i64,
}

/// Builds a `SELECT` of every source column at compile time.
macro_rules! select_sources {
    ($tail:literal) => {
        concat!(
            "SELECT id, host_id, kind, name, auto_add, last_scan_at, last_error, created_at \
             FROM sources ",
            $tail
        )
    };
}

pub async fn insert(
    pool: &SqlitePool,
    host_id: i64,
    kind: SourceKind,
    name: &str,
    auto_add: bool,
    now: i64,
) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "INSERT INTO sources (host_id, kind, name, auto_add, created_at) \
         VALUES (?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(host_id)
    .bind(kind)
    .bind(name)
    .bind(auto_add)
    .bind(now)
    .fetch_one(pool)
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

/// Sources whose new repositories are added automatically.
pub async fn list_auto_add(pool: &SqlitePool) -> sqlx::Result<Vec<Source>> {
    sqlx::query_as(select_sources!("WHERE auto_add = 1 ORDER BY id"))
        .fetch_all(pool)
        .await
}

pub async fn set_auto_add(pool: &SqlitePool, id: i64, auto_add: bool) -> sqlx::Result<bool> {
    let result = sqlx::query("UPDATE sources SET auto_add = ? WHERE id = ?")
        .bind(auto_add)
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

    #[tokio::test]
    async fn scan_state_and_auto_add_are_stored() {
        let (pool, host_id) = pool_with_host().await;
        let id = insert(&pool, host_id, SourceKind::Org, "forgejo", true, 5)
            .await
            .unwrap();
        assert_eq!(list_auto_add(&pool).await.unwrap().len(), 1);

        mark_scanned(&pool, id, 9, Some("rate limited"))
            .await
            .unwrap();
        let source = get(&pool, id).await.unwrap().unwrap();
        assert_eq!(source.last_scan_at, Some(9));
        assert_eq!(source.last_error.as_deref(), Some("rate limited"));

        assert!(set_auto_add(&pool, id, false).await.unwrap());
        assert!(list_auto_add(&pool).await.unwrap().is_empty());
        assert_eq!(list(&pool).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deleting_the_host_deletes_its_sources() {
        let (pool, host_id) = pool_with_host().await;
        let id = insert(&pool, host_id, SourceKind::User, "alice", true, 5)
            .await
            .unwrap();
        hosts::delete(&pool, host_id).await.unwrap();
        assert!(get(&pool, id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn duplicate_source_is_rejected() {
        let (pool, host_id) = pool_with_host().await;
        insert(&pool, host_id, SourceKind::User, "alice", true, 5)
            .await
            .unwrap();
        assert!(
            insert(&pool, host_id, SourceKind::User, "ALICE", true, 5)
                .await
                .is_err()
        );
    }
}
