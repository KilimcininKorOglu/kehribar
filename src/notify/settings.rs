//! Notification channels: values saved on the settings page win over the
//! environment. The Telegram bot token is stored encrypted.

use base64::prelude::{BASE64_STANDARD, Engine as _};
use sqlx::SqlitePool;

use crate::config::Config;
use crate::crypto::Cipher;
use crate::db::settings as store;
use crate::i18n::Lang;

const KEY_WEBHOOK_URL: &str = "notify_webhook_url";
const KEY_TELEGRAM_TOKEN: &str = "notify_telegram_bot_token";
const KEY_TELEGRAM_CHAT_ID: &str = "notify_telegram_chat_id";
const KEY_LANG: &str = "notify_lang";
const KEY_LAST_ERROR: &str = "notify_last_error";
const KEY_LAST_ERROR_AT: &str = "notify_last_error_at";

/// The values saved on the settings page; `None` means the environment applies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Saved {
    pub webhook_url: Option<String>,
    pub telegram_token_stored: bool,
    pub telegram_chat_id: Option<String>,
    pub lang: Option<Lang>,
}

/// What a save does with the stored bot token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenChange {
    Keep,
    /// The encrypted new token.
    Set(Vec<u8>),
    Clear,
}

/// The channels in force.
#[derive(Debug, Clone)]
pub struct Channels {
    pub webhook_url: Option<String>,
    pub telegram: Option<Telegram>,
    pub lang: Lang,
}

impl Channels {
    pub fn is_empty(&self) -> bool {
        self.webhook_url.is_none() && self.telegram.is_none()
    }
}

#[derive(Clone)]
pub struct Telegram {
    pub api_url: String,
    pub bot_token: String,
    pub chat_id: String,
}

impl std::fmt::Debug for Telegram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telegram")
            .field("api_url", &self.api_url)
            .field("bot_token", &"<redacted>")
            .field("chat_id", &self.chat_id)
            .finish()
    }
}

/// The last delivery error, shown on the settings page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastError {
    pub at: i64,
    pub message: String,
}

pub async fn saved(pool: &SqlitePool) -> sqlx::Result<Saved> {
    Ok(Saved {
        webhook_url: store::get(pool, KEY_WEBHOOK_URL).await?,
        telegram_token_stored: store::get(pool, KEY_TELEGRAM_TOKEN).await?.is_some(),
        telegram_chat_id: store::get(pool, KEY_TELEGRAM_CHAT_ID).await?,
        lang: store::get(pool, KEY_LANG)
            .await?
            .and_then(|code| Lang::parse(&code)),
    })
}

/// Saves the page values; a `None` field removes the saved value.
pub async fn save(
    pool: &SqlitePool,
    webhook_url: Option<&str>,
    telegram_chat_id: Option<&str>,
    lang: Option<Lang>,
    token: &TokenChange,
) -> sqlx::Result<()> {
    write(pool, KEY_WEBHOOK_URL, webhook_url).await?;
    write(pool, KEY_TELEGRAM_CHAT_ID, telegram_chat_id).await?;
    write(pool, KEY_LANG, lang.map(|l| l.code())).await?;
    match token {
        TokenChange::Keep => Ok(()),
        TokenChange::Set(enc) => {
            store::set(pool, KEY_TELEGRAM_TOKEN, &BASE64_STANDARD.encode(enc)).await
        }
        TokenChange::Clear => store::remove(pool, KEY_TELEGRAM_TOKEN).await,
    }
}

/// The channels in force: saved values first, then the environment.
/// Telegram is on only when both the token and the chat are known.
pub async fn effective(
    pool: &SqlitePool,
    config: &Config,
    cipher: &Cipher,
) -> Result<Channels, String> {
    let env = &config.notify;
    let saved = saved(pool).await.map_err(|err| err.to_string())?;
    let token = match stored_token(pool, cipher).await? {
        Some(token) => Some(token),
        None => env.telegram_bot_token.clone(),
    };
    let chat_id = saved
        .telegram_chat_id
        .or_else(|| env.telegram_chat_id.clone());
    let telegram = token.zip(chat_id).map(|(bot_token, chat_id)| Telegram {
        api_url: env.telegram_api_url.clone(),
        bot_token,
        chat_id,
    });
    Ok(Channels {
        webhook_url: saved.webhook_url.or_else(|| env.webhook_url.clone()),
        telegram,
        lang: saved.lang.unwrap_or(env.lang),
    })
}

async fn stored_token(pool: &SqlitePool, cipher: &Cipher) -> Result<Option<String>, String> {
    let Some(text) = store::get(pool, KEY_TELEGRAM_TOKEN)
        .await
        .map_err(|err| err.to_string())?
    else {
        return Ok(None);
    };
    let enc = BASE64_STANDARD
        .decode(text)
        .map_err(|err| format!("stored Telegram token is damaged: {err}"))?;
    cipher
        .decrypt(&enc)
        .map(Some)
        .map_err(|err| format!("cannot decrypt the stored Telegram token: {err}"))
}

/// Remembers a failed delivery; `None` clears the last error after a
/// delivery that worked.
pub async fn record_delivery(pool: &SqlitePool, at: i64, error: Option<&str>) -> sqlx::Result<()> {
    match error {
        Some(message) => {
            store::set(pool, KEY_LAST_ERROR_AT, &at.to_string()).await?;
            store::set(pool, KEY_LAST_ERROR, message).await
        }
        None => {
            store::remove(pool, KEY_LAST_ERROR_AT).await?;
            store::remove(pool, KEY_LAST_ERROR).await
        }
    }
}

pub async fn last_error(pool: &SqlitePool) -> sqlx::Result<Option<LastError>> {
    let message = store::get(pool, KEY_LAST_ERROR).await?;
    let at = store::get(pool, KEY_LAST_ERROR_AT).await?;
    Ok(message.map(|message| LastError {
        at: at.and_then(|t| t.parse().ok()).unwrap_or_default(),
        message,
    }))
}

async fn write(pool: &SqlitePool, key: &str, value: Option<&str>) -> sqlx::Result<()> {
    match value {
        Some(value) => store::set(pool, key, value).await,
        None => store::remove(pool, key).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;

    fn config() -> Config {
        Config::from_lookup(|name| match name {
            "GIT_ARCHIVE_ADMIN_USERNAME" | "GIT_ARCHIVE_ADMIN_PASSWORD" => Some("x".into()),
            "GIT_ARCHIVE_SECRET_KEY" => Some("00".repeat(32)),
            "GIT_ARCHIVE_NOTIFY_WEBHOOK_URL" => Some("https://env.example/hook".into()),
            "GIT_ARCHIVE_NOTIFY_TELEGRAM_CHAT_ID" => Some("42".into()),
            _ => None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn saved_values_win_and_telegram_needs_token_and_chat() {
        let pool = open_in_memory().await.unwrap();
        let config = config();
        let cipher = Cipher::new(&config.secret_key);

        let channels = effective(&pool, &config, &cipher).await.unwrap();
        assert_eq!(
            channels.webhook_url.as_deref(),
            Some("https://env.example/hook")
        );
        // The environment gives the chat, but no token.
        assert!(channels.telegram.is_none());
        assert_eq!(channels.lang, Lang::Tr);

        let token = TokenChange::Set(cipher.encrypt("123:secret").unwrap());
        save(
            &pool,
            Some("https://page.example/hook"),
            None,
            Some(Lang::En),
            &token,
        )
        .await
        .unwrap();
        let channels = effective(&pool, &config, &cipher).await.unwrap();
        assert_eq!(
            channels.webhook_url.as_deref(),
            Some("https://page.example/hook")
        );
        let telegram = channels.telegram.unwrap();
        assert_eq!(
            (telegram.bot_token.as_str(), telegram.chat_id.as_str()),
            ("123:secret", "42")
        );
        assert_eq!(channels.lang, Lang::En);
        assert!(!format!("{telegram:?}").contains("secret"));
        assert!(saved(&pool).await.unwrap().telegram_token_stored);

        // Keep leaves the token; clear removes it and the fields fall back.
        save(&pool, None, None, None, &TokenChange::Keep)
            .await
            .unwrap();
        assert!(saved(&pool).await.unwrap().telegram_token_stored);
        save(&pool, None, None, None, &TokenChange::Clear)
            .await
            .unwrap();
        assert_eq!(saved(&pool).await.unwrap(), Saved::default());
    }

    #[tokio::test]
    async fn last_error_is_kept_until_a_delivery_works() {
        let pool = open_in_memory().await.unwrap();
        record_delivery(&pool, 5, Some("webhook: 500"))
            .await
            .unwrap();
        assert_eq!(
            last_error(&pool).await.unwrap(),
            Some(LastError {
                at: 5,
                message: "webhook: 500".into()
            })
        );
        record_delivery(&pool, 6, None).await.unwrap();
        assert_eq!(last_error(&pool).await.unwrap(), None);
    }
}
