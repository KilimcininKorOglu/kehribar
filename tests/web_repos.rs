//! Repository management and settings pages.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::http::{StatusCode, header};
use common::{TestApp, body_text, header_value, urlencode};
use git_archive::db::repos::{self, RepoStatus};
use git_archive::db::unix_now;
use git_archive::settings;

const URL: &str = "https%3A%2F%2Fgithub.com%2Frust-lang%2Frust.git";

#[tokio::test]
async fn invalid_url_is_rejected_with_a_message() {
    let t = TestApp::new().await;
    let response = t
        .post("/repos", "url=git%3A%2F%2Fgithub.com%2Fa%2Fb&wiki=1")
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body_text(response).await;
    assert!(
        html.contains("Yalnız http, https ve ssh ile başlayan URL&#39;ler kabul edilir."),
        "{html}"
    );
    assert!(repos::list(&t.state.pool, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn bulk_add_reports_every_line_and_keeps_the_failed_ones() {
    let t = TestApp::new().await;
    t.post("/repos", &format!("url={URL}")).await;
    let lines = [
        "https://github.com/tokio-rs/axum",
        "",
        "  git@gitlab.com:group/project.git  ",
        "https://github.com/rust-lang/rust",
        "ftp://example.com/a/b",
    ]
    .join("\n");
    let response = t
        .post(
            "/repos/bulk",
            &format!("urls={}&interval=90", urlencode(&lines)),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    assert!(html.contains("2 repo eklendi."), "{html}");
    assert!(html.contains("href=\"/repos/2\""));
    assert!(html.contains("Zaten arşivde"));
    assert!(html.contains("Yalnız http, https ve ssh ile başlayan"));
    // Only the invalid line stays in the field.
    assert!(html.contains(">ftp://example.com/a/b</textarea>"), "{html}");

    let added = repos::list(&t.state.pool, None).await.unwrap();
    assert_eq!(added.len(), 3);
    let gitlab = added.iter().find(|r| r.host == "gitlab.com").unwrap();
    assert_eq!(gitlab.url, "git@gitlab.com:group/project.git");
    assert_eq!(gitlab.interval_minutes, Some(90));
    assert!(!gitlab.wiki_enabled);

    let bad = t.post("/repos/bulk", "urls=x&interval=0").await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(repos::list(&t.state.pool, None).await.unwrap().len(), 3);
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
async fn a_gist_page_is_added_under_its_owner_and_a_bare_gist_is_rejected() {
    let t = TestApp::new().await;
    let bare = t
        .post("/repos", "url=https%3A%2F%2Fgist.github.com%2Fabc123.git")
        .await;
    assert_eq!(bare.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body_text(bare).await;
    assert!(html.contains("sahibinin adıyla birlikte girin"), "{html}");

    let response = t
        .post(
            "/repos",
            "url=https%3A%2F%2Fgist.github.com%2Foctocat%2Fabc123",
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let repo = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(repo.url, "https://gist.github.com/abc123.git");
    assert_eq!(
        (repo.host.as_str(), repo.owner.as_str(), repo.name.as_str()),
        ("gist.github.com", "octocat", "abc123")
    );
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

/// Adds `example.com/team/repo-000` to `repo-119` (ids 1 to 120). Every
/// fourth one from 0 failed, every fourth one from 1 is up to date with a
/// size of `i * 1000`; both last synced at `1000 + i`. The rest are pending
/// and never synced.
async fn seed_list(pool: &sqlx::SqlitePool) {
    for i in 0..120_i64 {
        let name = format!("repo-{i:03}");
        let url = format!("https://example.com/team/{name}.git");
        let new = repos::NewRepo {
            url: &url,
            host: "example.com",
            owner: "team",
            name: &name,
            source_id: None,
            username: None,
            token_enc: None,
            interval_minutes: None,
            wiki_enabled: false,
        };
        let id = repos::insert(pool, &new, 1).await.unwrap().unwrap();
        let status = match i % 4 {
            0 => RepoStatus::Error,
            1 => RepoStatus::Ok,
            _ => continue,
        };
        assert!(repos::claim(pool, id, 1000 + i).await.unwrap());
        let outcome = repos::SyncOutcome {
            status,
            finished_at: 1000 + i,
            next_sync_at: 5000,
            error: None,
            size_bytes: if status == RepoStatus::Ok {
                i * 1000
            } else {
                0
            },
            lfs_size_bytes: 0,
            default_branch: None,
            has_wiki: false,
        };
        repos::finish(pool, id, &outcome).await.unwrap();
    }
}

/// The repository ids of the list rows, in order.
fn row_ids(html: &str) -> Vec<i64> {
    html.split("<a class=\"repo-name\" href=\"/repos/")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap().parse().unwrap())
        .collect()
}

#[tokio::test]
async fn list_is_paged_by_fifty() {
    let t = TestApp::new().await;
    seed_list(&t.state.pool).await;

    let first = body_text(t.get("/").await).await;
    assert_eq!(row_ids(&first), (1..=50).collect::<Vec<_>>());
    assert!(
        first.contains("120 repodan 1–50 arası gösteriliyor."),
        "{first}"
    );
    assert!(first.contains("href=\"/?page=2\" rel=\"next\""));
    assert!(!first.contains("rel=\"prev\""));

    let third = body_text(t.get("/?page=3").await).await;
    assert_eq!(row_ids(&third), (101..=120).collect::<Vec<_>>());
    assert!(third.contains("120 repodan 101–120 arası gösteriliyor."));
    assert!(third.contains("<span aria-current=\"page\">3</span>"));
    assert!(third.contains("href=\"/?page=2\" rel=\"prev\""));
    assert!(!third.contains("rel=\"next\""));

    // A page past the end shows the last page; a bad number the first.
    assert_eq!(
        row_ids(&body_text(t.get("/?page=99").await).await),
        row_ids(&third)
    );
    assert_eq!(
        row_ids(&body_text(t.get("/?page=x").await).await),
        row_ids(&first)
    );
}

#[tokio::test]
async fn status_filter_shows_counts_and_keeps_the_sort() {
    let t = TestApp::new().await;
    seed_list(&t.state.pool).await;

    let all = body_text(t.get("/").await).await;
    assert!(
        all.contains("Tümü <span class=\"chip-count\">120</span>"),
        "{all}"
    );
    assert!(all.contains("Hata <span class=\"chip-count\">30</span>"));
    assert!(all.contains("Güncel <span class=\"chip-count\">30</span>"));
    assert!(all.contains("Sırada <span class=\"chip-count\">60</span>"));
    // A status without repositories gets no chip.
    assert!(!all.contains("Güncelleniyor <span"));

    let errors = body_text(t.get("/?status=error&sort=size&dir=desc").await).await;
    let ids = row_ids(&errors);
    assert_eq!(ids.len(), 30);
    assert!(ids.iter().all(|id| (id - 1) % 4 == 0), "{ids:?}");
    // One page only, so no pager.
    assert!(!errors.contains("class=\"pager\""));
    assert!(
        errors.contains("href=\"/?status=error&#38;sort=size&#38;dir=desc\" aria-current=\"true\"")
    );
    assert!(errors.contains("href=\"/?sort=size&#38;dir=desc\">Tümü"));
    // The search form keeps the filter and the sort.
    assert!(errors.contains("<input type=\"hidden\" name=\"status\" value=\"error\">"));
    assert!(errors.contains("<input type=\"hidden\" name=\"dir\" value=\"desc\">"));

    // An unknown status lists every repository.
    assert_eq!(
        row_ids(&body_text(t.get("/?status=nope").await).await).len(),
        50
    );
}

#[tokio::test]
async fn every_column_sorts_both_ways() {
    let t = TestApp::new().await;
    seed_list(&t.state.pool).await;
    let first_ids = async |query: &str| row_ids(&body_text(t.get(query).await).await);

    assert_eq!(first_ids("/?sort=name&dir=asc").await[0], 1);
    assert_eq!(first_ids("/?sort=name&dir=desc").await[0], 120);
    // The largest repository is repo-117; size ties sort by name.
    assert_eq!(first_ids("/?sort=size&dir=desc").await[0], 118);
    assert_eq!(first_ids("/?sort=size&dir=asc").await[..2], [1, 3]);
    // Last synced: repo-000 first, repo-117 last; never synced ones come last both ways.
    assert_eq!(first_ids("/?sort=synced&dir=asc").await[0], 1);
    let synced_desc = first_ids("/?sort=synced&dir=desc").await;
    assert_eq!(synced_desc[..2], [118, 117]);
    let tail = first_ids("/?sort=synced&dir=desc&page=3").await;
    assert_eq!(tail.last(), Some(&120));
    assert_eq!(
        first_ids("/?sort=synced&dir=asc&page=3").await.last(),
        Some(&120)
    );
    // Problems first, up to date last.
    assert_eq!(first_ids("/?sort=status&dir=asc").await[0], 1);
    assert_eq!(first_ids("/?sort=status&dir=desc").await[0], 2);
    // An unknown column sorts by name.
    assert_eq!(first_ids("/?sort=bogus").await[0], 1);

    let html = body_text(t.get("/?sort=size&dir=asc").await).await;
    // The sorted column turns its direction; another column starts ascending.
    assert!(html.contains("aria-sort=\"ascending\">"));
    assert!(
        html.contains("href=\"/?sort=size&#38;dir=desc\">Boyut"),
        "{html}"
    );
    // Name ascending is the default, so its link carries no sort.
    assert!(html.contains("href=\"/\">Repo"));
    let name_asc = body_text(t.get("/").await).await;
    assert!(name_asc.contains("href=\"/?sort=name&#38;dir=desc\">Repo"));
}

#[tokio::test]
async fn search_and_status_filter_work_together() {
    let t = TestApp::new().await;
    seed_list(&t.state.pool).await;

    let html = body_text(t.get("/?q=repo-01&status=ok").await).await;
    // repo-013 and repo-017 are the up to date ones of repo-010 to repo-019.
    assert_eq!(row_ids(&html), [14, 18]);
    assert!(
        html.contains("Tümü <span class=\"chip-count\">10</span>"),
        "{html}"
    );
    assert!(html.contains("Güncel <span class=\"chip-count\">2</span>"));
    assert!(html.contains("href=\"/?q=repo-01&#38;status=error\""));
    // The total under the title counts the whole archive.
    assert!(html.contains("Arşivde 120 repo var."));

    let none = body_text(t.get("/?q=repo-01&status=syncing").await).await;
    assert!(none.contains("Aramanızla eşleşen repo yok."));
    // The current filter keeps its chip even with no repository.
    assert!(none.contains("Güncelleniyor <span class=\"chip-count\">0</span>"));
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
            lfs_size_bytes: 0,
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
