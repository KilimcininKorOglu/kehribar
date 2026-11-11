//! The REST API: adding, changing, deleting and syncing repositories.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{TestApp, add_repo, api_call, create_token, header_value, send};
use git_archive::db::{repos, unix_now};
use serde_json::{Value, json};

/// An app and an API token that may write.
async fn writer() -> (TestApp, String) {
    let t = TestApp::new().await;
    let token = create_token(&t, "kind=api&name=writer&permission=write").await;
    (t, token)
}

async fn call(
    t: &TestApp,
    token: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    api_call(t, method, path, Some(token), Some(&body)).await
}

/// Asserts the status and the error code of a refused request.
async fn refused(
    t: &TestApp,
    token: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, String) {
    let (status, json) = call(t, token, method, path, body).await;
    let message = json["error"]["message"].as_str().unwrap_or_default();
    assert!(!message.is_empty(), "{json}");
    (status, json["error"]["code"].as_str().unwrap().to_owned())
}

#[tokio::test]
async fn adding_a_repository_checks_it_like_the_form() {
    let (t, token) = writer().await;
    let body = json!({
        "url": "https://github.com/octocat/Hello-World",
        "interval_minutes": 120,
        "lfs_enabled": true,
        "ref_exclude": ["refs/pull/*"],
        "username": "bot",
        "token": "ghp_secret",
    });
    let request = common::api_request("POST", "/repos", Some(&token), Some(&body));
    let response = send(&t.app, request).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let location = header_value(&response, header::LOCATION);
    let repo: Value = serde_json::from_str(&common::body_text(response).await).unwrap();
    assert_eq!(location, format!("/api/v1/repos/{}", repo["id"]));
    assert_eq!(repo["status"], "pending");
    assert_eq!(
        (
            repo["interval_minutes"].as_i64(),
            repo["wiki_enabled"].as_bool()
        ),
        (Some(120), Some(true))
    );
    assert_eq!(repo["ref_exclude"], json!(["refs/pull/*"]));
    assert_eq!(
        (repo["lfs_enabled"].as_bool(), repo["has_token"].as_bool()),
        (Some(true), Some(true))
    );
    assert!(!repo.to_string().contains("ghp_secret"), "{repo}");
    let stored = repos::get(&t.state.pool, repo["id"].as_i64().unwrap())
        .await
        .unwrap()
        .unwrap();
    let token_enc = stored.token_enc.unwrap();
    assert_eq!(t.state.cipher.decrypt(&token_enc).unwrap(), "ghp_secret");
    assert!(stored.next_sync_at <= unix_now());

    let again = json!({ "url": "https://github.com/octocat/Hello-World.git" });
    assert_eq!(
        refused(&t, &token, "POST", "/repos", again).await,
        (StatusCode::CONFLICT, "repo_new.exists".to_owned())
    );
    let cases = [
        (
            json!({ "url": "https://github.com/octocat/Spoon-Knife", "interval_minutes": 0 }),
            "form.interval_invalid",
        ),
        (
            json!({ "url": "https://github.com/octocat/Spoon-Knife", "ref_exclude": ["pull/*"] }),
            "refs.invalid",
        ),
        (
            json!({ "url": "https://github.com/octocat/Spoon-Knife", "colour": "red" }),
            "invalid_body",
        ),
        (json!({ "interval_minutes": 60 }), "invalid_body"),
    ];
    for (body, code) in cases {
        let (status, found) = refused(&t, &token, "POST", "/repos", body).await;
        assert_eq!(
            (status, found.as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, code)
        );
    }
    let (status, code) = refused(&t, &token, "POST", "/repos", json!({ "url": "not a url" })).await;
    assert_eq!(
        (status, code.as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, "url.invalid")
    );
    assert_eq!(repos::count(&t.state.pool).await.unwrap(), 1);

    // A body that is not JSON.
    let request = Request::post("/api/v1/repos")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("url=x"))
        .unwrap();
    assert_eq!(
        send(&t.app, request).await.status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
}

#[tokio::test]
async fn a_patch_changes_only_the_fields_it_names() {
    let (t, token) = writer().await;
    let id = add_repo(&t, "alpha").await;
    let path = format!("/repos/{id}");
    let (status, repo) = call(
        &t,
        &token,
        "PATCH",
        &path,
        json!({ "interval_minutes": 90, "ref_exclude": ["refs/pull/*"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{repo}");
    assert_eq!(
        (
            repo["interval_minutes"].as_i64(),
            repo["wiki_enabled"].as_bool()
        ),
        (Some(90), Some(false))
    );

    let (_, repo) = call(&t, &token, "PATCH", &path, json!({ "lfs_enabled": true })).await;
    assert_eq!(repo["interval_minutes"], 90);
    assert_eq!(repo["ref_exclude"], json!(["refs/pull/*"]));
    assert_eq!(repo["lfs_enabled"], true);

    let (_, repo) = call(
        &t,
        &token,
        "PATCH",
        &path,
        json!({ "interval_minutes": null, "ref_exclude": [] }),
    )
    .await;
    assert_eq!(
        (
            repo["interval_minutes"].clone(),
            repo["ref_exclude"].clone()
        ),
        (Value::Null, json!([]))
    );

    let (status, code) = refused(
        &t,
        &token,
        "PATCH",
        &path,
        json!({ "ref_exclude": ["refs/git-archive/x"] }),
    )
    .await;
    assert_eq!(
        (status, code.as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, "refs.invalid")
    );
    // A settings change cannot carry a secret.
    let (status, code) = refused(&t, &token, "PATCH", &path, json!({ "token": "x" })).await;
    assert_eq!(
        (status, code.as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_body")
    );
    let (status, _) = refused(&t, &token, "PATCH", "/repos/99", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn credentials_are_written_but_never_read_back() {
    let (t, token) = writer().await;
    let id = add_repo(&t, "alpha").await;
    let path = format!("/repos/{id}/credentials");
    let stored = |t: &TestApp| {
        let pool = t.state.pool.clone();
        async move { repos::get(&pool, id).await.unwrap().unwrap() }
    };

    let (status, repo) = call(
        &t,
        &token,
        "PUT",
        &path,
        json!({ "username": "bot", "token": "glpat-1" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (repo["username"].as_str(), repo["has_token"].as_bool()),
        (Some("bot"), Some(true))
    );
    assert!(!repo.to_string().contains("glpat-1"));
    assert_eq!(
        t.state
            .cipher
            .decrypt(&stored(&t).await.token_enc.unwrap())
            .unwrap(),
        "glpat-1"
    );

    // Without a token field the stored token stays.
    let (_, repo) = call(&t, &token, "PUT", &path, json!({ "username": "other" })).await;
    assert_eq!(
        (repo["username"].as_str(), repo["has_token"].as_bool()),
        (Some("other"), Some(true))
    );

    let (_, repo) = call(&t, &token, "PUT", &path, json!({ "token": null })).await;
    assert_eq!(
        (repo["username"].clone(), repo["has_token"].clone()),
        (Value::Null, json!(false))
    );
    assert_eq!(stored(&t).await.token_enc, None);
}

#[tokio::test]
async fn deleting_refuses_a_syncing_repository() {
    let (t, token) = writer().await;
    let id = add_repo(&t, "alpha").await;
    let path = format!("/repos/{id}");
    sqlx::query("UPDATE repos SET status = 'syncing'")
        .execute(&t.state.pool)
        .await
        .unwrap();
    let (status, json) = api_call(&t, "DELETE", &path, Some(&token), None).await;
    assert_eq!(
        (status, json["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("repo.delete_busy"))
    );

    sqlx::query("UPDATE repos SET status = 'ok'")
        .execute(&t.state.pool)
        .await
        .unwrap();
    assert_eq!(
        api_call(&t, "DELETE", &path, Some(&token), None).await,
        (StatusCode::NO_CONTENT, Value::Null)
    );
    assert!(repos::get(&t.state.pool, id).await.unwrap().is_none());
    assert_eq!(
        api_call(&t, "DELETE", &path, Some(&token), None).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_sync_request_queues_the_repository() {
    let (t, token) = writer().await;
    let id = add_repo(&t, "alpha").await;
    let path = format!("/repos/{id}/sync");
    sqlx::query("UPDATE repos SET status = 'ok', next_sync_at = 9999999999")
        .execute(&t.state.pool)
        .await
        .unwrap();
    let answer = api_call(&t, "POST", &path, Some(&token), None).await;
    assert_eq!(answer, (StatusCode::ACCEPTED, json!({ "queued": true })));
    assert!(
        repos::get(&t.state.pool, id)
            .await
            .unwrap()
            .unwrap()
            .next_sync_at
            <= unix_now()
    );

    // During a sync the request asks for one more sync.
    sqlx::query("UPDATE repos SET status = 'syncing', next_sync_at = 9999999999")
        .execute(&t.state.pool)
        .await
        .unwrap();
    assert_eq!(
        api_call(&t, "POST", &path, Some(&token), None).await.0,
        StatusCode::ACCEPTED
    );
    let requested: bool = sqlx::query_scalar("SELECT sync_requested FROM repos")
        .fetch_one(&t.state.pool)
        .await
        .unwrap();
    assert!(requested);
    assert_eq!(
        api_call(&t, "POST", "/repos/99/sync", Some(&token), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    // A read-only token cannot ask for a sync.
    let reader = create_token(&t, "kind=api&name=reader&permission=read").await;
    assert_eq!(
        api_call(&t, "POST", &path, Some(&reader), None).await.0,
        StatusCode::FORBIDDEN
    );
}
