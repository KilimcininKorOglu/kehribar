//! Global sync and maintenance settings page.

use std::ops::RangeInclusive;

use askama::Template;
use axum::Form;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::form::{InvalidNumber, optional_number};
use super::pages::{Layout, Nav, render};
use super::repos::{NoticeQuery, PageContext};
use super::settings_notify::{self, NotifyView};
use crate::error::AppError;
use crate::settings::{
    self, CONCURRENCY_RANGE, GIT_TIMEOUT_RANGE, INTERVAL_RANGE, MAINTENANCE_RANGE, Overrides,
    SyncSettings,
};

/// Number of numeric fields on the form.
pub const FIELD_COUNT: usize = 5;

/// One numeric setting as the form shows it.
pub struct Field {
    pub name: &'static str,
    pub label_key: &'static str,
    /// Extra help under the field, besides the environment value.
    pub help_key: Option<&'static str>,
    pub value: String,
    pub default_text: String,
    pub default: i64,
    pub min: i64,
    pub max: i64,
}

/// The fixed description of one field.
struct Spec {
    name: &'static str,
    label_key: &'static str,
    help_key: Option<&'static str>,
    range: RangeInclusive<i64>,
}

const SPECS: [Spec; FIELD_COUNT] = [
    Spec {
        name: "interval",
        label_key: "settings.interval",
        help_key: None,
        range: INTERVAL_RANGE,
    },
    Spec {
        name: "concurrency",
        label_key: "settings.concurrency",
        help_key: None,
        range: CONCURRENCY_RANGE,
    },
    Spec {
        name: "git_timeout",
        label_key: "settings.git_timeout",
        help_key: None,
        range: GIT_TIMEOUT_RANGE,
    },
    Spec {
        name: "fsck_interval",
        label_key: "settings.fsck_interval",
        help_key: Some("settings.fsck_help"),
        range: MAINTENANCE_RANGE,
    },
    Spec {
        name: "repack_interval",
        label_key: "settings.repack_interval",
        help_key: Some("settings.repack_help"),
        range: MAINTENANCE_RANGE,
    },
];

fn defaults(settings: SyncSettings) -> [i64; FIELD_COUNT] {
    [
        settings.interval_minutes,
        settings.concurrency,
        settings.git_timeout_seconds,
        settings.fsck_interval_days,
        settings.repack_interval_days,
    ]
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    layout: Layout,
    fields: Vec<Field>,
    notify: NotifyView,
    notice: Option<String>,
    errors: Vec<String>,
}

/// What differs between the renderings of the page. `None` parts show the
/// saved values.
pub struct PageState {
    pub notice: Option<String>,
    pub errors: Vec<String>,
    /// Submitted sync field values, shown again with an error.
    pub sync_values: Option<[Option<String>; FIELD_COUNT]>,
    pub notify: Option<NotifyView>,
    pub status: StatusCode,
}

impl Default for PageState {
    fn default() -> Self {
        Self {
            notice: None,
            errors: Vec::new(),
            sync_values: None,
            notify: None,
            status: StatusCode::OK,
        }
    }
}

/// Renders the whole page; every form posts back to a handler that calls this.
pub async fn respond(
    ctx: &PageContext,
    layout: Layout,
    state: PageState,
) -> Result<Response, AppError> {
    let values = match state.sync_values {
        Some(values) => values,
        None => saved_values(ctx).await?,
    };
    let notify = match state.notify {
        Some(view) => view,
        None => settings_notify::load(ctx, &layout).await?,
    };
    let page = SettingsPage {
        fields: fields(
            &layout,
            values,
            SyncSettings::from_config(&ctx.state.config),
        ),
        notify,
        layout,
        notice: state.notice,
        errors: state.errors,
    };
    Ok((state.status, render(&page)?).into_response())
}

async fn saved_values(ctx: &PageContext) -> Result<[Option<String>; FIELD_COUNT], AppError> {
    let saved = settings::overrides(&ctx.state.pool).await?;
    Ok([
        saved.interval_minutes,
        saved.concurrency,
        saved.git_timeout_seconds,
        saved.fsck_interval_days,
        saved.repack_interval_days,
    ]
    .map(|v| v.map(|n| n.to_string())))
}

fn fields(layout: &Layout, values: [Option<String>; FIELD_COUNT], env: SyncSettings) -> Vec<Field> {
    SPECS
        .iter()
        .zip(values)
        .zip(defaults(env))
        .map(|((spec, value), default)| Field {
            name: spec.name,
            label_key: spec.label_key,
            help_key: spec.help_key,
            value: value.unwrap_or_default(),
            default_text: layout.tf_number("settings.env_default", &default),
            default,
            min: *spec.range.start(),
            max: *spec.range.end(),
        })
        .collect()
}

pub fn layout(ctx: &PageContext) -> Layout {
    Layout::private(
        &ctx.state,
        ctx.prefs,
        &ctx.session,
        Nav::Settings,
        &ctx.path,
    )
}

/// Maps the `notice` query value to a message key, ignoring anything else.
fn notice_key(notice: Option<&str>) -> Option<&'static str> {
    match notice? {
        "saved" => Some("settings.saved"),
        "notify_saved" => Some("settings.notify_saved"),
        _ => None,
    }
}

pub async fn show(
    ctx: PageContext,
    Query(query): Query<NoticeQuery>,
) -> Result<Response, AppError> {
    let layout = layout(&ctx);
    let state = PageState {
        notice: notice_key(query.notice.as_deref()).map(|key| layout.t(key).to_owned()),
        ..PageState::default()
    };
    respond(&ctx, layout, state).await
}

#[derive(Debug, Deserialize)]
pub struct SettingsForm {
    interval: Option<String>,
    concurrency: Option<String>,
    git_timeout: Option<String>,
    fsck_interval: Option<String>,
    repack_interval: Option<String>,
}

impl SettingsForm {
    fn values(self) -> [Option<String>; FIELD_COUNT] {
        [
            self.interval,
            self.concurrency,
            self.git_timeout,
            self.fsck_interval,
            self.repack_interval,
        ]
    }
}

/// Parses every field; the error list names each field out of its range.
fn parse(
    layout: &Layout,
    values: &[Option<String>; FIELD_COUNT],
) -> Result<[Option<i64>; FIELD_COUNT], Vec<String>> {
    let mut parsed = [None; FIELD_COUNT];
    let mut errors = Vec::new();
    for ((slot, value), spec) in parsed.iter_mut().zip(values).zip(&SPECS) {
        match optional_number(value.as_deref(), spec.range.clone()) {
            Ok(number) => *slot = number,
            Err(InvalidNumber) => errors.push(layout.tf(
                "settings.out_of_range",
                &[
                    ("field", layout.t(spec.label_key)),
                    ("min", &spec.range.start().to_string()),
                    ("max", &spec.range.end().to_string()),
                ],
            )),
        }
    }
    if errors.is_empty() {
        Ok(parsed)
    } else {
        Err(errors)
    }
}

pub async fn save(ctx: PageContext, Form(form): Form<SettingsForm>) -> Result<Response, AppError> {
    let layout = layout(&ctx);
    let values = form.values();
    let parsed = match parse(&layout, &values) {
        Ok(parsed) => parsed,
        Err(errors) => {
            let state = PageState {
                errors,
                sync_values: Some(values),
                status: StatusCode::UNPROCESSABLE_ENTITY,
                ..PageState::default()
            };
            return respond(&ctx, layout, state).await;
        }
    };
    let [
        interval_minutes,
        concurrency,
        git_timeout_seconds,
        fsck_interval_days,
        repack_interval_days,
    ] = parsed;
    let overrides = Overrides {
        interval_minutes,
        concurrency,
        git_timeout_seconds,
        fsck_interval_days,
        repack_interval_days,
    };
    settings::save(&ctx.state.pool, &overrides).await?;
    tracing::info!(?overrides, "sync settings saved");
    // A higher concurrency may let waiting repositories start now.
    ctx.state.scheduler.wake();
    Ok(Redirect::to("/settings?notice=saved").into_response())
}
