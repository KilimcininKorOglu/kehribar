//! Key/value settings saved from the web UI.

use base64::prelude::{BASE64_STANDARD, Engine as _};
use sqlx::SqlitePool;

use crate::crypto::Cipher;

/// What a save does with a stored secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretChange {
    Keep,
    /// The encrypted new secret.
    Set(Vec<u8>),
    Clear,
}

/// Stores an encrypted secret as base64 text, or removes it.
pub async fn put_secret(pool: &SqlitePool, key: &str, change: &SecretChange) -> sqlx::Result<()> {
    match change {
        SecretChange::Keep => Ok(()),
        SecretChange::Set(enc) => set(pool, key, &BASE64_STANDARD.encode(enc)).await,
        SecretChange::Clear => remove(pool, key).await,
    }
}

/// Decrypts a stored secret. The error says which one is damaged.
pub async fn get_secret(
    pool: &SqlitePool,
    cipher: &Cipher,
    key: &str,
) -> Result<Option<String>, String> {
    let Some(text) = get(pool, key).await.map_err(|err| err.to_string())? else {
        return Ok(None);
    };
    let enc = BASE64_STANDARD
        .decode(text)
        .map_err(|err| format!("stored {key} is damaged: {err}"))?;
    cipher
        .decrypt(&enc)
        .map(Some)
        .map_err(|err| format!("cannot decrypt the stored {key}: {err}"))
}

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

/// Saves `value`, or removes the saved value when it is `None`.
pub async fn put(pool: &SqlitePool, key: &str, value: Option<&str>) -> sqlx::Result<()> {
    match value {
        Some(value) => set(pool, key, value).await,
        None => remove(pool, key).await,
    }
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
