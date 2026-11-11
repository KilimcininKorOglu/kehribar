//! The disk page: the archive size, the largest repositories and the size
//! of the last 90 days as a chart drawn on the server.

use askama::Template;
use axum::response::{IntoResponse, Response};

use super::format;
use super::pages::{Layout, Nav, render};
use super::repos::PageContext;
use crate::db::repos::{self, ListFilter, Repo, RepoSort};
use crate::db::unix_now;
use crate::db::usage::{self, DAY_SECONDS, DayUsage, Totals};
use crate::error::AppError;
use crate::i18n::Lang;

/// Days the chart shows, today included.
const CHART_DAYS: i64 = 90;

/// Repositories in the largest list.
const LARGEST: i64 = 20;

// The plot `viewBox`. The SVG stretches to its box, so the labels are HTML
// around it and keep their size on every screen; templates/disk.html draws
// the grid lines at the top, the middle and the bottom of this box.
const WIDTH: f64 = 1000.0;
const HEIGHT: f64 = 300.0;

#[derive(Template)]
#[template(path = "disk.html")]
struct DiskPage {
    layout: Layout,
    totals: Totals,
    largest: Vec<Largest>,
    chart: Option<Chart>,
}

/// A repository of the largest list with its share of the archive.
struct Largest {
    repo: Repo,
    share: String,
}

/// The size chart; the coordinates are in the plot `viewBox`.
#[derive(Debug)]
pub struct Chart {
    /// `x,y x,y …` of the size line.
    pub line: String,
    /// The line closed along the zero line, for the filled area.
    pub area: String,
    /// The point of the last recorded day.
    pub last: (String, String),
    /// The labels of the top, middle and bottom grid lines.
    pub sizes: [String; 3],
    /// The first and the last day of the chart.
    pub dates: [String; 2],
    /// The chart in words, for screen readers.
    pub summary: String,
}

pub async fn show(ctx: PageContext) -> Result<Response, AppError> {
    let pool = &ctx.state.pool;
    let now = unix_now();
    let totals = usage::totals(pool).await?;
    let filter = ListFilter {
        search: "",
        status: None,
        sort: RepoSort::Size,
        descending: true,
        limit: LARGEST,
        offset: 0,
    };
    let layout = ctx.layout(Nav::Disk);
    let largest = repos::page(pool, &filter)
        .await?
        .into_iter()
        .filter(|repo| repo.size_bytes > 0)
        .map(|repo| Largest {
            share: share(&layout, repo.size_bytes, totals.total_bytes),
            repo,
        })
        .collect();
    let days = usage::last_days(pool, now, CHART_DAYS).await?;
    let page = DiskPage {
        chart: chart(&layout, &days, now / DAY_SECONDS),
        layout,
        totals,
        largest,
    };
    Ok(render(&page)?.into_response())
}

/// `12.5%` (`%12,5` in Turkish), the share of `part` in `whole`.
fn share(layout: &Layout, part: i64, whole: i64) -> String {
    let percent = if whole > 0 {
        part as f64 * 100.0 / whole as f64
    } else {
        0.0
    };
    let text = format!("{percent:.1}");
    let text = match layout.tr.lang() {
        Lang::Tr => text.replace('.', ","),
        Lang::En => text,
    };
    layout.tf("disk.share", &[("n", &text)])
}

/// A coordinate as the SVG gets it.
fn coord(value: f64) -> String {
    format!("{value:.1}")
}

/// The chart of `days`, which ends on the epoch day `today`; `None` when
/// no day is recorded.
pub fn chart(layout: &Layout, days: &[DayUsage], today: i64) -> Option<Chart> {
    let first = days.first()?;
    let last = days.last()?;
    let start = today - (CHART_DAYS - 1);
    let top = days.iter().map(|d| d.total_bytes).max().unwrap_or(0).max(1);
    let x = |day: i64| (day - start) as f64 * WIDTH / (CHART_DAYS - 1) as f64;
    let y = |bytes: i64| HEIGHT - bytes as f64 * HEIGHT / top as f64;
    let points: Vec<String> = days
        .iter()
        .map(|d| format!("{},{}", coord(x(d.epoch_day)), coord(y(d.total_bytes))))
        .collect();
    let line = points.join(" ");
    let area = format!(
        "{line} {},{} {},{}",
        coord(x(last.epoch_day)),
        coord(HEIGHT),
        coord(x(first.epoch_day)),
        coord(HEIGHT)
    );
    let sizes = [top, top / 2, 0].map(|bytes| layout.bytes(&bytes));
    let dates = [start, today].map(|day| format::date(day * DAY_SECONDS));
    let summary = layout.tf(
        "disk.chart_summary",
        &[
            ("from", &first.day),
            ("first", &layout.bytes(&first.total_bytes)),
            ("to", &last.day),
            ("last", &layout.bytes(&last.total_bytes)),
        ],
    );
    Some(Chart {
        line,
        area,
        last: (coord(x(last.epoch_day)), coord(y(last.total_bytes))),
        sizes,
        dates,
        summary,
    })
}
