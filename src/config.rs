//! Runtime configuration. Every setting comes from an environment variable,
//! so Docker Compose, Coolify and Dokploy can set each value directly.

use std::fmt;
use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::str::FromStr;

use thiserror::Error;

use crate::i18n::Lang;

pub const ENV_ADMIN_USERNAME: &str = "GIT_ARCHIVE_ADMIN_USERNAME";
pub const ENV_ADMIN_PASSWORD: &str = "GIT_ARCHIVE_ADMIN_PASSWORD";
pub const ENV_SECRET_KEY: &str = "GIT_ARCHIVE_SECRET_KEY";
pub const ENV_LISTEN_ADDR: &str = "GIT_ARCHIVE_LISTEN_ADDR";
pub const ENV_DATA_DIR: &str = "GIT_ARCHIVE_DATA_DIR";
pub const ENV_SYNC_INTERVAL_MINUTES: &str = "GIT_ARCHIVE_SYNC_INTERVAL_MINUTES";
pub const ENV_SYNC_CONCURRENCY: &str = "GIT_ARCHIVE_SYNC_CONCURRENCY";
pub const ENV_GIT_TIMEOUT_SECONDS: &str = "GIT_ARCHIVE_GIT_TIMEOUT_SECONDS";
pub const ENV_FSCK_INTERVAL_DAYS: &str = "GIT_ARCHIVE_FSCK_INTERVAL_DAYS";
pub const ENV_REPACK_INTERVAL_DAYS: &str = "GIT_ARCHIVE_REPACK_INTERVAL_DAYS";
pub const ENV_SESSION_TTL_HOURS: &str = "GIT_ARCHIVE_SESSION_TTL_HOURS";
pub const ENV_COOKIE_SECURE: &str = "GIT_ARCHIVE_COOKIE_SECURE";
pub const ENV_LOG_LEVEL: &str = "GIT_ARCHIVE_LOG_LEVEL";
pub const ENV_PUBLIC_URL: &str = "GIT_ARCHIVE_PUBLIC_URL";
pub const ENV_NOTIFY_WEBHOOK_URL: &str = "GIT_ARCHIVE_NOTIFY_WEBHOOK_URL";
pub const ENV_NOTIFY_TELEGRAM_BOT_TOKEN: &str = "GIT_ARCHIVE_NOTIFY_TELEGRAM_BOT_TOKEN";
pub const ENV_NOTIFY_TELEGRAM_CHAT_ID: &str = "GIT_ARCHIVE_NOTIFY_TELEGRAM_CHAT_ID";
pub const ENV_NOTIFY_TELEGRAM_API_URL: &str = "GIT_ARCHIVE_NOTIFY_TELEGRAM_API_URL";
pub const ENV_NOTIFY_LANG: &str = "GIT_ARCHIVE_NOTIFY_LANG";

pub const ENV_BACKUP_S3_ENDPOINT: &str = "GIT_ARCHIVE_BACKUP_S3_ENDPOINT";
pub const ENV_BACKUP_S3_REGION: &str = "GIT_ARCHIVE_BACKUP_S3_REGION";
pub const ENV_BACKUP_S3_BUCKET: &str = "GIT_ARCHIVE_BACKUP_S3_BUCKET";
pub const ENV_BACKUP_S3_PREFIX: &str = "GIT_ARCHIVE_BACKUP_S3_PREFIX";
pub const ENV_BACKUP_S3_ACCESS_KEY_ID: &str = "GIT_ARCHIVE_BACKUP_S3_ACCESS_KEY_ID";
pub const ENV_BACKUP_S3_SECRET_ACCESS_KEY: &str = "GIT_ARCHIVE_BACKUP_S3_SECRET_ACCESS_KEY";
pub const ENV_BACKUP_S3_PATH_STYLE: &str = "GIT_ARCHIVE_BACKUP_S3_PATH_STYLE";
pub const ENV_BACKUP_INTERVAL_HOURS: &str = "GIT_ARCHIVE_BACKUP_INTERVAL_HOURS";
pub const ENV_BACKUP_KEEP: &str = "GIT_ARCHIVE_BACKUP_KEEP";

/// Telegram Bot API used when `GIT_ARCHIVE_NOTIFY_TELEGRAM_API_URL` is not set.
pub const DEFAULT_TELEGRAM_API_URL: &str = "https://api.telegram.org";

/// Port of the web UI when `GIT_ARCHIVE_LISTEN_ADDR` is not set.
pub const DEFAULT_PORT: u16 = 8600;

/// Length in bytes of the key that encrypts stored access tokens.
pub const SECRET_KEY_LEN: usize = 32;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("required environment variable {0} is not set")]
    Missing(&'static str),
    #[error("environment variable {name} has an invalid value: {reason}")]
    Invalid { name: &'static str, reason: String },
}

/// Application settings resolved at startup.
#[derive(Clone)]
pub struct Config {
    pub admin_username: String,
    pub admin_password: String,
    pub secret_key: [u8; SECRET_KEY_LEN],
    /// `None` listens on both IPv6 and IPv4 at [`DEFAULT_PORT`].
    pub listen_addr: Option<SocketAddr>,
    pub data_dir: PathBuf,
    pub sync_interval_minutes: u32,
    pub sync_concurrency: u32,
    pub git_timeout_seconds: u64,
    /// Days between integrity checks of a repository; 0 turns them off.
    pub fsck_interval_days: u32,
    /// Days between repacks of a repository; 0 turns them off.
    pub repack_interval_days: u32,
    pub session_ttl_hours: u32,
    pub cookie_secure: bool,
    pub log_level: String,
    /// Address of this site as users open it, without a trailing slash.
    /// Notifications link to it when it is set.
    pub public_url: Option<String>,
    pub notify: NotifyConfig,
    pub backup: BackupConfig,
}

/// The S3-compatible storage that receives the bundles, from the
/// environment; the settings page can override each value.
#[derive(Clone)]
pub struct BackupConfig {
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: Option<String>,
    pub prefix: String,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    /// `https://endpoint/bucket/key` instead of `https://bucket.endpoint/key`;
    /// MinIO needs it.
    pub path_style: bool,
    pub interval_hours: u32,
    /// Bundles kept per repository.
    pub keep: u32,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            region: String::from("us-east-1"),
            bucket: None,
            prefix: String::new(),
            access_key_id: None,
            secret_access_key: None,
            path_style: false,
            interval_hours: 24,
            keep: 7,
        }
    }
}

impl fmt::Debug for BackupConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackupConfig")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("access_key_id", &self.access_key_id)
            .field(
                "secret_access_key",
                &self.secret_access_key.as_ref().map(|_| "<redacted>"),
            )
            .field("path_style", &self.path_style)
            .field("interval_hours", &self.interval_hours)
            .field("keep", &self.keep)
            .finish()
    }
}

/// Notification channels from the environment; the settings page can
/// override each value.
#[derive(Clone)]
pub struct NotifyConfig {
    pub webhook_url: Option<String>,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    pub telegram_api_url: String,
    pub lang: Lang,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            webhook_url: None,
            telegram_bot_token: None,
            telegram_chat_id: None,
            telegram_api_url: DEFAULT_TELEGRAM_API_URL.to_owned(),
            lang: Lang::Tr,
        }
    }
}

impl fmt::Debug for NotifyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A webhook URL often holds its own secret, as Discord's and Slack's do.
        let set = |value: &Option<String>| if value.is_some() { "<set>" } else { "<unset>" };
        f.debug_struct("NotifyConfig")
            .field("webhook_url", &set(&self.webhook_url))
            .field("telegram_bot_token", &set(&self.telegram_bot_token))
            .field("telegram_chat_id", &self.telegram_chat_id)
            .field("telegram_api_url", &self.telegram_api_url)
            .field("lang", &self.lang)
            .finish()
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("admin_username", &self.admin_username)
            .field("admin_password", &"<redacted>")
            .field("secret_key", &"<redacted>")
            .field("listen_addr", &self.listen_addr)
            .field("data_dir", &self.data_dir)
            .field("sync_interval_minutes", &self.sync_interval_minutes)
            .field("sync_concurrency", &self.sync_concurrency)
            .field("git_timeout_seconds", &self.git_timeout_seconds)
            .field("fsck_interval_days", &self.fsck_interval_days)
            .field("repack_interval_days", &self.repack_interval_days)
            .field("session_ttl_hours", &self.session_ttl_hours)
            .field("cookie_secure", &self.cookie_secure)
            .field("log_level", &self.log_level)
            .field("public_url", &self.public_url)
            .field("notify", &self.notify)
            .field("backup", &self.backup)
            .finish()
    }
}

impl Config {
    /// Temporary files: bundles before their upload, SSH keys during a fetch.
    pub fn tmp_dir(&self) -> PathBuf {
        self.data_dir.join("tmp")
    }

    /// Archived copies of the external images of Markdown files, named by
    /// the SHA-256 of their content.
    pub fn assets_dir(&self) -> PathBuf {
        self.data_dir.join("assets")
    }

    /// The SSH host keys stored on the first connection to each server.
    pub fn known_hosts(&self) -> PathBuf {
        self.data_dir.join("ssh").join("known_hosts")
    }

    /// Reads the configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads the configuration through `lookup`, which returns the value of
    /// one variable. Tests pass a map here instead of the real environment.
    pub fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let env = Env(lookup);
        let mut config = Self::with_credentials(&env)?;
        config.apply_server_settings(&env)?;
        config.apply_sync_settings(&env)?;
        config.apply_notify_settings(&env)?;
        config.apply_backup_settings(&env)?;
        Ok(config)
    }

    /// Reads the required values and fills every optional one with its default.
    fn with_credentials<F>(env: &Env<F>) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        Ok(Self {
            admin_username: env.required(ENV_ADMIN_USERNAME)?,
            admin_password: env.required(ENV_ADMIN_PASSWORD)?,
            secret_key: parse_secret_key(&env.required(ENV_SECRET_KEY)?)?,
            listen_addr: None,
            data_dir: PathBuf::from("/data"),
            sync_interval_minutes: 360,
            sync_concurrency: 2,
            git_timeout_seconds: 3600,
            fsck_interval_days: 7,
            repack_interval_days: 7,
            session_ttl_hours: 168,
            cookie_secure: false,
            log_level: String::from("info"),
            public_url: None,
            notify: NotifyConfig::default(),
            backup: BackupConfig::default(),
        })
    }

    fn apply_backup_settings<F>(&mut self, env: &Env<F>) -> Result<(), ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let backup = &mut self.backup;
        backup.endpoint = env.http_url(ENV_BACKUP_S3_ENDPOINT)?;
        backup.region = env.parsed(ENV_BACKUP_S3_REGION, backup.region.clone())?;
        backup.bucket = env.get(ENV_BACKUP_S3_BUCKET);
        backup.prefix = env.parsed(ENV_BACKUP_S3_PREFIX, String::new())?;
        backup.access_key_id = env.get(ENV_BACKUP_S3_ACCESS_KEY_ID);
        backup.secret_access_key = env.get(ENV_BACKUP_S3_SECRET_ACCESS_KEY);
        backup.path_style = env.boolean(ENV_BACKUP_S3_PATH_STYLE, backup.path_style)?;
        backup.interval_hours = env.ranged(
            ENV_BACKUP_INTERVAL_HOURS,
            backup.interval_hours,
            crate::backup::settings::INTERVAL_RANGE,
        )?;
        backup.keep = env.ranged(
            ENV_BACKUP_KEEP,
            backup.keep,
            crate::backup::settings::KEEP_RANGE,
        )?;
        Ok(())
    }

    fn apply_notify_settings<F>(&mut self, env: &Env<F>) -> Result<(), ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        self.public_url = env.http_url(ENV_PUBLIC_URL)?;
        let notify = &mut self.notify;
        notify.webhook_url = env.http_url(ENV_NOTIFY_WEBHOOK_URL)?;
        notify.telegram_bot_token = env.get(ENV_NOTIFY_TELEGRAM_BOT_TOKEN);
        notify.telegram_chat_id = env.get(ENV_NOTIFY_TELEGRAM_CHAT_ID);
        if let Some(url) = env.http_url(ENV_NOTIFY_TELEGRAM_API_URL)? {
            notify.telegram_api_url = url;
        }
        if let Some(code) = env.get(ENV_NOTIFY_LANG) {
            notify.lang = Lang::parse(&code).ok_or(ConfigError::Invalid {
                name: ENV_NOTIFY_LANG,
                reason: String::from("expected tr or en"),
            })?;
        }
        Ok(())
    }

    fn apply_server_settings<F>(&mut self, env: &Env<F>) -> Result<(), ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        self.listen_addr = env.optional(ENV_LISTEN_ADDR)?;
        self.data_dir = env.parsed(ENV_DATA_DIR, self.data_dir.clone())?;
        self.session_ttl_hours = env.positive(ENV_SESSION_TTL_HOURS, self.session_ttl_hours)?;
        self.cookie_secure = env.boolean(ENV_COOKIE_SECURE, self.cookie_secure)?;
        self.log_level = env.parsed(ENV_LOG_LEVEL, self.log_level.clone())?;
        Ok(())
    }

    fn apply_sync_settings<F>(&mut self, env: &Env<F>) -> Result<(), ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        use crate::settings::{
            CONCURRENCY_RANGE, GIT_TIMEOUT_RANGE, INTERVAL_RANGE, MAINTENANCE_RANGE,
        };
        self.sync_interval_minutes = env.ranged(
            ENV_SYNC_INTERVAL_MINUTES,
            self.sync_interval_minutes,
            INTERVAL_RANGE,
        )?;
        self.sync_concurrency = env.ranged(
            ENV_SYNC_CONCURRENCY,
            self.sync_concurrency,
            CONCURRENCY_RANGE,
        )?;
        self.git_timeout_seconds = env.ranged(
            ENV_GIT_TIMEOUT_SECONDS,
            self.git_timeout_seconds,
            GIT_TIMEOUT_RANGE,
        )?;
        self.fsck_interval_days = env.ranged(
            ENV_FSCK_INTERVAL_DAYS,
            self.fsck_interval_days,
            MAINTENANCE_RANGE,
        )?;
        self.repack_interval_days = env.ranged(
            ENV_REPACK_INTERVAL_DAYS,
            self.repack_interval_days,
            MAINTENANCE_RANGE,
        )?;
        Ok(())
    }
}

struct Env<F>(F);

impl<F> Env<F>
where
    F: Fn(&str) -> Option<String>,
{
    /// Returns the trimmed value, or `None` when the variable is unset or blank.
    fn get(&self, name: &str) -> Option<String> {
        (self.0)(name)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    fn required(&self, name: &'static str) -> Result<String, ConfigError> {
        self.get(name).ok_or(ConfigError::Missing(name))
    }

    /// Parses the value, or returns `None` when the variable is unset or blank.
    fn optional<T>(&self, name: &'static str) -> Result<Option<T>, ConfigError>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        self.get(name)
            .map(|raw| {
                raw.parse().map_err(|err: T::Err| ConfigError::Invalid {
                    name,
                    reason: err.to_string(),
                })
            })
            .transpose()
    }

    fn parsed<T>(&self, name: &'static str, default: T) -> Result<T, ConfigError>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        Ok(self.optional(name)?.unwrap_or(default))
    }

    fn positive<T>(&self, name: &'static str, default: T) -> Result<T, ConfigError>
    where
        T: FromStr + PartialOrd + From<u8>,
        T::Err: fmt::Display,
    {
        let value = self.parsed(name, default)?;
        if value < T::from(1) {
            return Err(ConfigError::Invalid {
                name,
                reason: String::from("must be 1 or greater"),
            });
        }
        Ok(value)
    }

    /// A whole number inside `range`, the range the settings page accepts
    /// for the same value.
    fn ranged<T>(
        &self,
        name: &'static str,
        default: T,
        range: RangeInclusive<i64>,
    ) -> Result<T, ConfigError>
    where
        T: TryFrom<i64>,
    {
        let Some(value) = self.optional::<i64>(name)? else {
            return Ok(default);
        };
        let invalid = || ConfigError::Invalid {
            name,
            reason: format!("must be between {} and {}", range.start(), range.end()),
        };
        if !range.contains(&value) {
            return Err(invalid());
        }
        T::try_from(value).map_err(|_| invalid())
    }

    /// An http(s) URL without a trailing slash, or `None` when unset.
    fn http_url(&self, name: &'static str) -> Result<Option<String>, ConfigError> {
        let Some(raw) = self.get(name) else {
            return Ok(None);
        };
        match crate::repo_url::parse_http_url(&raw) {
            Some(url) => Ok(Some(url)),
            None => Err(ConfigError::Invalid {
                name,
                reason: String::from("expected an http or https URL"),
            }),
        }
    }

    fn boolean(&self, name: &'static str, default: bool) -> Result<bool, ConfigError> {
        match self.get(name).map(|raw| raw.to_ascii_lowercase()) {
            None => Ok(default),
            Some(raw) if matches!(raw.as_str(), "1" | "true" | "yes" | "on") => Ok(true),
            Some(raw) if matches!(raw.as_str(), "0" | "false" | "no" | "off") => Ok(false),
            Some(_) => Err(ConfigError::Invalid {
                name,
                reason: String::from("expected true or false"),
            }),
        }
    }
}

/// Decodes the 64 hex characters of the token encryption key.
fn parse_secret_key(raw: &str) -> Result<[u8; SECRET_KEY_LEN], ConfigError> {
    let invalid = |reason: &str| ConfigError::Invalid {
        name: ENV_SECRET_KEY,
        reason: reason.to_owned(),
    };
    let bytes = raw.as_bytes();
    if bytes.len() != SECRET_KEY_LEN * 2 {
        return Err(invalid("expected 64 hex characters (openssl rand -hex 32)"));
    }
    let mut key = [0u8; SECRET_KEY_LEN];
    for (slot, pair) in key.iter_mut().zip(bytes.chunks_exact(2)) {
        let high = hex_value(pair[0]).ok_or_else(|| invalid("contains a non-hex character"))?;
        let low = hex_value(pair[1]).ok_or_else(|| invalid("contains a non-hex character"))?;
        *slot = (high << 4) | low;
    }
    Ok(key)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    fn load(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Config::from_lookup(|name| map.get(name).cloned())
    }

    fn required() -> Vec<(&'static str, &'static str)> {
        vec![
            (ENV_ADMIN_USERNAME, "admin"),
            (ENV_ADMIN_PASSWORD, "secret"),
            (ENV_SECRET_KEY, KEY),
        ]
    }

    #[test]
    fn applies_defaults_when_only_required_values_are_set() {
        let config = load(&required()).unwrap();
        assert_eq!(config.listen_addr, None);
        assert_eq!(config.data_dir, PathBuf::from("/data"));
        assert_eq!(config.sync_interval_minutes, 360);
        assert_eq!(config.sync_concurrency, 2);
        assert_eq!(config.git_timeout_seconds, 3600);
        assert!(!config.cookie_secure);
        assert_eq!(config.secret_key[31], 0x1f);
    }

    #[test]
    fn each_required_value_is_enforced() {
        for missing in [ENV_ADMIN_USERNAME, ENV_ADMIN_PASSWORD, ENV_SECRET_KEY] {
            let pairs: Vec<_> = required()
                .into_iter()
                .filter(|(k, _)| *k != missing)
                .collect();
            assert_eq!(load(&pairs).unwrap_err(), ConfigError::Missing(missing));
        }
    }

    #[test]
    fn blank_required_value_counts_as_missing() {
        let mut pairs = required();
        pairs[0].1 = "   ";
        assert_eq!(
            load(&pairs).unwrap_err(),
            ConfigError::Missing(ENV_ADMIN_USERNAME)
        );
    }

    #[test]
    fn rejects_a_short_or_non_hex_secret_key() {
        for bad in ["abcd", &KEY.replace('0', "g")] {
            let mut pairs = required();
            pairs[2].1 = bad;
            let err = load(&pairs).unwrap_err();
            assert!(matches!(
                err,
                ConfigError::Invalid {
                    name: ENV_SECRET_KEY,
                    ..
                }
            ));
        }
    }

    #[test]
    fn rejects_zero_interval_and_unparsable_numbers() {
        for (name, value) in [
            (ENV_SYNC_INTERVAL_MINUTES, "0"),
            (ENV_SYNC_CONCURRENCY, "two"),
        ] {
            let mut pairs = required();
            pairs.push((name, value));
            assert!(matches!(
                load(&pairs).unwrap_err(),
                ConfigError::Invalid { .. }
            ));
        }
    }

    #[test]
    fn rejects_numbers_the_settings_page_would_refuse() {
        for (name, value) in [
            (ENV_GIT_TIMEOUT_SECONDS, "5"),
            (ENV_SYNC_CONCURRENCY, "1000"),
            (ENV_SYNC_INTERVAL_MINUTES, "10081"),
            (ENV_FSCK_INTERVAL_DAYS, "366"),
            (ENV_REPACK_INTERVAL_DAYS, "-1"),
            (ENV_BACKUP_INTERVAL_HOURS, "169"),
            (ENV_BACKUP_KEEP, "366"),
        ] {
            let mut pairs = required();
            pairs.push((name, value));
            assert!(
                matches!(load(&pairs).unwrap_err(), ConfigError::Invalid { name: n, .. } if n == name),
                "{name}={value}"
            );
        }
        let mut pairs = required();
        pairs.push((ENV_GIT_TIMEOUT_SECONDS, "10"));
        pairs.push((ENV_SYNC_CONCURRENCY, "32"));
        let config = load(&pairs).unwrap();
        assert_eq!(
            (config.git_timeout_seconds, config.sync_concurrency),
            (10, 32)
        );
    }

    #[test]
    fn parses_overrides() {
        let mut pairs = required();
        pairs.push((ENV_LISTEN_ADDR, "127.0.0.1:9000"));
        pairs.push((ENV_COOKIE_SECURE, "TRUE"));
        pairs.push((ENV_SYNC_INTERVAL_MINUTES, "15"));
        pairs.push((ENV_FSCK_INTERVAL_DAYS, "0"));
        let config = load(&pairs).unwrap();
        assert_eq!(config.listen_addr.unwrap().to_string(), "127.0.0.1:9000");
        assert!(config.cookie_secure);
        assert_eq!(config.sync_interval_minutes, 15);
        // Zero turns the integrity check off.
        assert_eq!(config.fsck_interval_days, 0);
        assert_eq!(config.repack_interval_days, 7);
    }

    #[test]
    fn notification_settings_are_read_and_checked() {
        let config = load(&required()).unwrap();
        assert_eq!(config.notify.telegram_api_url, DEFAULT_TELEGRAM_API_URL);
        assert_eq!(config.notify.lang, Lang::Tr);
        assert_eq!(config.public_url, None);

        let mut pairs = required();
        pairs.push((ENV_PUBLIC_URL, "https://archive.example.com/"));
        pairs.push((ENV_NOTIFY_WEBHOOK_URL, "https://hooks.example.com/x"));
        pairs.push((ENV_NOTIFY_TELEGRAM_BOT_TOKEN, "123:abc"));
        pairs.push((ENV_NOTIFY_LANG, "en"));
        let config = load(&pairs).unwrap();
        assert_eq!(
            config.public_url.as_deref(),
            Some("https://archive.example.com")
        );
        assert_eq!(config.notify.telegram_bot_token.as_deref(), Some("123:abc"));
        assert_eq!(config.notify.lang, Lang::En);
        let shown = format!("{config:?}");
        assert!(!shown.contains("123:abc") && !shown.contains("hooks.example.com"));

        for (name, value) in [(ENV_NOTIFY_WEBHOOK_URL, "ftp://x"), (ENV_NOTIFY_LANG, "de")] {
            let mut pairs = required();
            pairs.push((name, value));
            assert!(matches!(
                load(&pairs).unwrap_err(),
                ConfigError::Invalid { .. }
            ));
        }
    }

    #[test]
    fn backup_settings_are_read_and_the_secret_is_hidden() {
        let config = load(&required()).unwrap();
        assert_eq!(config.backup.region, "us-east-1");
        assert_eq!((config.backup.interval_hours, config.backup.keep), (24, 7));

        let mut pairs = required();
        pairs.push((ENV_BACKUP_S3_ENDPOINT, "http://minio:9000/"));
        pairs.push((ENV_BACKUP_S3_BUCKET, "archive"));
        pairs.push((ENV_BACKUP_S3_SECRET_ACCESS_KEY, "s3cr3t-value"));
        pairs.push((ENV_BACKUP_S3_PATH_STYLE, "true"));
        pairs.push((ENV_BACKUP_KEEP, "3"));
        let config = load(&pairs).unwrap();
        assert_eq!(config.backup.endpoint.as_deref(), Some("http://minio:9000"));
        assert!(config.backup.path_style);
        assert_eq!(config.backup.keep, 3);
        assert!(!format!("{config:?}").contains("s3cr3t-value"));

        let mut pairs = required();
        pairs.push((ENV_BACKUP_KEEP, "0"));
        assert!(load(&pairs).is_err());
    }

    #[test]
    fn debug_output_hides_secrets() {
        let rendered = format!("{:?}", load(&required()).unwrap());
        assert!(!rendered.contains("secret\""));
        assert!(!rendered.contains("0x1f") && !rendered.contains("31"));
        assert!(rendered.contains("<redacted>"));
    }
}
