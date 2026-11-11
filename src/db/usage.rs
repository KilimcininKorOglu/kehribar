//! The archive size per day, for the disk page.

use sqlx::{FromRow, SqlitePool};

/// Days a daily row is kept.
pub const KEEP_DAYS: i64 = 400;

/// Seconds in a day.
pub const DAY_SECONDS: i64 = 86_400;

/// The size of the archive on one day.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct DayUsage {
    /// `YYYY-MM-DD` in UTC.
    pub day: String,
    /// Days since 1970-01-01.
    pub epoch_day: i64,
    pub total_bytes: i64,
    pub lfs_bytes: i64,
    pub repo_count: i64,
}

/// The size of the archive now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromRow)]
pub struct Totals {
    pub total_bytes: i64,
    pub lfs_bytes: i64,
    pub repo_count: i64,
}

/// Writes the row of the UTC day of `now` from the current repository
/// sizes, unless the day has a row already, and deletes the rows older
/// than [`KEEP_DAYS`]. Returns whether a row was written.
pub async fn record_day(pool: &SqlitePool, now: i64) -> sqlx::Result<bool> {
    let written = sqlx::query(
        "INSERT INTO disk_usage_daily (day, total_bytes, lfs_bytes, repo_count) \
         SELECT date(?1, 'unixepoch'), COALESCE(SUM(size_bytes), 0), \
                COALESCE(SUM(lfs_size_bytes), 0), COUNT(*) FROM repos \
         WHERE true ON CONFLICT (day) DO NOTHING",
    )
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected()
        == 1;
    sqlx::query("DELETE FROM disk_usage_daily WHERE day < date(?1, 'unixepoch', ?2)")
        .bind(now)
        .bind(format!("-{KEEP_DAYS} days"))
        .execute(pool)
        .await?;
    Ok(written)
}

/// The rows of the last `days` days up to the day of `now`, oldest first.
pub async fn last_days(pool: &SqlitePool, now: i64, days: i64) -> sqlx::Result<Vec<DayUsage>> {
    sqlx::query_as(
        "SELECT day, CAST(julianday(day) - 2440587.5 AS INTEGER) AS epoch_day, \
                total_bytes, lfs_bytes, repo_count \
         FROM disk_usage_daily WHERE day > date(?1, 'unixepoch', ?2) ORDER BY day",
    )
    .bind(now)
    .bind(format!("-{days} days"))
    .fetch_all(pool)
    .await
}

/// The size of every repository together, as the last syncs measured it.
pub async fn totals(pool: &SqlitePool) -> sqlx::Result<Totals> {
    sqlx::query_as(
        "SELECT COALESCE(SUM(size_bytes), 0) AS total_bytes, \
                COALESCE(SUM(lfs_size_bytes), 0) AS lfs_bytes, COUNT(*) AS repo_count \
         FROM repos",
    )
    .fetch_one(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;
    use crate::db::repos::{self, NewRepo, RepoStatus, SyncOutcome};

    async fn add_repo(pool: &SqlitePool, name: &str, size: i64, lfs: i64) {
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
        let id = repos::insert(pool, &new, 1).await.unwrap().unwrap();
        let outcome = SyncOutcome {
            status: RepoStatus::Ok,
            finished_at: 1,
            next_sync_at: 2,
            error: None,
            size_bytes: size,
            lfs_size_bytes: lfs,
            default_branch: None,
            has_wiki: false,
        };
        repos::finish(pool, id, &outcome).await.unwrap();
    }

    /// 2025-11-11 12:00 UTC.
    const NOON: i64 = 1_762_862_400;

    #[tokio::test]
    async fn a_day_is_recorded_once_with_the_sizes_of_its_first_tick() {
        let pool = open_in_memory().await.unwrap();
        add_repo(&pool, "one", 1000, 200).await;
        assert!(record_day(&pool, NOON).await.unwrap());
        add_repo(&pool, "two", 500, 0).await;
        // The second tick of the same day keeps the first row.
        assert!(!record_day(&pool, NOON + 3600).await.unwrap());
        let days = last_days(&pool, NOON, 90).await.unwrap();
        assert_eq!(
            days,
            [DayUsage {
                day: "2025-11-11".into(),
                epoch_day: NOON / DAY_SECONDS,
                total_bytes: 1000,
                lfs_bytes: 200,
                repo_count: 1,
            }]
        );
        // The next day gets its own row with the new sizes.
        assert!(record_day(&pool, NOON + DAY_SECONDS).await.unwrap());
        let days = last_days(&pool, NOON + DAY_SECONDS, 90).await.unwrap();
        assert_eq!(days.len(), 2);
        assert_eq!((days[1].total_bytes, days[1].repo_count), (1500, 2));
        assert_eq!(
            totals(&pool).await.unwrap(),
            Totals {
                total_bytes: 1500,
                lfs_bytes: 200,
                repo_count: 2
            }
        );
    }

    #[tokio::test]
    async fn rows_older_than_the_kept_days_are_deleted() {
        let pool = open_in_memory().await.unwrap();
        let old = NOON - (KEEP_DAYS + 1) * DAY_SECONDS;
        let kept = NOON - KEEP_DAYS * DAY_SECONDS;
        record_day(&pool, old).await.unwrap();
        record_day(&pool, kept).await.unwrap();
        record_day(&pool, NOON).await.unwrap();
        let all = last_days(&pool, NOON, KEEP_DAYS + 10).await.unwrap();
        let epoch_days: Vec<_> = all.iter().map(|d| d.epoch_day).collect();
        assert_eq!(epoch_days, [kept / DAY_SECONDS, NOON / DAY_SECONDS]);
        // The chart window holds only the last 90 days.
        assert_eq!(last_days(&pool, NOON, 90).await.unwrap().len(), 1);
        // An empty archive records a zero row, not an error.
        assert_eq!(all[1].total_bytes, 0);
    }
}
