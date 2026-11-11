//! The ref history of a repository: what each branch and tag pointed to at
//! the end of a chosen day, and every change the archive saw until then.

use askama::Template;
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use time::macros::format_description;
use time::{Date, OffsetDateTime};

use super::pages::{Layout, Nav, render};
use super::repos::PageContext;
use crate::db::ref_events::{self, EventKind, RefEvent};
use crate::db::repos::{self, Repo};
use crate::db::unix_now;
use crate::db::usage::DAY_SECONDS;
use crate::error::AppError;

/// Events on one page of the change list.
const PAGE_SIZE: i64 = 50;

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    at: Option<String>,
    page: Option<String>,
}

#[derive(Template)]
#[template(path = "history.html")]
struct HistoryPage {
    layout: Layout,
    repo: Repo,
    tab: &'static str,
    /// The chosen day, `YYYY-MM-DD` in UTC.
    day: String,
    today: String,
    /// What each ref pointed to at the end of the day.
    states: Vec<RefEvent>,
    /// When the recording began; `None` before the first sync with history.
    since: Option<i64>,
    /// Set when the day is before the recording: the rewrites after it.
    rewrites: Vec<RefEvent>,
    events: Vec<RefEvent>,
    page: i64,
    has_next: bool,
}

impl HistoryPage {
    fn is_before_recording(&self) -> bool {
        self.since.is_some_and(|since| self.end() < since)
    }

    /// The last second of the chosen day.
    fn end(&self) -> i64 {
        day_end(&self.day).unwrap_or(i64::MAX)
    }

    fn state_title(&self) -> String {
        self.layout
            .tf("history.state_title", &[("date", &self.day)])
    }

    /// Says when the recording began and what the history knows before it.
    fn before_note(&self) -> String {
        let since = self.since.map(super::format::absolute).unwrap_or_default();
        self.layout
            .tf("history.before_recording", &[("date", &since)])
    }

    fn is_deleted(&self, event: &RefEvent) -> bool {
        event.kind == EventKind::DeletedAtSource
    }

    /// Takes a value: the template passes a computed page number.
    fn page_url(&self, page: i64) -> String {
        let mut pairs = url::form_urlencoded::Serializer::new(String::new());
        pairs.append_pair("at", &self.day);
        if page > 1 {
            pairs.append_pair("page", &page.to_string());
        }
        format!("/repos/{}/history?{}", self.repo.id, pairs.finish())
    }

    fn previous_page(&self) -> i64 {
        self.page - 1
    }

    fn next_page(&self) -> i64 {
        self.page + 1
    }
}

/// `YYYY-MM-DD` of a Unix time, in UTC.
fn day_of(ts: i64) -> String {
    super::format::date(ts)
}

/// The last second (UTC) of a `YYYY-MM-DD` day; `None` for another text.
fn day_end(day: &str) -> Option<i64> {
    let date = Date::parse(day, format_description!("[year]-[month]-[day]")).ok()?;
    Some(OffsetDateTime::new_utc(date, time::Time::MIDNIGHT).unix_timestamp() + DAY_SECONDS - 1)
}

impl HistoryQuery {
    /// The last second of the chosen day (today without `at`) and the page.
    /// A day that is not `YYYY-MM-DD` is `None`.
    fn parse(&self, now: i64) -> Option<(i64, i64)> {
        let end = match self.at.as_deref().map(str::trim) {
            Some(at) if !at.is_empty() => day_end(at)?,
            _ => day_end(&day_of(now))?,
        };
        let page = self
            .page
            .as_deref()
            .and_then(|p| p.parse::<i64>().ok())
            .unwrap_or(1)
            .max(1);
        Some((end, page))
    }
}

/// When the recording began and, for a day `end` before it, the later
/// rewrites whose old commits are all the history knows of that day.
async fn recording(
    pool: &sqlx::SqlitePool,
    id: i64,
    end: i64,
) -> sqlx::Result<(Option<i64>, Vec<RefEvent>)> {
    let since = ref_events::recording_since(pool, id).await?;
    let rewrites = if since.is_some_and(|since| end < since) {
        ref_events::next_rewrites(pool, id, end).await?
    } else {
        Vec::new()
    };
    Ok((since, rewrites))
}

pub async fn show(
    ctx: PageContext,
    Path(id): Path<i64>,
    Query(query): Query<HistoryQuery>,
) -> Result<Response, AppError> {
    let pool = &ctx.state.pool;
    let repo = repos::get(pool, id).await?.ok_or(AppError::NotFound)?;
    let now = unix_now();
    let (end, page) = query.parse(now).ok_or(AppError::NotFound)?;
    let (since, rewrites) = recording(pool, id, end).await?;
    let total = ref_events::count_until(pool, id, end).await?;
    let page = HistoryPage {
        layout: ctx.layout(Nav::Repos),
        tab: "history",
        day: day_of(end),
        today: day_of(now),
        states: ref_events::state_at(pool, id, end).await?,
        since,
        rewrites,
        events: ref_events::list_until(pool, id, end, PAGE_SIZE, (page - 1) * PAGE_SIZE).await?,
        has_next: page * PAGE_SIZE < total,
        page,
        repo,
    };
    Ok(render(&page)?.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_day_ends_at_its_last_utc_second() {
        assert_eq!(day_end("1970-01-01"), Some(86_399));
        assert_eq!(day_end("2025-11-11"), Some(1_762_905_599));
        assert_eq!(day_of(1_762_905_599), "2025-11-11");
        assert_eq!(day_end("2025-13-01"), None);
        assert_eq!(day_end("yesterday"), None);
    }
}
