//! Host pages and the access settings of a repository.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::http::{StatusCode, header};
use common::{TestApp, body_text, header_value};
use git_archive::db::hosts::{self, Provider};
use git_archive::db::repos;

const HOST_FORM: &str =
    "host=GitHub.com&provider=github&api_base_url=&username=bot&token=ghp_secret1";

fn decrypt(t: &TestApp, stored: Option<Vec<u8>>) -> Option<String> {
    stored.map(|enc| t.state.cipher.decrypt(&enc).unwrap())
}

#[tokio::test]
async fn adding_a_host_stores_the_token_encrypted_and_never_shows_it() {
    let t = TestApp::new().await;
    let response = t.post("/hosts", HOST_FORM).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        header_value(&response, header::LOCATION),
        "/hosts?notice=saved"
    );

    let host = hosts::find_by_host(&t.state.pool, "github.com")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(host.host, "github.com");
    assert_eq!(host.provider, Provider::Github);
    assert_eq!(host.username.as_deref(), Some("bot"));
    let stored = host.token_enc.clone().unwrap();
    assert!(!stored.windows(11).any(|w| w == b"ghp_secret1"));
    assert_eq!(decrypt(&t, host.token_enc).as_deref(), Some("ghp_secret1"));

    let list = body_text(t.get("/hosts?notice=saved").await).await;
    assert!(list.contains("Host kaydedildi."));
    assert!(list.contains("href=\"/hosts/1\""));
    assert!(list.contains("Kayıtlı"));
    let edit = body_text(t.get("/hosts/1").await).await;
    for page in [&list, &edit] {
        assert!(!page.contains("ghp_secret1"));
    }
    assert!(edit.contains("Kayıtlı bir token var."));
}

#[tokio::test]
async fn invalid_and_duplicate_hosts_are_rejected() {
    let t = TestApp::new().await;
    for (form, message) in [
        (
            "host=https%3A%2F%2Fgithub.com&provider=github",
            "http:// ve yol yazmayın",
        ),
        (
            "host=github.com&provider=svn",
            "Listeden bir sunucu türü seçin.",
        ),
        (
            "host=gitlab.example.com&provider=gitlab&api_base_url=ftp%3A%2F%2Fx",
            "Yalnız http ve https URL&#39;leri desteklenir.",
        ),
    ] {
        let response = t.post("/hosts", form).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{form}"
        );
        assert!(body_text(response).await.contains(message), "{form}");
    }
    assert!(hosts::list(&t.state.pool).await.unwrap().is_empty());

    t.post("/hosts", HOST_FORM).await;
    let again = t.post("/hosts", "host=github.com&provider=generic").await;
    assert_eq!(again.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_text(again).await.contains("Bu host zaten listede."));
}

#[tokio::test]
async fn editing_keeps_replaces_or_clears_the_token() {
    let t = TestApp::new().await;
    t.post("/hosts", HOST_FORM).await;
    let token = || async {
        let host = hosts::get(&t.state.pool, 1).await.unwrap().unwrap();
        (host.provider, decrypt(&t, host.token_enc))
    };

    let keep = t
        .post(
            "/hosts/1",
            "host=github.com&provider=generic&username=bot&token=",
        )
        .await;
    assert_eq!(keep.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        token().await,
        (Provider::Generic, Some("ghp_secret1".into()))
    );

    // A new token wins over the clear box.
    t.post(
        "/hosts/1",
        "host=github.com&provider=github&token=ghp_new&clear_token=1",
    )
    .await;
    assert_eq!(token().await, (Provider::Github, Some("ghp_new".into())));

    t.post("/hosts/1", "host=github.com&provider=github&clear_token=1")
        .await;
    assert_eq!(token().await, (Provider::Github, None));

    let bad = t
        .post("/hosts/1", "host=github.com%2Fx&provider=github")
        .await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(t.get("/hosts/99").await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deleting_a_host_keeps_its_repositories() {
    let t = TestApp::new().await;
    t.post("/hosts", HOST_FORM).await;
    t.post("/repos", "url=https%3A%2F%2Fgithub.com%2Fa%2Fb")
        .await;

    let response = t.post("/hosts/1/delete", "").await;
    assert_eq!(
        header_value(&response, header::LOCATION),
        "/hosts?notice=deleted"
    );
    assert!(hosts::get(&t.state.pool, 1).await.unwrap().is_none());
    assert!(repos::get(&t.state.pool, 1).await.unwrap().is_some());
}

#[tokio::test]
async fn repository_access_follows_the_host_and_its_own_token() {
    let t = TestApp::new().await;
    t.post(
        "/repos",
        "url=https%3A%2F%2Fgithub.com%2Fa%2Fb&username=me&token=repo_secret1",
    )
    .await;
    let repo = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(repo.username.as_deref(), Some("me"));
    assert_eq!(decrypt(&t, repo.token_enc).as_deref(), Some("repo_secret1"));

    let page = body_text(t.get("/repos/1").await).await;
    assert!(
        page.contains("Bu repo kendi token&#39;ını kullanıyor."),
        "{page}"
    );
    assert!(!page.contains("repo_secret1"));

    // Clearing the own token falls back to the host token.
    t.post("/hosts", HOST_FORM).await;
    let response = t
        .post("/repos/1/credentials", "username=&token=&clear_token=1")
        .await;
    assert_eq!(
        header_value(&response, header::LOCATION),
        "/repos/1?notice=credentials"
    );
    let repo = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!((repo.username, repo.token_enc), (None, None));
    let page = body_text(t.get("/repos/1?notice=credentials").await).await;
    assert!(page.contains("Erişim ayarları kaydedildi."));
    assert!(page.contains("github.com hostunun varsayılan token&#39;ını kullanıyor."));

    t.post("/repos/1/credentials", "token=repo_secret2").await;
    let repo = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(decrypt(&t, repo.token_enc).as_deref(), Some("repo_secret2"));
    assert_eq!(
        t.post("/repos/9/credentials", "token=x").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn interval_settings_do_not_touch_the_credentials() {
    let t = TestApp::new().await;
    t.post(
        "/repos",
        "url=https%3A%2F%2Fgithub.com%2Fa%2Fb&username=me&token=repo_secret1",
    )
    .await;
    t.post("/repos/1/settings", "interval=30").await;
    let repo = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(repo.interval_minutes, Some(30));
    assert_eq!(repo.username.as_deref(), Some("me"));
    assert_eq!(decrypt(&t, repo.token_enc).as_deref(), Some("repo_secret1"));
}
