//! External images of Markdown files, archived during the sync.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::time::{Duration, SystemTime};

use axum::http::{StatusCode, header};
use common::{TestApp, archive_source, git_in, page, source_repo, urlencode};
use git_archive::db::assets;
use git_archive::db::repos;
use git_archive::db::unix_now;
use git_archive::sync::assets::{AddressGuard, MAX_IMAGES};
use sha2::Digest;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake image";
const SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>";

fn sha256(data: &[u8]) -> String {
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn image(content_type: &str, body: &[u8]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", content_type)
        .set_body_bytes(body.to_vec())
}

/// An image server with good and bad answers.
async fn image_server() -> MockServer {
    let server = MockServer::start().await;
    let routes = [
        ("/logo.png", image("image/png", PNG)),
        ("/diagram.svg", image("image/svg+xml", SVG.as_bytes())),
        ("/page.html", image("text/html", b"<html></html>")),
        (
            "/big.png",
            image("image/png", &vec![0u8; 5 * 1024 * 1024 + 1]),
        ),
        (
            "/moved.png",
            ResponseTemplate::new(302).insert_header("location", "/logo.png"),
        ),
        (
            "/to-private.png",
            ResponseTemplate::new(302).insert_header("location", "http://10.0.0.1/a.png"),
        ),
        ("/missing.png", ResponseTemplate::new(404)),
    ];
    for (route, response) in routes {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(response)
            .mount(&server)
            .await;
    }
    server
}

fn commit(source: &Path, name: &str, content: &str) {
    if let Some(parent) = source.join(name).parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(source.join(name), content).unwrap();
    git_in(source, &["add", name]);
    git_in(source, &["commit", "--quiet", "-m", name]);
}

async fn app() -> TestApp {
    TestApp::with_guard(AddressGuard::allowing(vec![IpAddr::V4(
        Ipv4Addr::LOCALHOST,
    )]))
    .await
}

async fn sync_again(t: &TestApp) {
    repos::schedule_now(&t.state.pool, 1, unix_now())
        .await
        .unwrap();
    for handle in t.state.scheduler.tick().await.unwrap() {
        handle.await.unwrap();
    }
}

async fn requests_to(server: &MockServer, route: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == route)
        .count()
}

#[tokio::test]
async fn images_are_archived_and_shown_from_the_archive() {
    let server = image_server().await;
    let base = server.uri();
    let t = app().await;
    let source = source_repo(t.dir.path(), "source");
    commit(
        &source,
        "README.md",
        &format!(
            "# Project\n\n![logo]({base}/logo.png) ![moved]({base}/moved.png)\n\n\
             ![page]({base}/page.html) ![big]({base}/big.png) ![gone]({base}/missing.png)\n"
        ),
    );
    commit(
        &source,
        "docs/guide.md",
        &format!("# Guide\n\n<img src=\"{base}/diagram.svg\" alt=\"diagram\">\n"),
    );
    archive_source(&t, &source, false).await;
    let logo = sha256(PNG);
    let diagram = sha256(SVG.as_bytes());

    // The README shows the archived copies; refused images keep their URL.
    let (_, tree) = page(&t, "/repos/1/tree").await;
    assert!(
        tree.contains(&format!("src=\"/repos/1/asset/{logo}\" alt=\"logo\"")),
        "{tree}"
    );
    assert!(tree.contains(&format!("src=\"/repos/1/asset/{logo}\" alt=\"moved\"")));
    for refused in ["page.html", "big.png", "missing.png"] {
        assert!(
            tree.contains(&format!("src=\"{base}/{refused}\"")),
            "{refused}"
        );
    }

    // The Markdown file page renders too, and shows its source on request.
    let (_, guide) = page(&t, "/repos/1/blob?path=docs%2Fguide.md").await;
    assert!(
        guide.contains(&format!("src=\"/repos/1/asset/{diagram}\"")),
        "{guide}"
    );
    assert!(guide.contains("href=\"/repos/1/blob?path=docs%2Fguide.md&#38;view=source\""));
    let (_, source_view) = page(&t, "/repos/1/blob?path=docs%2Fguide.md&view=source").await;
    assert!(source_view.contains("id=\"L3\""), "{source_view}");
    assert!(!source_view.contains("/asset/"));

    // The copies are served in a sandbox, with their own type.
    let response = t.get(&format!("/repos/1/asset/{logo}")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
    assert_eq!(
        response.headers()[header::CONTENT_SECURITY_POLICY],
        "sandbox; default-src 'none'"
    );
    assert_eq!(
        response.headers()[header::X_CONTENT_TYPE_OPTIONS],
        "nosniff"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], PNG);
    let svg = t.get(&format!("/repos/1/asset/{diagram}")).await;
    assert_eq!(svg.headers()[header::CONTENT_TYPE], "image/svg+xml");
    for bad in ["0".repeat(64), "..%2F..%2Fdata.db".to_owned()] {
        let (status, _) = page(&t, &format!("/repos/1/asset/{bad}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{bad}");
    }

    let (_, repo) = page(&t, "/repos/1").await;
    assert!(repo.contains("3 resim arşivde"), "{repo}");
    assert!(!repo.contains("dış resim var"));
}

#[tokio::test]
async fn an_image_is_downloaded_again_only_after_a_day() {
    let server = image_server().await;
    let t = app().await;
    let source = source_repo(t.dir.path(), "source");
    commit(
        &source,
        "README.md",
        &format!("![logo]({}/logo.png)\n", server.uri()),
    );
    archive_source(&t, &source, false).await;
    sync_again(&t).await;
    assert_eq!(requests_to(&server, "/logo.png").await, 1);

    // A copy older than a day is downloaded again.
    let mut asset = assets::for_repo(&t.state.pool, 1).await.unwrap().remove(0);
    asset.fetched_at -= 2 * 86_400;
    assets::upsert(&t.state.pool, 1, &asset).await.unwrap();
    sync_again(&t).await;
    assert_eq!(requests_to(&server, "/logo.png").await, 2);
    let refreshed = assets::for_repo(&t.state.pool, 1).await.unwrap();
    assert!(refreshed[0].fetched_at > asset.fetched_at);

    // An image the files no longer show loses its record.
    commit(&source, "README.md", "No pictures now.\n");
    sync_again(&t).await;
    assert!(assets::for_repo(&t.state.pool, 1).await.unwrap().is_empty());
}

#[tokio::test]
async fn private_addresses_are_never_fetched() {
    let server = image_server().await;
    let port = server.address().port();
    let t = app().await;
    let source = source_repo(t.dir.path(), "source");
    commit(
        &source,
        "README.md",
        &format!(
            "![a](http://10.0.0.1/a.png) ![b](http://169.254.169.254/latest/meta-data)\n\
             ![c](http://[::1]:{port}/logo.png) ![d](http://localhost:{port}/logo.png)\n\
             ![e]({}/to-private.png)\n",
            server.uri()
        ),
    );
    let started = std::time::Instant::now();
    archive_source(&t, &source, false).await;
    // Refused before any connection, so nothing waits for a timeout.
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(assets::for_repo(&t.state.pool, 1).await.unwrap().is_empty());
    // The redirect itself was asked for once; its private target never.
    assert_eq!(requests_to(&server, "/to-private.png").await, 1);
    assert_eq!(requests_to(&server, "/logo.png").await, 0);
}

#[tokio::test]
async fn the_limit_per_repository_is_kept_and_shown() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/img/\d+\.png$"))
        .respond_with(image("image/png", PNG))
        .mount(&server)
        .await;
    let t = app().await;
    let source = source_repo(t.dir.path(), "source");
    let found = MAX_IMAGES + 5;
    let readme: String = (0..found)
        .map(|i| format!("![{i}]({}/img/{i}.png)\n", server.uri()))
        .collect();
    commit(&source, "README.md", &readme);
    archive_source(&t, &source, false).await;
    assert_eq!(
        assets::for_repo(&t.state.pool, 1).await.unwrap().len(),
        MAX_IMAGES
    );
    let (_, repo) = page(&t, "/repos/1").await;
    assert!(
        repo.contains(&format!(
            "Markdown dosyalarında {found} dış resim var. Yalnız ilk {MAX_IMAGES} resim arşive alınıyor."
        )),
        "{repo}"
    );
}

#[tokio::test]
async fn deleting_the_repository_removes_its_images() {
    let server = image_server().await;
    let t = app().await;
    let source = source_repo(t.dir.path(), "source");
    commit(
        &source,
        "README.md",
        &format!("![logo]({}/logo.png)\n", server.uri()),
    );
    archive_source(&t, &source, false).await;
    let file = t.state.config.assets_dir().join(sha256(PNG));
    assert!(file.is_file());
    // A file younger than an hour is kept, as a parallel sync may own it.
    let old = SystemTime::now() - Duration::from_secs(2 * 3600);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(old)
        .unwrap();

    let body = format!("_csrf={}&confirm={}", t.csrf, urlencode("team/project"));
    let response = t.post("/repos/1/delete", &body).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(!file.exists());
}
