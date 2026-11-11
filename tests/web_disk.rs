//! The disk page: totals, the largest repositories and the size chart.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::http::StatusCode;
use common::{TestApp, body_text};
use kehribar::db::repos::{self, NewRepo, RepoStatus, SyncOutcome};
use kehribar::db::unix_now;
use kehribar::db::usage::{self, DAY_SECONDS};

async fn add_repo(pool: &sqlx::SqlitePool, name: &str, size: i64, lfs: i64) -> i64 {
    let url = format!("https://example.com/team/{name}");
    let new = NewRepo {
        url: &url,
        host: "example.com",
        owner: "team",
        name,
        source_id: None,
        username: None,
        token_enc: None,
        interval_minutes: None,
        wiki_enabled: false,
    };
    let id = repos::insert(pool, &new, 1).await.unwrap().unwrap();
    let outcome = SyncOutcome {
        status: RepoStatus::Ok,
        finished_at: 2,
        next_sync_at: 3,
        error: None,
        size_bytes: size,
        lfs_size_bytes: lfs,
        default_branch: None,
        has_wiki: false,
    };
    repos::finish(pool, id, &outcome).await.unwrap();
    id
}

/// The `points` value of the size line.
fn line_points(html: &str) -> Vec<&str> {
    let rest = html.split("class=\"chart-line\" points=\"").nth(1).unwrap();
    rest.split('"').next().unwrap().split(' ').collect()
}

#[tokio::test]
async fn empty_archive_shows_no_chart_and_no_list() {
    let t = TestApp::new().await;
    let response = t.get("/disk").await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    assert!(
        html.contains("href=\"/disk\" aria-current=\"page\""),
        "{html}"
    );
    assert!(html.contains("<dd>0 B</dd>"));
    assert!(html.contains("Henüz kayıtlı gün yok."));
    assert!(html.contains("Henüz boyutu ölçülmüş repo yok."));
    assert!(!html.contains("class=\"chart-plot\""));
}

#[tokio::test]
async fn one_recorded_day_draws_a_point_and_lists_the_largest_first() {
    let t = TestApp::new().await;
    let pool = &t.state.pool;
    let small = add_repo(pool, "small", 1000, 0).await;
    let big = add_repo(pool, "big", 3000, 512).await;
    // A repository never measured stays out of the list.
    add_repo(pool, "empty", 0, 0).await;
    let now = unix_now();
    assert!(usage::record_day(pool, now).await.unwrap());

    let html = body_text(t.get("/disk").await).await;
    assert!(html.contains("<dd>3,9 KB</dd>"), "{html}");
    assert!(html.contains("<dd>512 B</dd>"));
    assert!(html.contains("<dd>3</dd>"));
    let big_at = html.find(&format!("href=\"/repos/{big}\"")).unwrap();
    let small_at = html.find(&format!("href=\"/repos/{small}\"")).unwrap();
    assert!(big_at < small_at);
    assert!(!html.contains("href=\"/repos/3\""));
    assert!(html.contains("%75,0"));
    assert!(html.contains("%25,0"));

    assert!(html.contains("class=\"chart-plot\""));
    // One day is one point at the top right, marked with a dot.
    assert_eq!(line_points(&html), ["1000.0,0.0"]);
    assert!(html.contains("class=\"chart-dot\" x1=\"1000.0\" y1=\"0.0\""));
    let today = time::OffsetDateTime::now_utc().date().to_string();
    assert!(
        html.contains(&format!("{today} tarihinde 3,9 KB")),
        "{html}"
    );
}

#[tokio::test]
async fn the_chart_shows_the_last_ninety_days_only() {
    let t = TestApp::new().await;
    let pool = &t.state.pool;
    let now = unix_now();
    add_repo(pool, "a", 2048, 0).await;
    usage::record_day(pool, now - 100 * DAY_SECONDS)
        .await
        .unwrap();
    usage::record_day(pool, now - 30 * DAY_SECONDS)
        .await
        .unwrap();
    add_repo(pool, "b", 2048, 0).await;
    usage::record_day(pool, now).await.unwrap();

    let html = body_text(t.get("/disk").await).await;
    let points = line_points(&html);
    assert_eq!(points.len(), 2, "{points:?}");
    // The larger day is at the top of the chart, the smaller one half way.
    let ys: Vec<f64> = points
        .iter()
        .map(|p| p.split(',').nth(1).unwrap().parse().unwrap())
        .collect();
    assert_eq!(ys, [150.0, 0.0]);
    // The date axis starts 89 days ago and ends today.
    let today = time::OffsetDateTime::now_utc().date();
    let start = today - time::Duration::days(89);
    assert!(
        html.contains(&format!("<span>{start}</span><span>{today}</span>")),
        "{html}"
    );
    // The size axis goes up to the largest day.
    assert!(html.contains("<span>4,0 KB</span><span>2,0 KB</span><span>0 B</span>"));
}
