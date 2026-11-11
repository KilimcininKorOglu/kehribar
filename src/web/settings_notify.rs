//! The notification part of the settings page: channels, language and a
//! test message.

use axum::Form;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::form::{is_checked, optional_text};
use super::pages::Layout;
use super::repos::PageContext;
use super::settings_page::{self, PageState};
use crate::db::unix_now;
use crate::error::AppError;
use crate::i18n::Lang;
use crate::notify::settings::{self as notify_settings, TokenChange};
use crate::notify::{self, Event};
use crate::repo_url;

/// The notification form as the page shows it.
pub struct NotifyView {
    pub webhook_url: String,
    pub token_stored: bool,
    pub chat_id: String,
    /// The saved language code; empty when the environment applies.
    pub lang: String,
    pub env_lang: Lang,
    /// The channels in force, or why they cannot be read.
    pub channels: String,
    pub last_error: Option<(i64, String)>,
}

impl NotifyView {
    pub fn langs(&self) -> [Lang; 2] {
        Lang::ALL
    }

    pub fn is_lang(&self, lang: &Lang) -> bool {
        self.lang == lang.code()
    }
}

/// The saved values, the channels in force and the last delivery error.
pub async fn load(ctx: &PageContext, layout: &Layout) -> Result<NotifyView, AppError> {
    let pool = &ctx.state.pool;
    let saved = notify_settings::saved(pool).await?;
    let channels = channels_text(ctx, layout).await;
    let last_error = notify_settings::last_error(pool)
        .await?
        .map(|err| (err.at, err.message));
    Ok(NotifyView {
        webhook_url: saved.webhook_url.unwrap_or_default(),
        token_stored: saved.telegram_token_stored,
        chat_id: saved.telegram_chat_id.unwrap_or_default(),
        lang: saved.lang.map(|l| l.code().to_owned()).unwrap_or_default(),
        env_lang: ctx.state.config.notify.lang,
        channels,
        last_error,
    })
}

async fn channels_text(ctx: &PageContext, layout: &Layout) -> String {
    let state = &ctx.state;
    match notify_settings::effective(&state.pool, &state.config, &state.cipher).await {
        Ok(channels) if channels.is_empty() => layout.t("settings.channels_none").to_owned(),
        Ok(channels) => {
            let names: Vec<&str> = [
                channels.webhook_url.as_ref().map(|_| "Webhook"),
                channels.telegram.as_ref().map(|_| "Telegram"),
            ]
            .into_iter()
            .flatten()
            .collect();
            format!("{} {}", layout.t("settings.channels"), names.join(", "))
        }
        Err(err) => err,
    }
}

#[derive(Debug, Deserialize)]
pub struct NotifyForm {
    webhook_url: Option<String>,
    telegram_token: Option<String>,
    clear_telegram_token: Option<String>,
    telegram_chat_id: Option<String>,
    lang: Option<String>,
}

/// A form that passed validation.
struct ValidForm {
    webhook_url: Option<String>,
    chat_id: Option<String>,
    lang: Option<Lang>,
    token: TokenChange,
}

fn has_space(text: &str) -> bool {
    text.chars().any(char::is_whitespace)
}

fn valid_webhook(text: Option<&str>) -> Result<Option<String>, &'static str> {
    let Some(text) = optional_text(text) else {
        return Ok(None);
    };
    match repo_url::parse_http_url(text) {
        Some(url) => Ok(Some(url)),
        None => Err("settings.invalid_webhook"),
    }
}

/// Checks the form. The `Err` carries the message key to show.
fn validate(
    ctx: &PageContext,
    form: &NotifyForm,
) -> Result<Result<ValidForm, &'static str>, AppError> {
    let webhook_url = match valid_webhook(form.webhook_url.as_deref()) {
        Ok(url) => url,
        Err(key) => return Ok(Err(key)),
    };
    let chat_id = optional_text(form.telegram_chat_id.as_deref());
    if chat_id.is_some_and(has_space) {
        return Ok(Err("settings.invalid_chat"));
    }
    let token = match optional_text(form.telegram_token.as_deref()) {
        Some(token) if has_space(token) || !token.contains(':') => {
            return Ok(Err("settings.invalid_telegram_token"));
        }
        Some(token) => TokenChange::Set(
            ctx.state
                .cipher
                .encrypt(token)
                .map_err(AppError::internal)?,
        ),
        None if is_checked(form.clear_telegram_token.as_deref()) => TokenChange::Clear,
        None => TokenChange::Keep,
    };
    Ok(Ok(ValidForm {
        webhook_url,
        chat_id: chat_id.map(str::to_owned),
        lang: form.lang.as_deref().and_then(Lang::parse),
        token,
    }))
}

pub async fn save(ctx: PageContext, Form(form): Form<NotifyForm>) -> Result<Response, AppError> {
    let valid = match validate(&ctx, &form)? {
        Ok(valid) => valid,
        Err(key) => {
            let layout = settings_page::layout(&ctx);
            let mut view = load(&ctx, &layout).await?;
            view.webhook_url = form.webhook_url.unwrap_or_default();
            view.chat_id = form.telegram_chat_id.unwrap_or_default();
            view.lang = form.lang.unwrap_or_default();
            let state = PageState {
                errors: vec![layout.t(key).to_owned()],
                notify: Some(view),
                status: StatusCode::UNPROCESSABLE_ENTITY,
                ..PageState::default()
            };
            return settings_page::respond(&ctx, layout, state).await;
        }
    };
    notify_settings::save(
        &ctx.state.pool,
        valid.webhook_url.as_deref(),
        valid.chat_id.as_deref(),
        valid.lang,
        &valid.token,
    )
    .await?;
    tracing::info!("notification settings saved");
    Ok(Redirect::to("/settings?notice=notify_saved").into_response())
}

pub async fn test(ctx: PageContext) -> Result<Response, AppError> {
    let layout = settings_page::layout(&ctx);
    let sync = ctx.state.sync_context();
    let channels = notify_settings::effective(&sync.pool, &sync.config, &sync.cipher).await;
    let mut state = PageState::default();
    match channels {
        Ok(channels) if channels.is_empty() => {
            state
                .errors
                .push(layout.t("settings.notify_no_channel").to_owned());
        }
        Err(err) => state
            .errors
            .push(layout.tf("settings.notify_failed", &[("error", &err)])),
        Ok(_) => {
            let result = notify::deliver(&sync, &Event::Test).await;
            notify_settings::record_delivery(
                &sync.pool,
                unix_now(),
                result.as_ref().err().map(String::as_str),
            )
            .await?;
            match result {
                Ok(()) => state.notice = Some(layout.t("settings.notify_sent").to_owned()),
                Err(err) => state
                    .errors
                    .push(layout.tf("settings.notify_failed", &[("error", &err)])),
            }
        }
    }
    if !state.errors.is_empty() {
        state.status = StatusCode::UNPROCESSABLE_ENTITY;
    }
    settings_page::respond(&ctx, layout, state).await
}
