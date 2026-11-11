//! Sends one message to a webhook or to a Telegram chat.

use std::time::Duration;

use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};

use super::settings::Telegram;

/// Longest wait for one channel, so a dead endpoint cannot hold a sync.
const TIMEOUT: Duration = Duration::from_secs(10);

pub fn client() -> Result<Client, String> {
    Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("git-archive/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| format!("cannot create the HTTP client: {err}"))
}

/// The JSON a webhook receives. `text` and `content` carry the same
/// sentence, so Slack- and Discord-style receivers show it as is.
#[derive(Debug, Serialize)]
pub struct WebhookBody<'a> {
    pub event: &'static str,
    pub text: &'a str,
    pub content: &'a str,
    pub repo: Option<RepoInfo<'a>>,
    pub source: Option<&'a str>,
    pub status: Option<&'static str>,
    pub previous_status: Option<&'static str>,
    pub message: Option<&'a str>,
    pub at: i64,
    pub link: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RepoInfo<'a> {
    pub id: i64,
    pub name: String,
    pub url: &'a str,
}

pub async fn webhook(client: &Client, url: &str, body: &WebhookBody<'_>) -> Result<(), String> {
    // The URL often holds the receiver's secret; errors never show it.
    let response = client
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|err| err.without_url().to_string())?;
    check(response).await
}

#[derive(Serialize)]
struct TelegramMessage<'a> {
    chat_id: &'a str,
    text: &'a str,
    disable_web_page_preview: bool,
}

pub async fn telegram(client: &Client, telegram: &Telegram, text: &str) -> Result<(), String> {
    let url = format!("{}/bot{}/sendMessage", telegram.api_url, telegram.bot_token);
    let message = TelegramMessage {
        chat_id: &telegram.chat_id,
        text,
        disable_web_page_preview: true,
    };
    // The URL holds the bot token; errors never show it.
    let response = client
        .post(url)
        .json(&message)
        .send()
        .await
        .map_err(|err| err.without_url().to_string())?;
    check(response).await
}

/// The error field of a Telegram answer.
#[derive(Deserialize)]
struct ApiError {
    description: Option<String>,
}

async fn check(response: Response) -> Result<(), String> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let detail = match response.text().await {
        Ok(body) => serde_json::from_str::<ApiError>(&body)
            .ok()
            .and_then(|err| err.description)
            .unwrap_or_else(|| body.chars().take(200).collect()),
        Err(err) => format!("cannot read the answer: {}", err.without_url()),
    };
    Err(format!("HTTP {}: {}", status.as_u16(), detail.trim()))
}
