//! SQLite storage for repository metadata, credentials, settings and sessions.

pub mod backups;
pub mod hosts;
pub mod repos;
pub mod sessions;
pub mod settings;
pub mod sources;
pub mod sync_runs;

use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

static MIGRATOR: Migrator = sqlx::migrate!();

/// Opens (and creates when missing) the database file and applies migrations.
pub async fn open(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(10));
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

/// Opens a private in-memory database with the schema applied.
pub async fn open_in_memory() -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
    // Every in-memory connection is a separate database, so keep exactly one.
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

/// Current time in seconds since the Unix epoch, the unit of every stored timestamp.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Escapes `%`, `_` and `\` so `term` matches literally in `LIKE ? ESCAPE '\'`.
pub(crate) fn like_pattern(term: &str) -> String {
    let mut pattern = String::with_capacity(term.len() + 2);
    pattern.push('%');
    for ch in term.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("a_b%c\\"), "%a\\_b\\%c\\\\%");
    }

    #[tokio::test]
    async fn file_database_is_created_and_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let pool = open(&path).await.unwrap();
        let tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN \
             ('hosts', 'sources', 'repos', 'sync_runs', 'settings', 'sessions')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(tables, 6);
        pool.close().await;
        // Opening again must not re-apply migrations or fail.
        open(&path).await.unwrap();
    }
}
