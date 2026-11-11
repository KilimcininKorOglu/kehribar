//! Provider API clients against a mock server: paging, authentication,
//! forks and error answers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use git_archive::db::hosts::{Host, Provider};
use git_archive::db::sources::SourceKind;
use git_archive::git::Credentials;
use git_archive::providers::{ProviderError, RemoteRepo, list_repos};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
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
        max_concurrency: None,
        api_blocked_until: None,
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
async fn starred_lists_page_on_every_provider_that_has_them() {
    let server = MockServer::start().await;
    // GitHub, anonymous: the public list, paged by 100.
    mount_page(
        &server,
        "/users/alice/starred",
        "1",
        items(0, 100, github_repo),
    )
    .await;
    mount_page(
        &server,
        "/users/alice/starred",
        "2",
        items(100, 1, github_repo),
    )
    .await;
    let github = host(Provider::Github, server.uri());
    let repos = list_repos(&github, None, SourceKind::Starred, "alice")
        .await
        .unwrap();
    assert_eq!(repos.len(), 101);

    // GitLab: the user name works in place of the ID.
    let project =
        |n: usize| json!({ "http_url_to_repo": format!("https://git.example/x/p{n}.git") });
    mount_page(
        &server,
        "/api/v4/users/alice/starred_projects",
        "1",
        items(0, 2, project),
    )
    .await;
    let gitlab = host(Provider::Gitlab, format!("{}/api/v4", server.uri()));
    let repos = list_repos(&gitlab, None, SourceKind::Starred, "alice")
        .await
        .unwrap();
    assert_eq!(
        urls(&repos),
        [
            "https://git.example/x/p0.git",
            "https://git.example/x/p1.git"
        ]
    );

    // Forgejo / Gitea.
    mount_page(
        &server,
        "/api/v1/users/alice/starred",
        "1",
        items(0, 1, github_repo),
    )
    .await;
    let forgejo = host(Provider::Forgejo, format!("{}/api/v1", server.uri()));
    let repos = list_repos(&forgejo, None, SourceKind::Starred, "alice")
        .await
        .unwrap();
    assert_eq!(urls(&repos), ["https://git.example/org/r0.git"]);

    // Bitbucket has no such list; nothing is requested.
    let before = server.received_requests().await.unwrap().len();
    let bitbucket = host(Provider::Bitbucket, server.uri());
    let err = list_repos(&bitbucket, None, SourceKind::Starred, "alice")
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "the bitbucket provider has no starred list"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn github_gists_name_their_owner_and_need_github_com() {
    let server = MockServer::start().await;
    let gist = |n: usize| {
        if n == 2 {
            // An anonymous gist has no owner and no archive path.
            json!({ "id": "anon", "owner": null })
        } else {
            json!({ "id": format!("g{n}"), "owner": { "login": "alice" } })
        }
    };
    mount_page(&server, "/users/alice/gists", "1", items(0, 3, gist)).await;
    let mut github = host(Provider::Github, server.uri());
    github.host = "github.com".into();
    let repos = list_repos(&github, None, SourceKind::Gists, "alice")
        .await
        .unwrap();
    assert_eq!(
        urls(&repos),
        [
            "https://gist.github.com/alice/g0",
            "https://gist.github.com/alice/g1"
        ]
    );

    // The token owner's list has the secret gists too.
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "alice" })))
        .mount(&server)
        .await;
    mount_page(&server, "/gists", "1", items(0, 1, gist)).await;
    let token = creds("git", "secret");
    let own = list_repos(&github, Some(&token), SourceKind::Gists, "alice")
        .await
        .unwrap();
    assert_eq!(urls(&own), ["https://gist.github.com/alice/g0"]);

    // GitHub Enterprise has no gist list here.
    let enterprise = host(Provider::Github, server.uri());
    let err = list_repos(&enterprise, None, SourceKind::Gists, "alice")
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "the github provider has no gists list");
}

#[tokio::test]
async fn github_lists_the_token_owners_private_stars() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "alice" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user/starred"))
        .and(header("authorization", "Bearer secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(items(0, 1, github_repo)))
        .expect(1)
        .mount(&server)
        .await;
    let host = host(Provider::Github, server.uri());
    let token = creds("git", "secret");
    let repos = list_repos(&host, Some(&token), SourceKind::Starred, "Alice")
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
            p["archived"] = json!(true);
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
                fork: false,
                archived: false,
            },
            RemoteRepo {
                clone_url: "https://git.example/grp/sub/p1.git".into(),
                fork: true,
                archived: true,
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

fn gitee_repo(n: usize) -> Value {
    json!({ "html_url": format!("https://gitee.example/org/r{n}.git"), "fork": n == 1, "status": "开始" })
}

#[tokio::test]
async fn gitee_pages_by_a_hundred_with_the_token_in_the_query() {
    let server = MockServer::start().await;
    for (page, body) in [
        ("1", items(0, 100, gitee_repo)),
        ("2", items(100, 1, gitee_repo)),
    ] {
        Mock::given(method("GET"))
            .and(path("/api/v5/orgs/team/repos"))
            .and(query_param("per_page", "100"))
            .and(query_param("page", page))
            .and(query_param("access_token", "gt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
    }
    let host = host(Provider::Gitee, format!("{}/api/v5", server.uri()));
    let token = creds("git", "gt");
    let repos = list_repos(&host, Some(&token), SourceKind::Org, "team")
        .await
        .unwrap();
    assert_eq!(repos.len(), 101);
    assert_eq!(repos[0].clone_url, "https://gitee.example/org/r0.git");
    assert!(repos[1].fork && !repos[0].fork);

    // A failed request names the URL without the token.
    let err = list_repos(&host, Some(&token), SourceKind::Org, "missing")
        .await
        .unwrap_err();
    assert!(
        matches!(err, ProviderError::Status { status: 404, .. }),
        "{err}"
    );
    assert!(!err.to_string().contains("gt"), "{err}");
}

#[tokio::test]
async fn gitee_lists_own_repositories_and_stars_of_others() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v5/user"))
        .and(query_param("access_token", "gt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "alice" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v5/user/repos"))
        .and(query_param("type", "owner"))
        .respond_with(ResponseTemplate::new(200).set_body_json(items(0, 1, gitee_repo)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v5/users/bob/starred"))
        .respond_with(ResponseTemplate::new(200).set_body_json(items(5, 1, gitee_repo)))
        .expect(1)
        .mount(&server)
        .await;
    let host = host(Provider::Gitee, format!("{}/api/v5", server.uri()));
    let token = creds("git", "gt");
    let own = list_repos(&host, Some(&token), SourceKind::User, "Alice")
        .await
        .unwrap();
    assert_eq!(urls(&own), ["https://gitee.example/org/r0.git"]);
    let stars = list_repos(&host, Some(&token), SourceKind::Starred, "bob")
        .await
        .unwrap();
    assert_eq!(urls(&stars), ["https://gitee.example/org/r5.git"]);
}

#[tokio::test]
async fn azure_lists_a_project_with_basic_auth_and_skips_disabled_repositories() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fabrikam/Fiber/_apis/git/repositories"))
        .and(query_param("api-version", "7.1"))
        // `git:pat`: Azure DevOps ignores the username.
        .and(header("authorization", "Basic Z2l0OnBhdA=="))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "count": 3,
            "value": [
                { "remoteUrl": "https://fabrikam@dev.example/fabrikam/Fiber/_git/App", "isFork": false },
                { "remoteUrl": "https://fabrikam@dev.example/fabrikam/Fiber/_git/Copy", "isFork": true },
                { "remoteUrl": "https://fabrikam@dev.example/fabrikam/Fiber/_git/Old", "isDisabled": true }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let host = host(Provider::Azure, server.uri());
    let token = creds("git", "pat");
    let repos = list_repos(&host, Some(&token), SourceKind::Org, "fabrikam/Fiber")
        .await
        .unwrap();
    assert_eq!(
        repos,
        [
            RemoteRepo {
                clone_url: "https://dev.example/fabrikam/Fiber/_git/App".into(),
                fork: false,
                archived: false,
            },
            RemoteRepo {
                clone_url: "https://dev.example/fabrikam/Fiber/_git/Copy".into(),
                fork: true,
                archived: false,
            },
        ]
    );
    let err = list_repos(&host, Some(&token), SourceKind::User, "fabrikam")
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "the azure provider has no user list");
}

fn sourcehut_page(names: &[&str], cursor: Option<&str>) -> Value {
    let results: Vec<Value> = names.iter().map(|name| json!({ "name": name })).collect();
    json!({ "data": { "user": {
        "canonicalName": "~alice",
        "repositories": { "results": results, "cursor": cursor }
    }}})
}

#[tokio::test]
async fn sourcehut_follows_the_cursor_with_a_bearer_token() {
    let server = MockServer::start().await;
    for (cursor, body) in [
        (Value::Null, sourcehut_page(&["one", "two"], Some("c1"))),
        (json!("c1"), sourcehut_page(&["three"], None)),
    ] {
        Mock::given(method("POST"))
            .and(path("/query"))
            .and(header("authorization", "Bearer srht"))
            .and(body_partial_json(
                json!({ "variables": { "username": "alice", "cursor": cursor } }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
    }
    let host = host(Provider::Sourcehut, format!("{}/query", server.uri()));
    let token = creds("git", "srht");
    let repos = list_repos(&host, Some(&token), SourceKind::User, "~alice")
        .await
        .unwrap();
    let base = server.uri();
    assert_eq!(
        urls(&repos),
        [
            format!("{base}/~alice/one"),
            format!("{base}/~alice/two"),
            format!("{base}/~alice/three")
        ]
    );
    let err = list_repos(&host, Some(&token), SourceKind::Org, "team")
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "the sourcehut provider has no org list");
}

#[tokio::test]
async fn sourcehut_errors_and_unknown_users_are_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/query"))
        .and(body_partial_json(
            json!({ "variables": { "username": "ghost" } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": { "user": null } })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/query"))
        .and(body_partial_json(
            json!({ "variables": { "username": "alice" } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": null,
            "errors": [{ "message": "Authentication error" }]
        })))
        .mount(&server)
        .await;
    let host = host(Provider::Sourcehut, format!("{}/query", server.uri()));
    let err = list_repos(&host, None, SourceKind::User, "ghost")
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "the API answered with an error: no user named ghost"
    );
    let err = list_repos(&host, None, SourceKind::User, "alice")
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "the API answered with an error: Authentication error"
    );
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
