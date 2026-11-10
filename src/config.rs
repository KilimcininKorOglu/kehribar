//! Runtime configuration. Every setting comes from an environment variable,
//! so Docker Compose, Coolify and Dokploy can set each value directly.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use thiserror::Error;

pub const ENV_ADMIN_USERNAME: &str = "GIT_ARCHIVE_ADMIN_USERNAME";
pub const ENV_ADMIN_PASSWORD: &str = "GIT_ARCHIVE_ADMIN_PASSWORD";
pub const ENV_SECRET_KEY: &str = "GIT_ARCHIVE_SECRET_KEY";
pub const ENV_LISTEN_ADDR: &str = "GIT_ARCHIVE_LISTEN_ADDR";
pub const ENV_DATA_DIR: &str = "GIT_ARCHIVE_DATA_DIR";
pub const ENV_SYNC_INTERVAL_MINUTES: &str = "GIT_ARCHIVE_SYNC_INTERVAL_MINUTES";
pub const ENV_SYNC_CONCURRENCY: &str = "GIT_ARCHIVE_SYNC_CONCURRENCY";
pub const ENV_GIT_TIMEOUT_SECONDS: &str = "GIT_ARCHIVE_GIT_TIMEOUT_SECONDS";
pub const ENV_SESSION_TTL_HOURS: &str = "GIT_ARCHIVE_SESSION_TTL_HOURS";
pub const ENV_COOKIE_SECURE: &str = "GIT_ARCHIVE_COOKIE_SECURE";
pub const ENV_LOG_LEVEL: &str = "GIT_ARCHIVE_LOG_LEVEL";

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
    pub session_ttl_hours: u32,
    pub cookie_secure: bool,
    pub log_level: String,
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
            .field("session_ttl_hours", &self.session_ttl_hours)
            .field("cookie_secure", &self.cookie_secure)
            .field("log_level", &self.log_level)
            .finish()
    }
}

impl Config {
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
            session_ttl_hours: 168,
            cookie_secure: false,
            log_level: String::from("info"),
        })
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
        self.sync_interval_minutes =
            env.positive(ENV_SYNC_INTERVAL_MINUTES, self.sync_interval_minutes)?;
        self.sync_concurrency = env.positive(ENV_SYNC_CONCURRENCY, self.sync_concurrency)?;
        self.git_timeout_seconds =
            env.positive(ENV_GIT_TIMEOUT_SECONDS, self.git_timeout_seconds)?;
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
    fn parses_overrides() {
        let mut pairs = required();
        pairs.push((ENV_LISTEN_ADDR, "127.0.0.1:9000"));
        pairs.push((ENV_COOKIE_SECURE, "TRUE"));
        pairs.push((ENV_SYNC_INTERVAL_MINUTES, "15"));
        let config = load(&pairs).unwrap();
        assert_eq!(config.listen_addr.unwrap().to_string(), "127.0.0.1:9000");
        assert!(config.cookie_secure);
        assert_eq!(config.sync_interval_minutes, 15);
    }

    #[test]
    fn debug_output_hides_secrets() {
        let rendered = format!("{:?}", load(&required()).unwrap());
        assert!(!rendered.contains("secret\""));
        assert!(!rendered.contains("0x1f") && !rendered.contains("31"));
        assert!(rendered.contains("<redacted>"));
    }
}
