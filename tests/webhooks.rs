//! Push webhooks: the repository page creates, renews and removes the URL,
//! and a call to it syncs the repository.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{TestApp, archive_source, page, send, source_repo};
use kehribar::db::repos;
use kehribar::db::sync_runs;
use kehribar::web::auth::MAX_FAILED_LOGINS;

/// The webhook URL path the repository page shows.
async fn webhook_path(t: &TestApp) -> Option<String> {
    let (_, html) = page(t, "/repos/1").await;
    let start = html.find("/hooks/khw_")?;
    Some(html[start..start + "/hooks/khw_".len() + 64].to_owned())
}

/// A call from the source server: no session, no CSRF token, any body.
async fn call(t: &TestApp, path: &str) -> StatusCode {
    let request = Request::post(path)
        .header("content-type", "application/json")
        .header("x-github-event", "push")
        .body(Body::from("{\"ref\":\"refs/heads/main\"}"))
        .unwrap();
    send(&t.app, request).await.status()
}

async fn next_sync_at(t: &TestApp) -> i64 {
    repos::get(&t.state.pool, 1)
        .await
        .unwrap()
        .unwrap()
        .next_sync_at
}

#[tokio::test]
async fn a_webhook_call_syncs_the_repository() {
    let t = TestApp::new().await;
    let source = source_repo(t.dir.path(), "source");
    archive_source(&t, &source, false).await;
    assert_eq!(webhook_path(&t).await, None);

    let response = t.post("/repos/1/webhook", "").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let (_, html) = page(&t, "/repos/1?notice=webhook_created").await;
    assert!(html.contains("hazır. Kaynak sunucuya ekleyin."), "{html}");
    assert!(html.contains("GitLab: Settings → Webhooks"), "{html}");
    let path = webhook_path(&t).await.expect("a webhook URL");
    // The page shows the full URL; the token is stored encrypted.
    assert!(html.contains(&format!("http://localhost{path}")), "{html}");
    let stored: Vec<u8> = sqlx::query_scalar("SELECT webhook_enc FROM repos")
        .fetch_one(&t.state.pool)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&stored).contains("khw_"));

    // Nothing is due; the webhook makes the repository due now.
    sqlx::query("UPDATE repos SET next_sync_at = 9999999999")
        .execute(&t.state.pool)
        .await
        .unwrap();
    assert_eq!(call(&t, &path).await, StatusCode::ACCEPTED);
    assert!(next_sync_at(&t).await < 9_999_999_999);
    for handle in t.state.scheduler.tick().await.unwrap() {
        handle.await.unwrap();
    }
    let runs = sync_runs::list_for_repo(&t.state.pool, 1, 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 2);
}

/// An archived repository with a webhook URL, and the URL's path.
async fn with_webhook() -> (TestApp, String) {
    let t = TestApp::new().await;
    let source = source_repo(t.dir.path(), "source");
    archive_source(&t, &source, false).await;
    t.post("/repos/1/webhook", "").await;
    let path = webhook_path(&t).await.unwrap();
    (t, path)
}

#[tokio::test]
async fn calls_during_a_sync_ask_for_one_more() {
    let (t, path) = with_webhook().await;
    sqlx::query("UPDATE repos SET status = 'syncing', next_sync_at = 9999999999")
        .execute(&t.state.pool)
        .await
        .unwrap();
    assert_eq!(call(&t, &path).await, StatusCode::ACCEPTED);
    assert_eq!(call(&t, &path).await, StatusCode::ACCEPTED);
    let requested: bool = sqlx::query_scalar("SELECT sync_requested FROM repos")
        .fetch_one(&t.state.pool)
        .await
        .unwrap();
    assert!(requested);
    assert_eq!(next_sync_at(&t).await, 9_999_999_999);
}

#[tokio::test]
async fn a_renewed_or_removed_url_stops_working() {
    let (t, old) = with_webhook().await;
    t.post("/repos/1/webhook", "").await;
    let new = webhook_path(&t).await.unwrap();
    assert_ne!(old, new);
    assert_eq!(call(&t, &old).await, StatusCode::NOT_FOUND);
    assert_eq!(call(&t, &new).await, StatusCode::ACCEPTED);

    let response = t.post("/repos/1/webhook/delete", "").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(webhook_path(&t).await, None);
    assert_eq!(call(&t, &new).await, StatusCode::NOT_FOUND);
    // Creating one needs the session and its CSRF token.
    let anonymous = send(&t.app, common::form_request("/repos/1/webhook", "")).await;
    assert_eq!(anonymous.status(), StatusCode::SEE_OTHER);
    assert_eq!(webhook_path(&t).await, None);
    assert_eq!(
        t.post("/repos/9/webhook", "").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn guessing_tokens_locks_the_caller_out() {
    let (t, path) = with_webhook().await;
    for guess in 0..MAX_FAILED_LOGINS {
        let wrong = format!("/hooks/khw_{guess:064}");
        assert_eq!(call(&t, &wrong).await, StatusCode::NOT_FOUND);
    }
    assert_eq!(call(&t, &path).await, StatusCode::TOO_MANY_REQUESTS);
    // Only POST is served.
    let get = Request::get(&path).body(Body::empty()).unwrap();
    assert_eq!(
        send(&t.app, get).await.status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
}
