//! Backup storage and schedule: values saved on the settings page win over
//! the environment. The secret access key is stored encrypted.

use std::ops::RangeInclusive;

use sqlx::SqlitePool;

use crate::config::Config;
use crate::crypto::Cipher;
use crate::db::settings::{self as store, SecretChange};

const KEY_ENDPOINT: &str = "backup_s3_endpoint";
const KEY_REGION: &str = "backup_s3_region";
const KEY_BUCKET: &str = "backup_s3_bucket";
const KEY_PREFIX: &str = "backup_s3_prefix";
const KEY_ACCESS_KEY_ID: &str = "backup_s3_access_key_id";
const KEY_SECRET: &str = "backup_s3_secret_access_key";
const KEY_PATH_STYLE: &str = "backup_s3_path_style";
const KEY_INTERVAL: &str = "backup_interval_hours";
const KEY_KEEP: &str = "backup_keep";
const KEY_LAST_AT: &str = "backup_last_run_at";
const KEY_LAST_UPLOADED: &str = "backup_last_uploaded";
const KEY_LAST_ERROR: &str = "backup_last_error";

/// One week between backups at most, and at most 365 bundles a repository.
pub const INTERVAL_RANGE: RangeInclusive<i64> = 1..=168;
pub const KEEP_RANGE: RangeInclusive<i64> = 1..=365;

/// The values saved on the settings page; `None` means the environment applies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Saved {
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub bucket: Option<String>,
    pub prefix: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_stored: bool,
    pub path_style: Option<bool>,
    pub interval_hours: Option<i64>,
    pub keep: Option<i64>,
}

/// Where the bundles go.
#[derive(Clone)]
pub struct Target {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// Without leading and trailing slashes; may be empty.
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub path_style: bool,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("path_style", &self.path_style)
            .finish()
    }
}

/// The settings in force.
#[derive(Debug, Clone)]
pub struct BackupSettings {
    /// `None` until the endpoint, the bucket and both keys are known.
    pub target: Option<Target>,
    pub interval_hours: i64,
    pub keep: i64,
}

/// The outcome of the last backup run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastRun {
    pub at: i64,
    pub uploaded: i64,
    pub error: Option<String>,
}

/// The fields a save writes, already checked.
#[derive(Debug, Clone, Default)]
pub struct SaveInput<'a> {
    pub endpoint: Option<&'a str>,
    pub region: Option<&'a str>,
    pub bucket: Option<&'a str>,
    pub prefix: Option<&'a str>,
    pub access_key_id: Option<&'a str>,
    pub path_style: Option<bool>,
    pub interval_hours: Option<i64>,
    pub keep: Option<i64>,
}

pub async fn saved(pool: &SqlitePool) -> sqlx::Result<Saved> {
    Ok(Saved {
        endpoint: store::get(pool, KEY_ENDPOINT).await?,
        region: store::get(pool, KEY_REGION).await?,
        bucket: store::get(pool, KEY_BUCKET).await?,
        prefix: store::get(pool, KEY_PREFIX).await?,
        access_key_id: store::get(pool, KEY_ACCESS_KEY_ID).await?,
        secret_stored: store::get(pool, KEY_SECRET).await?.is_some(),
        path_style: store::get(pool, KEY_PATH_STYLE)
            .await?
            .map(|value| value == "true"),
        interval_hours: number(pool, KEY_INTERVAL, INTERVAL_RANGE).await?,
        keep: number(pool, KEY_KEEP, KEEP_RANGE).await?,
    })
}

/// A stored number outside its range (edited by hand) is ignored.
async fn number(
    pool: &SqlitePool,
    key: &str,
    range: RangeInclusive<i64>,
) -> sqlx::Result<Option<i64>> {
    Ok(store::get(pool, key)
        .await?
        .and_then(|text| text.parse().ok())
        .filter(|value| range.contains(value)))
}

pub async fn save(
    pool: &SqlitePool,
    input: &SaveInput<'_>,
    secret: &SecretChange,
) -> sqlx::Result<()> {
    store::put(pool, KEY_ENDPOINT, input.endpoint).await?;
    store::put(pool, KEY_REGION, input.region).await?;
    store::put(pool, KEY_BUCKET, input.bucket).await?;
    store::put(pool, KEY_PREFIX, input.prefix).await?;
    store::put(pool, KEY_ACCESS_KEY_ID, input.access_key_id).await?;
    let path_style = input.path_style.map(|on| if on { "true" } else { "false" });
    store::put(pool, KEY_PATH_STYLE, path_style).await?;
    let interval = input.interval_hours.map(|n| n.to_string());
    store::put(pool, KEY_INTERVAL, interval.as_deref()).await?;
    let keep = input.keep.map(|n| n.to_string());
    store::put(pool, KEY_KEEP, keep.as_deref()).await?;
    store::put_secret(pool, KEY_SECRET, secret).await
}

/// The settings in force: saved values first, then the environment.
pub async fn effective(
    pool: &SqlitePool,
    config: &Config,
    cipher: &Cipher,
) -> Result<BackupSettings, String> {
    let env = &config.backup;
    let saved = saved(pool).await.map_err(|err| err.to_string())?;
    let secret = match store::get_secret(pool, cipher, KEY_SECRET).await? {
        Some(secret) => Some(secret),
        None => env.secret_access_key.clone(),
    };
    let required = (
        saved.endpoint.or_else(|| env.endpoint.clone()),
        saved.bucket.or_else(|| env.bucket.clone()),
        saved.access_key_id.or_else(|| env.access_key_id.clone()),
        secret,
    );
    let target = match required {
        (Some(endpoint), Some(bucket), Some(access_key_id), Some(secret_access_key)) => {
            Some(Target {
                endpoint,
                region: saved.region.unwrap_or_else(|| env.region.clone()),
                bucket,
                prefix: clean_prefix(saved.prefix.as_deref().unwrap_or(&env.prefix)),
                access_key_id,
                secret_access_key,
                path_style: saved.path_style.unwrap_or(env.path_style),
            })
        }
        _ => None,
    };
    Ok(BackupSettings {
        target,
        interval_hours: saved
            .interval_hours
            .unwrap_or(i64::from(env.interval_hours)),
        keep: saved.keep.unwrap_or(i64::from(env.keep)),
    })
}

fn clean_prefix(prefix: &str) -> String {
    prefix.trim().trim_matches('/').to_owned()
}

pub async fn record_run(pool: &SqlitePool, run: &LastRun) -> sqlx::Result<()> {
    store::set(pool, KEY_LAST_AT, &run.at.to_string()).await?;
    store::set(pool, KEY_LAST_UPLOADED, &run.uploaded.to_string()).await?;
    store::put(pool, KEY_LAST_ERROR, run.error.as_deref()).await
}

pub async fn last_run(pool: &SqlitePool) -> sqlx::Result<Option<LastRun>> {
    let Some(at) = store::get(pool, KEY_LAST_AT).await? else {
        return Ok(None);
    };
    let uploaded = store::get(pool, KEY_LAST_UPLOADED).await?;
    Ok(Some(LastRun {
        at: at.parse().unwrap_or_default(),
        uploaded: uploaded.and_then(|n| n.parse().ok()).unwrap_or_default(),
        error: store::get(pool, KEY_LAST_ERROR).await?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;

    fn config() -> Config {
        Config::from_lookup(|name| match name {
            "KEHRIBAR_ADMIN_USERNAME" | "KEHRIBAR_ADMIN_PASSWORD" => Some("x".into()),
            "KEHRIBAR_SECRET_KEY" => Some("00".repeat(32)),
            "KEHRIBAR_BACKUP_S3_ENDPOINT" => Some("https://s3.example".into()),
            "KEHRIBAR_BACKUP_S3_BUCKET" => Some("env-bucket".into()),
            "KEHRIBAR_BACKUP_S3_ACCESS_KEY_ID" => Some("env-key".into()),
            _ => None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn the_target_needs_every_value_and_saved_values_win() {
        let pool = open_in_memory().await.unwrap();
        let config = config();
        let cipher = Cipher::new(&config.secret_key);

        // The environment has no secret key yet.
        let settings = effective(&pool, &config, &cipher).await.unwrap();
        assert!(settings.target.is_none());
        assert_eq!((settings.interval_hours, settings.keep), (24, 7));

        let input = SaveInput {
            bucket: Some("page-bucket"),
            prefix: Some("/archive/"),
            path_style: Some(true),
            keep: Some(3),
            ..SaveInput::default()
        };
        let secret = SecretChange::Set(cipher.encrypt("page-secret").unwrap());
        save(&pool, &input, &secret).await.unwrap();
        let settings = effective(&pool, &config, &cipher).await.unwrap();
        let target = settings.target.unwrap();
        assert_eq!(target.endpoint, "https://s3.example");
        assert_eq!(target.bucket, "page-bucket");
        assert_eq!(target.prefix, "archive");
        assert_eq!(target.access_key_id, "env-key");
        assert_eq!(target.secret_access_key, "page-secret");
        assert!(target.path_style);
        assert_eq!(settings.keep, 3);
        assert!(!format!("{target:?}").contains("page-secret"));

        save(&pool, &SaveInput::default(), &SecretChange::Clear)
            .await
            .unwrap();
        assert_eq!(saved(&pool).await.unwrap(), Saved::default());
    }

    #[tokio::test]
    async fn the_last_run_is_stored() {
        let pool = open_in_memory().await.unwrap();
        assert_eq!(last_run(&pool).await.unwrap(), None);
        let run = LastRun {
            at: 9,
            uploaded: 2,
            error: Some("x".into()),
        };
        record_run(&pool, &run).await.unwrap();
        assert_eq!(last_run(&pool).await.unwrap(), Some(run));
        let clean = LastRun {
            at: 10,
            uploaded: 0,
            error: None,
        };
        record_run(&pool, &clean).await.unwrap();
        assert_eq!(last_run(&pool).await.unwrap(), Some(clean));
    }
}
