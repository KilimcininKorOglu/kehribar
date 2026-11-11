//! The REST API: Bearer token checks and the read endpoints.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{TestApp, add_repo, auth_request, body_text, create_token, header_value, send};
use git_archive::db::hosts::{self, HostInput, Provider};
use git_archive::db::sources::{self, NewSource, SourceKind, SourceOptions};
use git_archive::db::{repos, sync_runs, unix_now};
use git_archive::web::auth::MAX_FAILED_LOGINS;
use serde_json::Value;

/// A request to the API with `token` as the Bearer token.
fn api_request(method: &str, path: &str, token: Option<&str>) -> Request<Body> {
    let auth = token.map(|token| format!("Bearer {token}"));
    auth_request(method, &format!("/api/v1{path}"), auth.as_deref())
}

/// The status and the JSON body of an API answer.
async fn call(t: &TestApp, method: &str, path: &str, token: Option<&str>) -> (StatusCode, Value) {
    let response = send(&t.app, api_request(method, path, token)).await;
    let status = response.status();
    let text = body_text(response).await;
    let json = serde_json::from_str(&text).unwrap_or_else(|_| panic!("JSON: {text}"));
    (status, json)
}

async fn get(t: &TestApp, path: &str, token: &str) -> Value {
    let (status, json) = call(t, "GET", path, Some(token)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    json
}

async fn read_token(t: &TestApp) -> String {
    create_token(t, "kind=api&name=reader&permission=read").await
}

#[tokio::test]
async fn requests_need_a_valid_api_token() {
    let t = TestApp::new().await;
    let reader = read_token(&t).await;

    let response = send(&t.app, api_request("GET", "/repos", None)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(header_value(&response, header::WWW_AUTHENTICATE), "Bearer");
    let (status, json) = call(&t, "GET", "/repos", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["error"]["code"], "unauthorized");

    // A clone token and a signed-in session are not API credentials.
    let clone = create_token(&t, "kind=clone&name=c&scope=all").await;
    assert_eq!(
        call(&t, "GET", "/repos", Some(&clone)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        t.get("/api/v1/repos").await.status(),
        StatusCode::UNAUTHORIZED
    );

    assert_eq!(get(&t, "/repos", &reader).await["total"], 0);
    let (status, json) = call(&t, "GET", "/nothing", Some(&reader)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"]["code"], "no_route");

    // A read-only token may not change anything.
    let (status, json) = call(&t, "POST", "/repos", Some(&reader)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["error"]["code"], "read_only");
}

#[tokio::test]
async fn revoked_and_expired_tokens_stop_working() {
    let t = TestApp::new().await;
    let expiring = read_token(&t).await;
    let revoked = create_token(&t, "kind=api&name=old&permission=write").await;
    get(&t, "/repos", &expiring).await;
    get(&t, "/repos", &revoked).await;

    sqlx::query("UPDATE access_tokens SET expires_at = ? WHERE name = 'reader'")
        .bind(unix_now() - 1)
        .execute(&t.state.pool)
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM access_tokens WHERE name = 'old'")
        .fetch_one(&t.state.pool)
        .await
        .unwrap();
    let response = t.post(&format!("/settings/tokens/{id}/revoke"), "").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    for token in [&expiring, &revoked] {
        assert_eq!(
            call(&t, "GET", "/repos", Some(token)).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    // A used token records its last use.
    let used: Option<i64> =
        sqlx::query_scalar("SELECT last_used_at FROM access_tokens WHERE name = 'reader'")
            .fetch_one(&t.state.pool)
            .await
            .unwrap();
    assert!(used.is_some());
}

#[tokio::test]
async fn guessing_tokens_locks_the_caller_out() {
    let t = TestApp::new().await;
    let reader = read_token(&t).await;
    for guess in 0..MAX_FAILED_LOGINS {
        let wrong = format!("gaa_{guess:064}");
        assert_eq!(
            call(&t, "GET", "/repos", Some(&wrong)).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (status, json) = call(&t, "GET", "/repos", Some(&reader)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(json["error"]["code"], "locked");
}

/// The names of the repositories of a list answer.
fn names(json: &Value) -> Vec<&str> {
    json["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| repo["name"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn the_repository_list_filters_sorts_and_pages() {
    let t = TestApp::new().await;
    let reader = read_token(&t).await;
    for name in ["alpha", "beta", "gamma", "delta"] {
        add_repo(&t, name).await;
    }
    sqlx::query("UPDATE repos SET status = 'error' WHERE name IN ('beta', 'delta')")
        .execute(&t.state.pool)
        .await
        .unwrap();

    let all = get(&t, "/repos", &reader).await;
    assert_eq!(names(&all), ["alpha", "beta", "delta", "gamma"]);
    assert_eq!(
        (all["total"].as_i64(), all["per_page"].as_i64()),
        (Some(4), Some(50))
    );

    let failed = get(&t, "/repos?status=error&dir=desc", &reader).await;
    assert_eq!(names(&failed), ["delta", "beta"]);
    assert_eq!(failed["total"], 2);

    let second = get(&t, "/repos?per_page=3&page=2", &reader).await;
    assert_eq!(names(&second), ["gamma"]);
    assert_eq!(
        (second["total"].as_i64(), second["page"].as_i64()),
        (Some(4), Some(2))
    );

    let found = get(&t, "/repos?q=amm", &reader).await;
    assert_eq!(names(&found), ["gamma"]);
    assert_eq!(found["total"], 1);

    for query in [
        "status=lost",
        "sort=age",
        "dir=up",
        "page=0",
        "per_page=101",
        "page=x",
    ] {
        let (status, json) = call(&t, "GET", &format!("/repos?{query}"), Some(&reader)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{query}");
        assert_eq!(json["error"]["code"], "invalid_query");
    }
}

#[tokio::test]
async fn one_repository_and_its_runs_without_secrets() {
    let t = TestApp::new().await;
    let reader = read_token(&t).await;
    let id = add_repo(&t, "alpha").await;
    let secret = t.state.cipher.encrypt("repo-secret").unwrap();
    sqlx::query("UPDATE repos SET username = 'bot', token_enc = ? WHERE id = ?")
        .bind(&secret)
        .bind(id)
        .execute(&t.state.pool)
        .await
        .unwrap();
    let run = sync_runs::start(&t.state.pool, id, 100).await.unwrap();

    let repo = get(&t, &format!("/repos/{id}"), &reader).await;
    assert_eq!(repo["url"], "https://example.com/team/alpha");
    assert_eq!(repo["status"], "pending");
    assert_eq!(
        (repo["username"].as_str(), repo["has_token"].as_bool()),
        (Some("bot"), Some(true))
    );
    assert!(repo.get("token_enc").is_none(), "{repo}");

    let runs = get(&t, &format!("/repos/{id}/runs"), &reader).await;
    assert_eq!(runs[0]["id"], run);
    assert_eq!(runs[0]["outcome"], Value::Null);

    for path in [
        "/repos/99",
        "/repos/99/runs",
        "/repos/abc",
        "/hosts/99",
        "/sources/99",
    ] {
        let (status, json) = call(&t, "GET", path, Some(&reader)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(json["error"]["code"], "not_found");
    }
    assert!(repos::get(&t.state.pool, id).await.unwrap().is_some());
}

#[tokio::test]
async fn hosts_and_sources_without_secrets() {
    let t = TestApp::new().await;
    let reader = read_token(&t).await;
    let token_enc = t.state.cipher.encrypt("host-secret").unwrap();
    let input = HostInput {
        host: "git.example.com",
        provider: Provider::Forgejo,
        api_base_url: None,
        username: Some("bot"),
        token_enc: Some(&token_enc),
    };
    let host_id = hosts::insert(&t.state.pool, &input, 100).await.unwrap();
    let key_enc = t.state.cipher.encrypt("PRIVATE KEY").unwrap();
    sqlx::query("UPDATE hosts SET ssh_key_enc = ?, ssh_public_key = 'ssh-ed25519 AAAA'")
        .bind(&key_enc)
        .execute(&t.state.pool)
        .await
        .unwrap();
    let source = NewSource {
        host_id,
        kind: SourceKind::Org,
        name: "team",
        options: SourceOptions {
            include_patterns: Some("app-*\nlib-*".to_owned()),
            ..SourceOptions::default()
        },
    };
    let source_id = sources::insert(&t.state.pool, &source, 100)
        .await
        .unwrap()
        .unwrap();

    let list = get(&t, "/hosts", &reader).await;
    let host = get(&t, &format!("/hosts/{host_id}"), &reader).await;
    assert_eq!(list[0], host);
    assert_eq!(
        (host["provider"].as_str(), host["has_token"].as_bool()),
        (Some("forgejo"), Some(true))
    );
    assert_eq!(host["has_ssh_key"], true);
    assert_eq!(host["ssh_public_key"], "ssh-ed25519 AAAA");
    let text = host.to_string();
    assert!(
        !text.contains("token_enc") && !text.contains("ssh_key_enc"),
        "{text}"
    );

    let list = get(&t, "/sources", &reader).await;
    let source = get(&t, &format!("/sources/{source_id}"), &reader).await;
    assert_eq!(list[0], source);
    assert_eq!(
        (source["kind"].as_str(), source["host_id"].as_i64()),
        (Some("org"), Some(host_id))
    );
    assert_eq!(
        source["include_patterns"],
        serde_json::json!(["app-*", "lib-*"])
    );
}
