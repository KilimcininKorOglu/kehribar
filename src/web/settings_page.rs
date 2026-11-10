//! Global sync settings page.

use askama::Template;
use axum::Form;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::pages::{Layout, Nav, render};
use super::repos::{InvalidNumber, NoticeQuery, PageContext, optional_number};
use crate::error::AppError;
use crate::settings::{
    self, CONCURRENCY_RANGE, GIT_TIMEOUT_RANGE, INTERVAL_RANGE, Overrides, SyncSettings,
};

/// One numeric setting as the form shows it.
pub struct Field {
    pub name: &'static str,
    pub label_key: &'static str,
    pub value: String,
    pub default_text: String,
    pub default: i64,
    pub min: i64,
    pub max: i64,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    layout: Layout,
    fields: Vec<Field>,
    notice: Option<String>,
    errors: Vec<String>,
}

fn fields(layout: &Layout, values: [Option<String>; 3], defaults: SyncSettings) -> Vec<Field> {
    let specs = [
        (
            "interval",
            "settings.interval",
            defaults.interval_minutes,
            INTERVAL_RANGE,
        ),
        (
            "concurrency",
            "settings.concurrency",
            defaults.concurrency,
            CONCURRENCY_RANGE,
        ),
        (
            "git_timeout",
            "settings.git_timeout",
            defaults.git_timeout_seconds,
            GIT_TIMEOUT_RANGE,
        ),
    ];
    specs
        .into_iter()
        .zip(values)
        .map(|((name, label_key, default, range), value)| Field {
            name,
            label_key,
            value: value.unwrap_or_default(),
            default_text: layout.tf_number("settings.env_default", &default),
            default,
            min: *range.start(),
            max: *range.end(),
        })
        .collect()
}

pub async fn show(
    ctx: PageContext,
    Query(query): Query<NoticeQuery>,
) -> Result<Response, AppError> {
    let saved = settings::overrides(&ctx.state.pool).await?;
    let layout = Layout::private(
        &ctx.state,
        ctx.prefs,
        &ctx.session,
        Nav::Settings,
        &ctx.path,
    );
    let values = [
        saved.interval_minutes,
        saved.concurrency,
        saved.git_timeout_seconds,
    ]
    .map(|v| v.map(|n| n.to_string()));
    let notice =
        (query.notice.as_deref() == Some("saved")).then(|| layout.t("settings.saved").to_owned());
    let page = SettingsPage {
        fields: fields(
            &layout,
            values,
            SyncSettings::from_config(&ctx.state.config),
        ),
        layout,
        notice,
        errors: Vec::new(),
    };
    Ok(render(&page)?.into_response())
}

#[derive(Debug, Deserialize)]
pub struct SettingsForm {
    interval: Option<String>,
    concurrency: Option<String>,
    git_timeout: Option<String>,
}

pub async fn save(ctx: PageContext, Form(form): Form<SettingsForm>) -> Result<Response, AppError> {
    let layout = Layout::private(
        &ctx.state,
        ctx.prefs,
        &ctx.session,
        Nav::Settings,
        &ctx.path,
    );
    let inputs = [
        (
            form.interval.as_deref(),
            INTERVAL_RANGE,
            "settings.interval",
        ),
        (
            form.concurrency.as_deref(),
            CONCURRENCY_RANGE,
            "settings.concurrency",
        ),
        (
            form.git_timeout.as_deref(),
            GIT_TIMEOUT_RANGE,
            "settings.git_timeout",
        ),
    ];
    let mut parsed = [None; 3];
    let mut errors = Vec::new();
    for (slot, (text, range, label_key)) in parsed.iter_mut().zip(inputs) {
        match optional_number(text, range.clone()) {
            Ok(value) => *slot = value,
            Err(InvalidNumber) => errors.push(layout.tf(
                "settings.out_of_range",
                &[
                    ("field", layout.t(label_key)),
                    ("min", &range.start().to_string()),
                    ("max", &range.end().to_string()),
                ],
            )),
        }
    }
    if !errors.is_empty() {
        let values = [form.interval, form.concurrency, form.git_timeout];
        let page = SettingsPage {
            fields: fields(
                &layout,
                values,
                SyncSettings::from_config(&ctx.state.config),
            ),
            layout,
            notice: None,
            errors,
        };
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, render(&page)?).into_response());
    }
    let [interval_minutes, concurrency, git_timeout_seconds] = parsed;
    let overrides = Overrides {
        interval_minutes,
        concurrency,
        git_timeout_seconds,
    };
    settings::save(&ctx.state.pool, &overrides).await?;
    tracing::info!(?overrides, "sync settings saved");
    // A higher concurrency may let waiting repositories start now.
    ctx.state.scheduler.wake();
    Ok(Redirect::to("/settings?notice=saved").into_response())
}
