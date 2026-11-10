//! Key/value settings saved from the web UI.

use sqlx::SqlitePool;

pub async fn get(pool: &SqlitePool, key: &str) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await
}

pub async fn set(pool: &SqlitePool, key: &str, value: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?, ?) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Removes the saved value, so the environment value applies again.
pub async fn remove(pool: &SqlitePool, key: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM settings WHERE key = ?")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;

    #[tokio::test]
    async fn set_overwrites_and_remove_clears() {
        let pool = open_in_memory().await.unwrap();
        assert_eq!(get(&pool, "sync_interval_minutes").await.unwrap(), None);
        set(&pool, "sync_interval_minutes", "60").await.unwrap();
        set(&pool, "sync_interval_minutes", "90").await.unwrap();
        assert_eq!(
            get(&pool, "sync_interval_minutes")
                .await
                .unwrap()
                .as_deref(),
            Some("90")
        );
        remove(&pool, "sync_interval_minutes").await.unwrap();
        assert_eq!(get(&pool, "sync_interval_minutes").await.unwrap(), None);
    }
}
