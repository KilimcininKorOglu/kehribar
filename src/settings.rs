//! Sync settings: the environment gives the defaults, and values saved from
//! the settings page override them.

use std::ops::RangeInclusive;

use sqlx::SqlitePool;

use crate::config::Config;
use crate::db::settings as store;

pub const KEY_INTERVAL: &str = "sync_interval_minutes";
pub const KEY_CONCURRENCY: &str = "sync_concurrency";
pub const KEY_GIT_TIMEOUT: &str = "git_timeout_seconds";
pub const KEY_FSCK_INTERVAL: &str = "fsck_interval_days";
pub const KEY_REPACK_INTERVAL: &str = "repack_interval_days";

/// Accepted values; one week, 32 parallel syncs and one day are the upper
/// bounds a single archive host needs.
pub const INTERVAL_RANGE: RangeInclusive<i64> = 1..=10_080;
pub const CONCURRENCY_RANGE: RangeInclusive<i64> = 1..=32;
pub const GIT_TIMEOUT_RANGE: RangeInclusive<i64> = 10..=86_400;
/// Days between maintenance runs; 0 turns the task off.
pub const MAINTENANCE_RANGE: RangeInclusive<i64> = 0..=365;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncSettings {
    pub interval_minutes: i64,
    pub concurrency: i64,
    pub git_timeout_seconds: i64,
    pub fsck_interval_days: i64,
    pub repack_interval_days: i64,
}

impl SyncSettings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            interval_minutes: i64::from(config.sync_interval_minutes),
            concurrency: i64::from(config.sync_concurrency),
            git_timeout_seconds: i64::try_from(config.git_timeout_seconds).unwrap_or(i64::MAX),
            fsck_interval_days: i64::from(config.fsck_interval_days),
            repack_interval_days: i64::from(config.repack_interval_days),
        }
    }
}

/// Values saved on the settings page; `None` means the environment applies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Overrides {
    pub interval_minutes: Option<i64>,
    pub concurrency: Option<i64>,
    pub git_timeout_seconds: Option<i64>,
    pub fsck_interval_days: Option<i64>,
    pub repack_interval_days: Option<i64>,
}

pub async fn overrides(pool: &SqlitePool) -> sqlx::Result<Overrides> {
    Ok(Overrides {
        interval_minutes: read(pool, KEY_INTERVAL, INTERVAL_RANGE).await?,
        concurrency: read(pool, KEY_CONCURRENCY, CONCURRENCY_RANGE).await?,
        git_timeout_seconds: read(pool, KEY_GIT_TIMEOUT, GIT_TIMEOUT_RANGE).await?,
        fsck_interval_days: read(pool, KEY_FSCK_INTERVAL, MAINTENANCE_RANGE).await?,
        repack_interval_days: read(pool, KEY_REPACK_INTERVAL, MAINTENANCE_RANGE).await?,
    })
}

/// The settings in force: saved values first, then the environment.
pub async fn effective(pool: &SqlitePool, config: &Config) -> sqlx::Result<SyncSettings> {
    let defaults = SyncSettings::from_config(config);
    let saved = overrides(pool).await?;
    Ok(SyncSettings {
        interval_minutes: saved.interval_minutes.unwrap_or(defaults.interval_minutes),
        concurrency: saved.concurrency.unwrap_or(defaults.concurrency),
        git_timeout_seconds: saved
            .git_timeout_seconds
            .unwrap_or(defaults.git_timeout_seconds),
        fsck_interval_days: saved
            .fsck_interval_days
            .unwrap_or(defaults.fsck_interval_days),
        repack_interval_days: saved
            .repack_interval_days
            .unwrap_or(defaults.repack_interval_days),
    })
}

/// Saves the overrides; a `None` field removes the saved value.
pub async fn save(pool: &SqlitePool, values: &Overrides) -> sqlx::Result<()> {
    write(pool, KEY_INTERVAL, values.interval_minutes).await?;
    write(pool, KEY_CONCURRENCY, values.concurrency).await?;
    write(pool, KEY_GIT_TIMEOUT, values.git_timeout_seconds).await?;
    write(pool, KEY_FSCK_INTERVAL, values.fsck_interval_days).await?;
    write(pool, KEY_REPACK_INTERVAL, values.repack_interval_days).await
}

/// A stored value outside its range (edited by hand) is ignored.
async fn read(
    pool: &SqlitePool,
    key: &str,
    range: RangeInclusive<i64>,
) -> sqlx::Result<Option<i64>> {
    let value = store::get(pool, key).await?;
    Ok(value
        .and_then(|text| text.parse().ok())
        .filter(|value| range.contains(value)))
}

async fn write(pool: &SqlitePool, key: &str, value: Option<i64>) -> sqlx::Result<()> {
    match value {
        Some(value) => store::set(pool, key, &value.to_string()).await,
        None => store::remove(pool, key).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;

    fn config() -> Config {
        Config::from_lookup(|name| match name {
            "KEHRIBAR_ADMIN_USERNAME" | "KEHRIBAR_ADMIN_PASSWORD" => Some("x".into()),
            "KEHRIBAR_SECRET_KEY" => Some("00".repeat(32)),
            "KEHRIBAR_SYNC_INTERVAL_MINUTES" => Some("120".into()),
            _ => None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn saved_values_override_the_environment_until_cleared() {
        let pool = open_in_memory().await.unwrap();
        let config = config();
        assert_eq!(
            effective(&pool, &config).await.unwrap().interval_minutes,
            120
        );

        let saved = Overrides {
            interval_minutes: Some(15),
            concurrency: Some(4),
            git_timeout_seconds: None,
            fsck_interval_days: Some(0),
            repack_interval_days: None,
        };
        save(&pool, &saved).await.unwrap();
        let settings = effective(&pool, &config).await.unwrap();
        assert_eq!(settings.interval_minutes, 15);
        assert_eq!(settings.concurrency, 4);
        assert_eq!(settings.git_timeout_seconds, 3600);
        assert_eq!(settings.fsck_interval_days, 0);
        assert_eq!(settings.repack_interval_days, 7);
        assert_eq!(overrides(&pool).await.unwrap(), saved);

        save(&pool, &Overrides::default()).await.unwrap();
        assert_eq!(
            effective(&pool, &config).await.unwrap().interval_minutes,
            120
        );
    }

    #[tokio::test]
    async fn out_of_range_stored_value_is_ignored() {
        let pool = open_in_memory().await.unwrap();
        store::set(&pool, KEY_CONCURRENCY, "0").await.unwrap();
        store::set(&pool, KEY_INTERVAL, "abc").await.unwrap();
        store::set(&pool, KEY_REPACK_INTERVAL, "-1").await.unwrap();
        assert_eq!(overrides(&pool).await.unwrap(), Overrides::default());
    }
}
