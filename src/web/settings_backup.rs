//! The backup part of the settings page: storage, schedule, a connection
//! test and a "back up now" button.

use axum::Form;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::form::{InvalidNumber, is_checked, optional_number, optional_text};
use super::repos::PageContext;
use super::settings_page::{self, PageState};
use crate::backup::settings::{INTERVAL_RANGE, KEEP_RANGE, LastRun, SaveInput};
use crate::backup::{self, settings as backup_settings};
use crate::db::settings::SecretChange;
use crate::error::AppError;
use crate::repo_url;

/// The backup form as the page shows it.
pub struct BackupView {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    pub secret_stored: bool,
    /// `""` (environment), `"true"` or `"false"`.
    pub path_style: String,
    pub interval_hours: String,
    pub keep: String,
    pub env_interval_hours: u32,
    pub env_keep: u32,
    /// Whether every value the storage needs is known.
    pub configured: bool,
    pub last_run: Option<LastRun>,
    pub running: bool,
}

impl BackupView {
    pub fn is_path_style(&self, value: &str) -> bool {
        self.path_style == value
    }
}

pub async fn load(ctx: &PageContext) -> Result<BackupView, AppError> {
    let state = &ctx.state;
    let saved = backup_settings::saved(&state.pool).await?;
    let configured = backup_settings::effective(&state.pool, &state.config, &state.cipher)
        .await
        .is_ok_and(|current| current.target.is_some());
    let text = |value: Option<String>| value.unwrap_or_default();
    Ok(BackupView {
        endpoint: text(saved.endpoint),
        region: text(saved.region),
        bucket: text(saved.bucket),
        prefix: text(saved.prefix),
        access_key_id: text(saved.access_key_id),
        secret_stored: saved.secret_stored,
        path_style: saved
            .path_style
            .map(|on| on.to_string())
            .unwrap_or_default(),
        interval_hours: saved
            .interval_hours
            .map(|n| n.to_string())
            .unwrap_or_default(),
        keep: saved.keep.map(|n| n.to_string()).unwrap_or_default(),
        env_interval_hours: state.config.backup.interval_hours,
        env_keep: state.config.backup.keep,
        configured,
        last_run: backup_settings::last_run(&state.pool).await?,
        running: state.scheduler.is_backing_up(),
    })
}

#[derive(Debug, Deserialize)]
pub struct BackupForm {
    s3_endpoint: Option<String>,
    s3_region: Option<String>,
    s3_bucket: Option<String>,
    s3_prefix: Option<String>,
    s3_access_key_id: Option<String>,
    s3_secret: Option<String>,
    clear_s3_secret: Option<String>,
    s3_path_style: Option<String>,
    interval_hours: Option<String>,
    keep: Option<String>,
}

/// A text field that must not contain spaces; the `Err` is the message key.
fn word<'a>(text: Option<&'a str>, key: &'static str) -> Result<Option<&'a str>, &'static str> {
    match optional_text(text) {
        Some(value) if value.chars().any(char::is_whitespace) => Err(key),
        value => Ok(value),
    }
}

fn number(
    text: Option<&str>,
    range: std::ops::RangeInclusive<i64>,
) -> Result<Option<i64>, &'static str> {
    optional_number(text, range).map_err(|InvalidNumber| "settings.backup_invalid_number")
}

fn endpoint(text: Option<&str>) -> Result<Option<String>, &'static str> {
    match optional_text(text) {
        None => Ok(None),
        Some(value) => repo_url::parse_http_url(value)
            .map(Some)
            .ok_or("settings.backup_invalid_endpoint"),
    }
}

/// The checked values; the `Err` carries the message key to show.
fn validate(form: &BackupForm) -> Result<(Option<String>, SaveInput<'_>), &'static str> {
    let endpoint = endpoint(form.s3_endpoint.as_deref())?;
    let input = SaveInput {
        endpoint: None,
        region: word(form.s3_region.as_deref(), "settings.backup_invalid_text")?,
        bucket: word(form.s3_bucket.as_deref(), "settings.backup_invalid_text")?,
        prefix: word(form.s3_prefix.as_deref(), "settings.backup_invalid_text")?,
        access_key_id: word(
            form.s3_access_key_id.as_deref(),
            "settings.backup_invalid_text",
        )?,
        path_style: match form.s3_path_style.as_deref() {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        },
        interval_hours: number(form.interval_hours.as_deref(), INTERVAL_RANGE)?,
        keep: number(form.keep.as_deref(), KEEP_RANGE)?,
    };
    Ok((endpoint, input))
}

fn secret_change(ctx: &PageContext, form: &BackupForm) -> Result<SecretChange, AppError> {
    Ok(match optional_text(form.s3_secret.as_deref()) {
        Some(secret) => SecretChange::Set(
            ctx.state
                .cipher
                .encrypt(secret)
                .map_err(AppError::internal)?,
        ),
        None if is_checked(form.clear_s3_secret.as_deref()) => SecretChange::Clear,
        None => SecretChange::Keep,
    })
}

pub async fn save(ctx: PageContext, Form(form): Form<BackupForm>) -> Result<Response, AppError> {
    let (endpoint, mut input) = match validate(&form) {
        Ok(valid) => valid,
        Err(key) => return show_form_error(&ctx, form, key).await,
    };
    input.endpoint = endpoint.as_deref();
    let secret = secret_change(&ctx, &form)?;
    backup_settings::save(&ctx.state.pool, &input, &secret).await?;
    tracing::info!("backup settings saved");
    // A newly configured backup may be due now.
    ctx.state.scheduler.wake();
    Ok(Redirect::to("/settings?notice=backup_saved#backup").into_response())
}

async fn show_form_error(
    ctx: &PageContext,
    form: BackupForm,
    key: &str,
) -> Result<Response, AppError> {
    let layout = settings_page::layout(ctx);
    let mut view = load(ctx).await?;
    view.endpoint = form.s3_endpoint.unwrap_or_default();
    view.region = form.s3_region.unwrap_or_default();
    view.bucket = form.s3_bucket.unwrap_or_default();
    view.prefix = form.s3_prefix.unwrap_or_default();
    view.access_key_id = form.s3_access_key_id.unwrap_or_default();
    view.path_style = form.s3_path_style.unwrap_or_default();
    view.interval_hours = form.interval_hours.unwrap_or_default();
    view.keep = form.keep.unwrap_or_default();
    let state = PageState {
        errors: vec![layout.t(key).to_owned()],
        backup: Some(view),
        status: StatusCode::UNPROCESSABLE_ENTITY,
        ..PageState::default()
    };
    settings_page::respond(ctx, layout, state).await
}

pub async fn test(ctx: PageContext) -> Result<Response, AppError> {
    let layout = settings_page::layout(&ctx);
    let state = &ctx.state;
    let result = match backup_settings::effective(&state.pool, &state.config, &state.cipher).await {
        Ok(current) => match current.target {
            Some(target) => backup::test_target(&target).await,
            None => Err(layout.t("settings.backup_not_configured").to_owned()),
        },
        Err(err) => Err(err),
    };
    let page = match result {
        Ok(()) => PageState {
            notice: Some(layout.t("settings.backup_test_ok").to_owned()),
            ..PageState::default()
        },
        Err(err) => PageState {
            errors: vec![layout.tf("settings.backup_test_failed", &[("error", &err)])],
            status: StatusCode::UNPROCESSABLE_ENTITY,
            ..PageState::default()
        },
    };
    settings_page::respond(&ctx, layout, page).await
}

/// Starts a backup now; the page shows that it runs.
pub async fn run(ctx: PageContext) -> Result<Response, AppError> {
    let state = &ctx.state;
    let configured = backup_settings::effective(&state.pool, &state.config, &state.cipher)
        .await
        .is_ok_and(|current| current.target.is_some());
    if !configured {
        let layout = settings_page::layout(&ctx);
        let page = PageState {
            errors: vec![layout.t("settings.backup_not_configured").to_owned()],
            status: StatusCode::UNPROCESSABLE_ENTITY,
            ..PageState::default()
        };
        return settings_page::respond(&ctx, layout, page).await;
    }
    let notice = match state.scheduler.start_backup() {
        Some(_) => "backup_started",
        None => "backup_running",
    };
    Ok(Redirect::to(&format!("/settings?notice={notice}#backup")).into_response())
}
