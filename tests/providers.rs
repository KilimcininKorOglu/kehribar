//! Provider API clients against a mock server: paging, authentication,
//! forks and error answers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use git_archive::db::hosts::{Host, Provider};
use git_archive::db::sources::SourceKind;
use git_archive::git::Credentials;
use git_archive::providers::{ProviderError, RemoteRepo, list_repos};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn host(provider: Provider, api: String) -> Host {
    Host {
        id: 1,
        host: "git.example".into(),
        provider,
        api_base_url: Some(api),
        username: None,
        token_enc: None,
        created_at: 0,
        updated_at: 0,
        ref_exclude: None,
    }
}

fn creds(username: &str, token: &str) -> Credentials {
    Credentials {
        username: username.into(),
        password: token.into(),
    }
}

fn urls(repos: &[RemoteRepo]) -> Vec<&str> {
    repos.iter().map(|r| r.clone_url.as_str()).collect()
}

/// `count` items made by `item(n)` for numbers from `start`.
fn items(start: usize, count: usize, item: impl Fn(usize) -> Value) -> Value {
    Value::Array((start..start + count).map(item).collect())
}

async fn mount_page(server: &MockServer, route: &str, page: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(route))
        .and(query_param("page", page))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(server)
        .await;
}

fn github_repo(n: usize) -> Value {
    json!({ "clone_url": format!("https://git.example/org/r{n}.git"), "fork": n == 0 })
}

#[tokio::test]
async fn github_org_pages_until_a_short_page() {
    let server = MockServer::start().await;
    let route = "/orgs/octo/repos";
    // A full page of 100, then a short page of 2.
    mount_page(&server, route, "1", items(0, 100, github_repo)).await;
    mount_page(&server, route, "2", items(100, 2, github_repo)).await;
    let host = host(Provider::Github, server.uri());
    let repos = list_repos(&host, None, SourceKind::Org, "octo")
        .await
        .unwrap();
    assert_eq!(repos.len(), 102);
    assert!(repos[0].fork);
    assert!(!repos[101].fork);
    assert_eq!(repos[101].clone_url, "https://git.example/org/r101.git");
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|r| r.url.query().unwrap().contains("type=all"))
    );
    assert!(
        requests
            .iter()
            .all(|r| r.url.query().unwrap().contains("per_page=100"))
    );
}

#[tokio::test]
async fn github_lists_private_repositories_of_the_token_owner() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", "Bearer secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "Alice" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(query_param("affiliation", "owner"))
        .and(header("authorization", "Bearer secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(items(0, 1, github_repo)))
        .mount(&server)
        .await;
    let host = host(Provider::Github, server.uri());
    let token = creds("git", "secret");
    let repos = list_repos(&host, Some(&token), SourceKind::User, "alice")
        .await
        .unwrap();
    assert_eq!(urls(&repos), ["https://git.example/org/r0.git"]);
}

#[tokio::test]
async fn github_lists_another_user_through_the_public_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "alice" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/users/bob/repos"))
        .and(query_param("type", "owner"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    let host = host(Provider::Github, server.uri());
    let token = creds("git", "secret");
    let repos = list_repos(&host, Some(&token), SourceKind::User, "bob")
        .await
        .unwrap();
    assert!(repos.is_empty());
}

#[tokio::test]
async fn gitlab_groups_include_subgroups_and_escape_the_path() {
    let server = MockServer::start().await;
    let project = |n: usize| {
        let mut p = json!({ "http_url_to_repo": format!("https://git.example/grp/sub/p{n}.git") });
        if n == 1 {
            p["forked_from_project"] = json!({ "id": 9 });
        }
        p
    };
    Mock::given(method("GET"))
        .and(path("/api/v4/groups/grp%2Fsub/projects"))
        .and(query_param("include_subgroups", "true"))
        .and(query_param("page", "1"))
        .and(header("private-token", "glpat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(items(0, 2, project)))
        .expect(1)
        .mount(&server)
        .await;
    let host = host(Provider::Gitlab, format!("{}/api/v4", server.uri()));
    let token = creds("git", "glpat");
    let repos = list_repos(&host, Some(&token), SourceKind::Org, "grp/sub")
        .await
        .unwrap();
    assert_eq!(
        repos,
        [
            RemoteRepo {
                clone_url: "https://git.example/grp/sub/p0.git".into(),
                fork: false
            },
            RemoteRepo {
                clone_url: "https://git.example/grp/sub/p1.git".into(),
                fork: true
            },
        ]
    );
}

#[tokio::test]
async fn forgejo_pages_by_fifty_with_a_token_header() {
    let server = MockServer::start().await;
    let repo =
        |n: usize| json!({ "clone_url": format!("https://git.example/u/r{n}.git"), "fork": false });
    for (page, body) in [("1", items(0, 50, repo)), ("2", json!([]))] {
        Mock::given(method("GET"))
            .and(path("/api/v1/users/u/repos"))
            .and(query_param("limit", "50"))
            .and(query_param("page", page))
            .and(header("authorization", "token fj"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
    }
    let host = host(Provider::Forgejo, format!("{}/api/v1", server.uri()));
    let token = creds("git", "fj");
    let repos = list_repos(&host, Some(&token), SourceKind::User, "u")
        .await
        .unwrap();
    assert_eq!(repos.len(), 50);
}

fn bitbucket_page(server: &str, names: &[&str], next: Option<&str>) -> Value {
    let values: Vec<Value> = names
        .iter()
        .map(|name| {
            json!({
                "links": { "clone": [
                    { "name": "https", "href": format!("https://alice@git.example/team/{name}.git") },
                    { "name": "ssh", "href": format!("git@git.example:team/{name}.git") }
                ]},
                "parent": if *name == "fork" { json!({ "full_name": "x/y" }) } else { Value::Null }
            })
        })
        .collect();
    let mut page = json!({ "values": values });
    if let Some(next) = next {
        page["next"] = json!(format!("{server}{next}"));
    }
    page
}

#[tokio::test]
async fn bitbucket_follows_next_links_with_basic_auth() {
    let server = MockServer::start().await;
    let uri = server.uri();
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/team"))
        .and(query_param("pagelen", "100"))
        .and(header("authorization", "Basic YWxpY2U6YXBw"))
        .respond_with(ResponseTemplate::new(200).set_body_json(bitbucket_page(
            &uri,
            &["one", "fork"],
            Some("/2.0/repositories/team?page=2"),
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/team"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(bitbucket_page(
            &uri,
            &["two"],
            None,
        )))
        .expect(1)
        .mount(&server)
        .await;
    let host = host(Provider::Bitbucket, format!("{uri}/2.0"));
    let token = creds("alice", "app");
    let repos = list_repos(&host, Some(&token), SourceKind::Org, "team")
        .await
        .unwrap();
    assert_eq!(
        urls(&repos),
        [
            "https://git.example/team/one.git",
            "https://git.example/team/fork.git",
            "https://git.example/team/two.git"
        ]
    );
    assert_eq!(repos.iter().filter(|r| r.fork).count(), 1);
}

#[tokio::test]
async fn bitbucket_refuses_a_next_link_on_another_server() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/team"))
        .respond_with(ResponseTemplate::new(200).set_body_json(bitbucket_page(
            "https://evil.example",
            &["one"],
            Some("/steal"),
        )))
        .mount(&server)
        .await;
    let host = host(Provider::Bitbucket, format!("{}/2.0", server.uri()));
    let token = creds("alice", "app");
    let err = list_repos(&host, Some(&token), SourceKind::Org, "team")
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::ForeignPage(_)), "{err}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn error_answers_and_bad_bodies_are_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/orgs/missing/repos"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/orgs/broken/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
        .mount(&server)
        .await;
    let host = host(Provider::Github, server.uri());
    let err = list_repos(&host, None, SourceKind::Org, "missing")
        .await
        .unwrap_err();
    match &err {
        ProviderError::Status { status, url } => {
            assert_eq!(*status, 404);
            assert!(url.ends_with("/orgs/missing/repos"), "{url}");
        }
        other => panic!("unexpected {other}"),
    }
    let err = list_repos(&host, None, SourceKind::Org, "broken")
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Request(_)), "{err}");

    let generic = Host {
        api_base_url: None,
        ..host_of(Provider::Generic)
    };
    assert!(matches!(
        list_repos(&generic, None, SourceKind::Org, "x").await,
        Err(ProviderError::Unsupported(_))
    ));
}

fn host_of(provider: Provider) -> Host {
    host(provider, String::new())
}
