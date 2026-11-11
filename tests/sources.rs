//! Watched sources: the scan adds new repositories, honours the fork
//! option, keeps repositories that vanish, and the sources page drives it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use axum::http::{StatusCode, header};
use common::{TestApp, body_text, header_value};
use git_archive::db::hosts::{self, HostInput, Provider};
use git_archive::db::repos;
use git_archive::db::sources::{self, NewSource, Source, SourceKind, SourceOptions};
use git_archive::db::unix_now;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A GitHub host whose API is the mock server.
async fn github_host(t: &TestApp, server: &MockServer) -> i64 {
    let input = HostInput {
        host: "github.com",
        provider: Provider::Github,
        api_base_url: Some(&server.uri()),
        username: None,
        token_enc: None,
    };
    hosts::insert(&t.state.pool, &input, unix_now())
        .await
        .unwrap()
}

async fn list_answers(server: &MockServer, body: Value) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/orgs/octo/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

fn repo(url: &str, fork: bool) -> Value {
    json!({ "clone_url": url, "fork": fork })
}

fn options(include_forks: bool) -> SourceOptions {
    SourceOptions {
        auto_add: true,
        include_forks,
        ..SourceOptions::default()
    }
}

async fn add_source(t: &TestApp, host_id: i64, include_forks: bool) -> Source {
    add_source_with(t, host_id, options(include_forks)).await
}

async fn add_source_with(t: &TestApp, host_id: i64, options: SourceOptions) -> Source {
    let new = NewSource {
        host_id,
        kind: SourceKind::Org,
        name: "octo",
        options,
    };
    let id = sources::insert(&t.state.pool, &new, unix_now())
        .await
        .unwrap()
        .unwrap();
    sources::get(&t.state.pool, id).await.unwrap().unwrap()
}

async fn scan(t: &TestApp, id: i64) -> Source {
    let source = sources::get(&t.state.pool, id).await.unwrap().unwrap();
    t.state.scheduler.scan_now(source).await.unwrap();
    sources::get(&t.state.pool, id).await.unwrap().unwrap()
}

async fn archived_urls(t: &TestApp) -> Vec<String> {
    let mut urls: Vec<String> = repos::list(&t.state.pool, None)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.url)
        .collect();
    urls.sort();
    urls
}

#[tokio::test]
async fn scan_adds_new_repositories_and_never_removes_any() {
    let t = TestApp::new().await;
    let server = MockServer::start().await;
    let host_id = github_host(&t, &server).await;
    let source = add_source(&t, host_id, false).await;

    list_answers(
        &server,
        json!([
            repo("https://github.com/octo/app.git", false),
            repo("https://github.com/octo/fork.git", true)
        ]),
    )
    .await;
    let scanned = scan(&t, source.id).await;
    assert_eq!(scanned.last_error, None);
    assert!(scanned.last_scan_at.is_some());
    // The fork is left out while the option is off.
    assert_eq!(archived_urls(&t).await, ["https://github.com/octo/app.git"]);
    let added = &repos::list(&t.state.pool, None).await.unwrap()[0];
    assert_eq!(added.source_id, Some(source.id));
    assert!(added.wiki_enabled);

    sources::update_options(&t.state.pool, source.id, &options(true))
        .await
        .unwrap();
    scan(&t, source.id).await;
    assert_eq!(
        archived_urls(&t).await,
        [
            "https://github.com/octo/app.git",
            "https://github.com/octo/fork.git"
        ]
    );

    // Both repositories vanish from the provider; the archive keeps them.
    list_answers(
        &server,
        json!([repo("https://github.com/octo/new.git", false)]),
    )
    .await;
    scan(&t, source.id).await;
    assert_eq!(archived_urls(&t).await.len(), 3);
}

#[tokio::test]
async fn name_and_archived_filters_decide_what_a_scan_adds() {
    let t = TestApp::new().await;
    let server = MockServer::start().await;
    let host_id = github_host(&t, &server).await;
    let source = add_source_with(
        &t,
        host_id,
        SourceOptions {
            auto_add: true,
            include_forks: false,
            skip_archived: true,
            include_patterns: Some("api-*\ntool".into()),
            exclude_patterns: Some("*-old".into()),
        },
    )
    .await;
    let archived = json!({ "clone_url": "https://github.com/octo/api-frozen.git", "fork": false, "archived": true });
    list_answers(
        &server,
        json!([
            repo("https://github.com/octo/API-core.git", false),
            repo("https://github.com/octo/api-old.git", false),
            repo("https://github.com/octo/tool.git", false),
            repo("https://github.com/octo/docs.git", false),
            archived
        ]),
    )
    .await;
    let report = t.state.scheduler.context().clone();
    let scanned = git_archive::sync::source_scan::scan(&report, &source)
        .await
        .unwrap();
    assert_eq!((scanned.listed, scanned.added, scanned.filtered), (5, 2, 3));
    assert_eq!(scanned.error, None);
    assert_eq!(
        archived_urls(&t).await,
        [
            "https://github.com/octo/API-core.git",
            "https://github.com/octo/tool.git"
        ]
    );

    // Filters only decide new imports: cleared filters add the rest, and a
    // later, stricter filter removes nothing.
    sources::update_options(&t.state.pool, source.id, &options(false))
        .await
        .unwrap();
    scan(&t, source.id).await;
    assert_eq!(archived_urls(&t).await.len(), 5);
    let strict = SourceOptions {
        exclude_patterns: Some("*".into()),
        ..options(false)
    };
    sources::update_options(&t.state.pool, source.id, &strict)
        .await
        .unwrap();
    scan(&t, source.id).await;
    assert_eq!(archived_urls(&t).await.len(), 5);
}

#[tokio::test]
async fn the_sources_page_saves_filters_and_rejects_bad_patterns() {
    let t = TestApp::new().await;
    let server = MockServer::start().await;
    let host_id = github_host(&t, &server).await;
    list_answers(&server, json!([])).await;
    let form = format!(
        "host_id={host_id}&kind=org&name=octo&auto_add=1&skip_archived=1\
         &include_patterns=api-*%0D%0A%0D%0Aapi-*&exclude_patterns=*-old"
    );
    let response = t.post("/sources", &form).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let source = wait_for_scan(&t).await;
    assert!(source.skip_archived);
    assert_eq!(source.include_patterns.as_deref(), Some("api-*"));
    assert_eq!(source.exclude_patterns.as_deref(), Some("*-old"));
    let page = body_text(t.get("/sources").await).await;
    assert!(
        page.contains("<details class=\"source-filter\" open>"),
        "{page}"
    );

    let bad = t
        .post(
            &format!("/sources/{}", source.id),
            "auto_add=1&include_patterns=a%2Fb",
        )
        .await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_text(bad).await.contains("Desenler yalnız harf"));
    let kept = sources::get(&t.state.pool, source.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.include_patterns.as_deref(), Some("api-*"));

    let cleared = t
        .post(&format!("/sources/{}", source.id), "auto_add=1")
        .await;
    assert_eq!(cleared.status(), StatusCode::SEE_OTHER);
    let cleared = sources::get(&t.state.pool, source.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!cleared.has_filter());

    let bad_new = t
        .post(
            "/sources",
            &format!("host_id={host_id}&kind=user&name=x&exclude_patterns=%3Cb%3E"),
        )
        .await;
    assert_eq!(bad_new.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(sources::list(&t.state.pool).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_gist_source_adds_gists_without_a_wiki() {
    let t = TestApp::new().await;
    let server = MockServer::start().await;
    let host_id = github_host(&t, &server).await;
    Mock::given(method("GET"))
        .and(path("/users/alice/gists"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!([{ "id": "aa5a315d61ae9438b18d", "owner": { "login": "alice" } }]),
        ))
        .mount(&server)
        .await;
    let form = format!("host_id={host_id}&kind=gists&name=alice&auto_add=1");
    assert_eq!(
        t.post("/sources", &form).await.status(),
        StatusCode::SEE_OTHER
    );
    let source = wait_for_scan(&t).await;
    assert_eq!(source.last_error, None);
    let gist = &repos::list(&t.state.pool, None).await.unwrap()[0];
    assert_eq!(gist.url, "https://gist.github.com/aa5a315d61ae9438b18d.git");
    assert_eq!(
        (gist.host.as_str(), gist.owner.as_str(), gist.name.as_str()),
        ("gist.github.com", "alice", "aa5a315d61ae9438b18d")
    );
    assert!(!gist.wiki_enabled);
    let page = body_text(t.get("/sources").await).await;
    assert!(page.contains("Gist&#39;leri"), "{page}");
}

#[tokio::test]
async fn repositories_on_another_host_are_reported_not_added() {
    let t = TestApp::new().await;
    let server = MockServer::start().await;
    let host_id = github_host(&t, &server).await;
    let source = add_source(&t, host_id, false).await;
    list_answers(
        &server,
        json!([
            repo("https://github.com/octo/app.git", false),
            repo("https://mirror.example/octo/other.git", false),
            repo("ssh://github.com/octo/ssh.git", false)
        ]),
    )
    .await;
    let scanned = scan(&t, source.id).await;
    assert_eq!(archived_urls(&t).await, ["https://github.com/octo/app.git"]);
    let error = scanned.last_error.unwrap();
    assert!(error.starts_with("2 repositories skipped"), "{error}");
    assert!(error.contains("https://mirror.example/octo/other.git"));
}

#[tokio::test]
async fn a_provider_error_is_stored_on_the_source() {
    let t = TestApp::new().await;
    let server = MockServer::start().await;
    let host_id = github_host(&t, &server).await;
    let source = add_source(&t, host_id, false).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    let scanned = scan(&t, source.id).await;
    let error = scanned.last_error.unwrap();
    assert!(error.contains("HTTP 403"), "{error}");
    assert!(archived_urls(&t).await.is_empty());
}

#[tokio::test]
async fn scheduled_scans_run_once_per_interval() {
    let t = TestApp::new().await;
    let server = MockServer::start().await;
    let host_id = github_host(&t, &server).await;
    add_source(&t, host_id, false).await;
    list_answers(
        &server,
        json!([repo("https://github.com/octo/app.git", false)]),
    )
    .await;

    let handle = t.state.scheduler.scan_sources().await.unwrap();
    handle
        .expect("a never scanned source is due")
        .await
        .unwrap();
    assert_eq!(archived_urls(&t).await.len(), 1);
    // Scanned a moment ago: not due again before the global interval.
    assert!(t.state.scheduler.scan_sources().await.unwrap().is_none());
}

/// Waits for the background scan the page started.
async fn wait_for_scan(t: &TestApp) -> Source {
    for _ in 0..100 {
        let all = sources::list(&t.state.pool).await.unwrap();
        if let Some(source) = all.into_iter().find(|s| s.last_scan_at.is_some()) {
            return source;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the scan did not finish");
}

#[tokio::test]
async fn the_sources_page_imports_an_organization() {
    let t = TestApp::new().await;
    let server = MockServer::start().await;
    let host_id = github_host(&t, &server).await;
    list_answers(
        &server,
        json!([repo("https://github.com/octo/app.git", false)]),
    )
    .await;

    let page = body_text(t.get("/sources").await).await;
    assert!(
        page.contains(&format!("<option value=\"{host_id}\"")),
        "{page}"
    );

    let form = format!("host_id={host_id}&kind=org&name=octo&auto_add=1");
    let response = t.post("/sources", &form).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        header_value(&response, header::LOCATION),
        "/sources?notice=added"
    );
    let source = wait_for_scan(&t).await;
    assert!(source.auto_add && !source.include_forks);
    assert_eq!(archived_urls(&t).await, ["https://github.com/octo/app.git"]);

    let page = body_text(t.get("/sources?notice=added").await).await;
    assert!(page.contains("Kaynak eklendi, ilk tarama başladı."));
    assert!(page.contains("<strong>octo</strong>"));

    // The same source twice is refused.
    let again = t.post("/sources", &form).await;
    assert_eq!(again.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_text(again).await.contains("Bu kaynak zaten ekli."));

    // Options and deletion; the repository stays.
    let saved = t
        .post(&format!("/sources/{}", source.id), "include_forks=1")
        .await;
    assert_eq!(saved.status(), StatusCode::SEE_OTHER);
    let stored = sources::get(&t.state.pool, source.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!stored.auto_add && stored.include_forks);
    let deleted = t.post(&format!("/sources/{}/delete", source.id), "").await;
    assert_eq!(deleted.status(), StatusCode::SEE_OTHER);
    assert!(sources::list(&t.state.pool).await.unwrap().is_empty());
    assert_eq!(archived_urls(&t).await.len(), 1);
}

#[tokio::test]
async fn invalid_source_forms_are_rejected() {
    let t = TestApp::new().await;
    let server = MockServer::start().await;
    let github = github_host(&t, &server).await;
    let generic = hosts::insert(
        &t.state.pool,
        &HostInput {
            host: "git.example",
            provider: Provider::Generic,
            api_base_url: None,
            username: None,
            token_enc: None,
        },
        unix_now(),
    )
    .await
    .unwrap();
    let bitbucket = hosts::insert(
        &t.state.pool,
        &HostInput {
            host: "bitbucket.org",
            provider: Provider::Bitbucket,
            api_base_url: None,
            username: None,
            token_enc: None,
        },
        unix_now(),
    )
    .await
    .unwrap();
    let cases = [
        (
            format!("host_id={generic}&kind=org&name=octo"),
            "Listeden bir sunucu seçin.",
        ),
        (
            "host_id=999&kind=org&name=octo".to_owned(),
            "Listeden bir sunucu seçin.",
        ),
        (
            format!("host_id={github}&kind=team&name=octo"),
            "Listeden bir tür seçin.",
        ),
        (
            format!("host_id={bitbucket}&kind=starred&name=alice"),
            "Yıldızlananlar yalnız GitHub, GitLab ve Forgejo",
        ),
        (
            format!("host_id={github}&kind=org&name=a%2Fb"),
            "yalnız GitLab gruplarında kullanılabilir",
        ),
        (
            format!("host_id={github}&kind=org&name=%3Cscript%3E"),
            "Ad yalnız harf, rakam",
        ),
    ];
    for (form, message) in cases {
        let response = t.post("/sources", &form).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{form}"
        );
        let page = body_text(response).await;
        assert!(page.contains(message), "{form}: {page}");
        assert!(!page.contains("<script>"));
    }
    assert!(sources::list(&t.state.pool).await.unwrap().is_empty());
    // The generic host is not offered.
    let page = body_text(t.get("/sources").await).await;
    assert!(!page.contains(&format!("<option value=\"{generic}\"")));
}
