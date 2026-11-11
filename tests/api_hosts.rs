//! The REST API: managing hosts, their SSH keys and sources.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::process::Command;

use axum::http::StatusCode;
use common::{TestApp, api_call, create_token};
use kehribar::db::{hosts, sources};
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

/// The status and the error code of a refused request.
async fn refused(
    t: &TestApp,
    token: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, String) {
    let (status, json) = call(t, token, method, path, body).await;
    let code = json["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("{json}"));
    (status, code.to_owned())
}

/// A host whose API answers nothing, so a source scan fails at once.
fn offline_host() -> Value {
    json!({
        "host": "git.example.com",
        "provider": "gitlab",
        "api_base_url": "http://127.0.0.1:9/api/v4",
    })
}

#[tokio::test]
async fn hosts_are_added_changed_and_deleted() {
    let (t, token) = writer().await;
    let body = json!({
        "host": "git.example.com",
        "provider": "forgejo",
        "username": "bot",
        "token": "forgejo-secret",
        "ref_exclude": ["refs/pull/*"],
        "max_concurrency": 2,
    });
    let (status, host) = call(&t, &token, "POST", "/hosts", body).await;
    assert_eq!(status, StatusCode::CREATED, "{host}");
    assert_eq!(
        (host["provider"].as_str(), host["has_token"].as_bool()),
        (Some("forgejo"), Some(true))
    );
    assert_eq!(
        (host["ref_exclude"].clone(), host["max_concurrency"].clone()),
        (json!(["refs/pull/*"]), json!(2))
    );
    assert!(!host.to_string().contains("forgejo-secret"));
    let id = host["id"].as_i64().unwrap();
    let stored = hosts::get(&t.state.pool, id).await.unwrap().unwrap();
    assert_eq!(
        t.state.cipher.decrypt(&stored.token_enc.unwrap()).unwrap(),
        "forgejo-secret"
    );

    let again = json!({ "host": "GIT.example.com", "provider": "generic" });
    assert_eq!(
        refused(&t, &token, "POST", "/hosts", again).await,
        (StatusCode::CONFLICT, "hosts.exists".to_owned())
    );
    let cases = [
        (
            json!({ "host": "not a host", "provider": "generic" }),
            "hosts.invalid_host",
        ),
        (
            json!({ "host": "a.example.com", "provider": "svn" }),
            "hosts.invalid_provider",
        ),
        (
            json!({ "host": "a.example.com", "provider": "generic", "max_concurrency": 0 }),
            "hosts.invalid_concurrency",
        ),
        (
            json!({ "host": "a.example.com", "provider": "generic", "ref_exclude": ["x"] }),
            "refs.invalid",
        ),
    ];
    for (body, code) in cases {
        let (status, found) = refused(&t, &token, "POST", "/hosts", body).await;
        assert_eq!(
            (status, found.as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, code)
        );
    }

    // A patch keeps every field it does not name, the token included.
    let path = format!("/hosts/{id}");
    let (status, host) = call(
        &t,
        &token,
        "PATCH",
        &path,
        json!({ "max_concurrency": null }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{host}");
    assert_eq!(
        (host["max_concurrency"].clone(), host["has_token"].clone()),
        (Value::Null, json!(true))
    );
    assert_eq!(
        (host["username"].as_str(), host["ref_exclude"].clone()),
        (Some("bot"), json!(["refs/pull/*"]))
    );
    let (_, host) = call(
        &t,
        &token,
        "PATCH",
        &path,
        json!({ "token": null, "username": null }),
    )
    .await;
    assert_eq!(
        (host["has_token"].clone(), host["username"].clone()),
        (json!(false), Value::Null)
    );
    let (status, code) = refused(&t, &token, "PATCH", &path, json!({ "ssh_key": "x" })).await;
    assert_eq!(
        (status, code.as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_body")
    );

    assert_eq!(
        api_call(&t, "DELETE", &path, Some(&token), None).await,
        (StatusCode::NO_CONTENT, Value::Null)
    );
    assert!(hosts::get(&t.state.pool, id).await.unwrap().is_none());
    assert_eq!(
        api_call(&t, "DELETE", &path, Some(&token), None).await.0,
        StatusCode::NOT_FOUND
    );
}

/// A new ed25519 private key in OpenSSH format.
fn new_private_key(dir: &std::path::Path) -> String {
    let path = dir.join("id_test");
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "api-test", "-f"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::read_to_string(path).unwrap()
}

#[tokio::test]
async fn an_ssh_key_is_stored_and_only_its_public_line_shows() {
    let (t, token) = writer().await;
    let (_, host) = call(&t, &token, "POST", "/hosts", offline_host()).await;
    let path = format!("/hosts/{}/ssh-key", host["id"]);
    let private = new_private_key(t.dir.path());

    let (status, host) = call(&t, &token, "PUT", &path, json!({ "private_key": private })).await;
    assert_eq!(status, StatusCode::OK, "{host}");
    assert_eq!(host["has_ssh_key"], true);
    let public = host["ssh_public_key"].as_str().unwrap();
    assert!(public.starts_with("ssh-ed25519 "), "{public}");
    assert!(!host.to_string().contains("PRIVATE KEY"));

    for (key, code) in [
        ("  ", "hosts.ssh_key_missing"),
        ("not-a-key", "hosts.ssh_key_invalid"),
    ] {
        let (status, found) =
            refused(&t, &token, "PUT", &path, json!({ "private_key": key })).await;
        assert_eq!(
            (status, found.as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, code)
        );
    }
    assert_eq!(
        api_call(&t, "DELETE", &path, Some(&token), None).await.0,
        StatusCode::NO_CONTENT
    );
    let host = hosts::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert!(host.ssh_key_enc.is_none() && host.ssh_public_key.is_none());
    assert_eq!(
        api_call(&t, "DELETE", "/hosts/9/ssh-key", Some(&token), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn sources_are_added_scanned_changed_and_deleted() {
    let (t, token) = writer().await;
    let (_, host) = call(&t, &token, "POST", "/hosts", offline_host()).await;
    let host_id = host["id"].as_i64().unwrap();
    let body =
        json!({ "host_id": host_id, "kind": "org", "name": "team", "include_patterns": ["app-*"] });
    let (status, source) = call(&t, &token, "POST", "/sources", body.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{source}");
    assert_eq!(
        (
            source["auto_add"].as_bool(),
            source["include_patterns"].clone()
        ),
        (Some(true), json!(["app-*"]))
    );
    let id = source["id"].as_i64().unwrap();

    assert_eq!(
        refused(&t, &token, "POST", "/sources", body).await,
        (StatusCode::CONFLICT, "sources.exists".to_owned())
    );
    let cases = [
        (
            json!({ "host_id": 99, "kind": "org", "name": "x" }),
            "sources.invalid_host",
        ),
        (
            json!({ "host_id": host_id, "kind": "team", "name": "x" }),
            "sources.invalid_kind",
        ),
        (
            json!({ "host_id": host_id, "kind": "org", "name": "a b" }),
            "sources.invalid_name",
        ),
        (
            json!({ "host_id": host_id, "kind": "org", "name": "x", "exclude_patterns": ["["] }),
            "sources.invalid_pattern",
        ),
    ];
    for (body, code) in cases {
        let (status, found) = refused(&t, &token, "POST", "/sources", body).await;
        assert_eq!(
            (status, found.as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, code)
        );
    }

    let path = format!("/sources/{id}");
    let (status, source) = call(
        &t,
        &token,
        "PATCH",
        &path,
        json!({ "auto_add": false, "exclude_patterns": ["*-old"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{source}");
    assert_eq!(
        (
            source["auto_add"].as_bool(),
            source["include_patterns"].clone()
        ),
        (Some(false), json!(["app-*"]))
    );
    assert_eq!(source["exclude_patterns"], json!(["*-old"]));

    let scan = api_call(&t, "POST", &format!("{path}/scan"), Some(&token), None).await;
    assert_eq!(scan, (StatusCode::ACCEPTED, json!({ "scanning": true })));
    assert_eq!(
        api_call(&t, "POST", "/sources/99/scan", Some(&token), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    assert_eq!(
        api_call(&t, "DELETE", &path, Some(&token), None).await.0,
        StatusCode::NO_CONTENT
    );
    assert!(sources::get(&t.state.pool, id).await.unwrap().is_none());
    assert_eq!(
        api_call(&t, "DELETE", &path, Some(&token), None).await.0,
        StatusCode::NOT_FOUND
    );
}
