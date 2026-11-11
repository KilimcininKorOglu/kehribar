//! Repository management and settings pages.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::http::{StatusCode, header};
use common::{TestApp, body_text, header_value};
use git_archive::db::repos::{self, RepoStatus};
use git_archive::db::unix_now;
use git_archive::settings;

const URL: &str = "https%3A%2F%2Fgithub.com%2Frust-lang%2Frust.git";

#[tokio::test]
async fn invalid_url_is_rejected_with_a_message() {
    let t = TestApp::new().await;
    let response = t
        .post("/repos", "url=ssh%3A%2F%2Fgithub.com%2Fa%2Fb&wiki=1")
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body_text(response).await;
    assert!(
        html.contains("Yalnız http ve https ile başlayan URL&#39;ler kabul edilir."),
        "{html}"
    );
    assert!(repos::list(&t.state.pool, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn adding_a_repository_lists_it_and_rejects_a_duplicate() {
    let t = TestApp::new().await;
    let response = t
        .post("/repos", &format!("url={URL}&wiki=1&interval=45"))
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(header_value(&response, header::LOCATION), "/repos/1");

    let repo = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(repo.url, "https://github.com/rust-lang/rust.git");
    assert_eq!(
        (repo.owner.as_str(), repo.name.as_str()),
        ("rust-lang", "rust")
    );
    assert_eq!(repo.interval_minutes, Some(45));
    assert!(repo.wiki_enabled);
    assert_eq!(repo.status, RepoStatus::Pending);

    let list = body_text(t.get("/").await).await;
    assert!(list.contains("href=\"/repos/1\""));
    assert!(list.contains("Sırada"));
    // A pending repository makes the table refresh itself.
    assert!(list.contains("hx-trigger=\"every 5s\""));

    let again = t
        .post("/repos", "url=https%3A%2F%2Fgithub.com%2FRust-Lang%2FRust")
        .await;
    assert_eq!(again.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_text(again).await.contains("Bu repo zaten arşivde."));
}

#[tokio::test]
async fn unchecked_wiki_and_bad_interval_are_handled() {
    let t = TestApp::new().await;
    let bad = t.post("/repos", &format!("url={URL}&interval=0")).await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);

    t.post("/repos", &format!("url={URL}")).await;
    let repo = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert!(!repo.wiki_enabled);
    assert_eq!(repo.interval_minutes, None);
}

#[tokio::test]
async fn search_filters_the_list() {
    let t = TestApp::new().await;
    t.post("/repos", &format!("url={URL}")).await;
    t.post(
        "/repos",
        "url=https%3A%2F%2Fcodeberg.org%2Fforgejo%2Fforgejo",
    )
    .await;

    let html = body_text(t.get("/?q=forgejo").await).await;
    assert!(html.contains("/repos/2"));
    assert!(!html.contains("/repos/1\""));
    let none = body_text(t.get("/?q=nothing-like-this").await).await;
    assert!(none.contains("Aramanızla eşleşen repo yok."));
}

#[tokio::test]
async fn detail_page_sync_now_and_repository_settings() {
    let t = TestApp::new().await;
    t.post("/repos", &format!("url={URL}")).await;
    repos::finish(
        &t.state.pool,
        1,
        &repos::SyncOutcome {
            status: RepoStatus::Ok,
            finished_at: unix_now(),
            next_sync_at: unix_now() + 3600,
            error: None,
            size_bytes: 2048,
            default_branch: Some("main"),
            has_wiki: false,
        },
    )
    .await
    .unwrap();

    let html = body_text(t.get("/repos/1").await).await;
    assert!(html.contains("rust-lang/<strong>rust</strong>"));
    assert!(html.contains("2,0 KB"));
    assert!(html.contains("Boş bırakırsanız genel aralık kullanılır: 360 dakika."));

    let queued = t.post("/repos/1/sync", "").await;
    assert_eq!(
        header_value(&queued, header::LOCATION),
        "/repos/1?notice=queued"
    );
    let repo = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert!(repo.next_sync_at <= unix_now());
    assert!(
        body_text(t.get("/repos/1?notice=queued").await)
            .await
            .contains("Güncelleme sıraya alındı.")
    );

    let bad = t.post("/repos/1/settings", "interval=99999").await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let saved = t.post("/repos/1/settings", "interval=20&wiki=1").await;
    assert_eq!(
        header_value(&saved, header::LOCATION),
        "/repos/1?notice=saved"
    );
    let repo = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(repo.interval_minutes, Some(20));
    assert!(repo.wiki_enabled);
}

#[tokio::test]
async fn delete_needs_the_exact_name_and_removes_the_archive() {
    let t = TestApp::new().await;
    t.post("/repos", &format!("url={URL}")).await;
    let repo = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    let dir = git_archive::repo_url::archive_dir(
        &t.state.config.data_dir,
        &repo.host,
        &repo.owner,
        &repo.name,
    );
    let wiki = git_archive::git::mirror::wiki_dir(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&wiki).unwrap();

    let wrong = t.post("/repos/1/delete", "confirm=rust").await;
    assert_eq!(wrong.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_text(wrong).await.contains("Repo silinmedi."));
    assert!(dir.exists());

    let deleted = t.post("/repos/1/delete", "confirm=rust-lang%2Frust").await;
    assert_eq!(header_value(&deleted, header::LOCATION), "/");
    assert!(repos::get(&t.state.pool, 1).await.unwrap().is_none());
    assert!(!dir.exists());
    assert!(!wiki.exists());
    // The owner directory may stay; the data directory itself must stay.
    assert!(t.state.config.data_dir.join("repos").exists());
}

#[tokio::test]
async fn syncing_repository_cannot_be_deleted() {
    let t = TestApp::new().await;
    t.post("/repos", &format!("url={URL}")).await;
    repos::claim(&t.state.pool, 1, unix_now()).await.unwrap();
    let response = t.post("/repos/1/delete", "confirm=rust-lang%2Frust").await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(repos::get(&t.state.pool, 1).await.unwrap().is_some());
}

#[tokio::test]
async fn unknown_repository_is_not_found() {
    let t = TestApp::new().await;
    assert_eq!(t.get("/repos/99").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        t.post("/repos/99/sync", "").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn settings_page_saves_overrides_and_rejects_out_of_range_values() {
    let t = TestApp::new().await;
    let page = body_text(t.get("/settings").await).await;
    assert!(page.contains("Ortam değişkenindeki değer: 360"));

    let bad = t
        .post("/settings", "interval=0&concurrency=&git_timeout=")
        .await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_text(bad).await.contains("1 ile 10080 arasında"));
    let bad = t.post("/settings", "repack_interval=400").await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body_text(bad)
            .await
            .contains("Disk bakımı aralığı (gün): 0 ile 365 arasında")
    );

    let saved = t
        .post(
            "/settings",
            "interval=30&concurrency=4&git_timeout=&fsck_interval=0&repack_interval=14",
        )
        .await;
    assert_eq!(
        header_value(&saved, header::LOCATION),
        "/settings?notice=saved"
    );
    let effective = settings::effective(&t.state.pool, &t.state.config)
        .await
        .unwrap();
    assert_eq!(effective.interval_minutes, 30);
    assert_eq!(effective.concurrency, 4);
    assert_eq!(effective.git_timeout_seconds, 3600);
    assert_eq!(effective.fsck_interval_days, 0);
    assert_eq!(effective.repack_interval_days, 14);

    let cleared = t
        .post("/settings", "interval=&concurrency=&git_timeout=")
        .await;
    assert_eq!(cleared.status(), StatusCode::SEE_OTHER);
    let effective = settings::effective(&t.state.pool, &t.state.config)
        .await
        .unwrap();
    assert_eq!(effective.interval_minutes, 360);
}
