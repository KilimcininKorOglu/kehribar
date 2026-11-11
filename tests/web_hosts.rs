//! Host pages and the access settings of a repository.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::http::{StatusCode, header};
use common::{TestApp, body_text, header_value, urlencode};
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
    assert!(list.contains("Sunucu kaydedildi."));
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
            "Başına http:// ve sonuna yol eklemeyin.",
        ),
        (
            "host=github.com&provider=svn",
            "Listeden bir sunucu türü seçin.",
        ),
        (
            "host=gitlab.example.com&provider=gitlab&api_base_url=ftp%3A%2F%2Fx",
            "Yalnız http ve https ile başlayan URL&#39;ler kabul edilir.",
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
    assert!(body_text(again).await.contains("Bu sunucu zaten ekli."));
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
    assert!(page.contains("Bu repo, github.com sunucusunun varsayılan token&#39;ını kullanıyor."));

    t.post("/repos/1/credentials", "token=repo_secret2").await;
    let repo = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(decrypt(&t, repo.token_enc).as_deref(), Some("repo_secret2"));
    assert_eq!(
        t.post("/repos/9/credentials", "token=x").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn ref_patterns_are_stored_for_hosts_and_repositories() {
    let t = TestApp::new().await;
    let form = format!("{HOST_FORM}&ref_exclude=refs%2Fpull%2F*%0D%0A%0D%0Arefs%2Fpull%2F*");
    assert_eq!(
        t.post("/hosts", &form).await.status(),
        StatusCode::SEE_OTHER
    );
    let host = hosts::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(host.ref_exclude.as_deref(), Some("refs/pull/*"));
    assert!(
        body_text(t.get("/hosts/1").await)
            .await
            .contains(">refs/pull/*</textarea>")
    );

    t.post("/repos", "url=https%3A%2F%2Fgithub.com%2Fa%2Fb")
        .await;
    let response = t
        .post("/repos/1/settings", "ref_exclude=refs%2Fkeep-around%2F*")
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let repo = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(repo.ref_exclude.as_deref(), Some("refs/keep-around/*"));
    let page = body_text(t.get("/repos/1").await).await;
    assert!(page.contains("Sunucu ayarlarındaki desenler de uygulanır: <code>refs/pull/*</code>"));

    // An invalid pattern is shown again with the error and nothing changes.
    let bad = t
        .post("/repos/1/settings", "ref_exclude=refs%2Fa%2F*%2F*")
        .await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let page = body_text(bad).await;
    assert!(page.contains("Ref desenlerinden biri geçersiz."));
    assert!(page.contains(">refs/a/*/*</textarea>"));
    let repo = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(repo.ref_exclude.as_deref(), Some("refs/keep-around/*"));

    let bad_host = t
        .post(
            "/hosts/1",
            "host=github.com&provider=github&ref_exclude=pull%2F*",
        )
        .await;
    assert_eq!(bad_host.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // An empty field removes the patterns.
    t.post("/hosts/1", "host=github.com&provider=github&ref_exclude=")
        .await;
    let host = hosts::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(host.ref_exclude, None);
}

#[tokio::test]
async fn the_host_sync_limit_is_stored_and_checked() {
    let t = TestApp::new().await;
    let saved = t
        .post("/hosts", &format!("{HOST_FORM}&max_concurrency=2"))
        .await;
    assert_eq!(saved.status(), StatusCode::SEE_OTHER);
    let host = hosts::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(host.max_concurrency, Some(2));
    assert!(
        body_text(t.get("/hosts/1").await)
            .await
            .contains("name=\"max_concurrency\" type=\"number\" min=\"1\" max=\"32\" inputmode=\"numeric\" value=\"2\"")
    );

    for bad in ["0", "33", "two"] {
        let response = t
            .post(
                "/hosts/1",
                &format!("host=github.com&provider=github&max_concurrency={bad}"),
            )
            .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert!(
            body_text(response)
                .await
                .contains("1 ile 32 arasında bir sayı girin")
        );
    }
    let host = hosts::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(host.max_concurrency, Some(2));

    // An empty field removes the limit.
    t.post(
        "/hosts/1",
        "host=github.com&provider=github&max_concurrency=",
    )
    .await;
    let host = hosts::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(host.max_concurrency, None);
}

#[tokio::test]
async fn ssh_keys_are_generated_pasted_and_deleted_and_only_the_public_key_shows() {
    let t = TestApp::new().await;
    t.post("/hosts", HOST_FORM).await;

    let generated = t.post("/hosts/1/ssh/generate", "").await;
    assert_eq!(generated.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        header_value(&generated, header::LOCATION),
        "/hosts/1?notice=ssh_saved"
    );
    let host = hosts::get(&t.state.pool, 1).await.unwrap().unwrap();
    let public = host.ssh_public_key.clone().unwrap();
    assert!(public.starts_with("ssh-ed25519 "), "{public}");
    assert!(public.ends_with(" git-archive@github.com"));
    let private = decrypt(&t, host.ssh_key_enc.clone()).unwrap();
    assert!(private.contains("OPENSSH PRIVATE KEY"));
    let stored = host.ssh_key_enc.unwrap();
    assert!(!stored.windows(11).any(|w| w == b"PRIVATE KEY"));

    let page = body_text(t.get("/hosts/1?notice=ssh_saved").await).await;
    assert!(page.contains("SSH anahtarı kaydedildi."));
    assert!(page.contains(&public));
    assert!(!page.contains("PRIVATE KEY-----\n"));
    assert!(page.contains("Henüz kayıtlı host key yok."));

    let removed = t.post("/hosts/1/ssh/delete", "").await;
    assert_eq!(
        header_value(&removed, header::LOCATION),
        "/hosts/1?notice=ssh_removed"
    );
    let host = hosts::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert!(host.ssh_key_enc.is_none() && host.ssh_public_key.is_none());

    // A pasted key with CRLF line ends is stored with its public line.
    let crlf = private.replace('\n', "\r\n");
    let pasted = t
        .post(
            "/hosts/1/ssh/key",
            &format!("private_key={}", urlencode(&crlf)),
        )
        .await;
    assert_eq!(pasted.status(), StatusCode::SEE_OTHER);
    let host = hosts::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(host.ssh_public_key.as_deref(), Some(public.as_str()));
    assert_eq!(decrypt(&t, host.ssh_key_enc).unwrap(), private.trim());

    for (body, message) in [
        ("private_key=+", "Bir private key yapıştırın."),
        (
            "private_key=not-a-key",
            "parolası olmayan bir private key değil",
        ),
    ] {
        let response = t.post("/hosts/1/ssh/key", body).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body_text(response).await.contains(message), "{body}");
    }
    assert_eq!(
        t.post("/hosts/9/ssh/generate", "").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn stored_host_keys_are_listed_and_forgotten_on_the_host_page() {
    let t = TestApp::new().await;
    t.post("/hosts", HOST_FORM).await;
    let config = &t.state.config;
    std::fs::create_dir_all(config.known_hosts_dir()).unwrap();
    let known_hosts = config.known_hosts_of("github.com");
    let other = config.known_hosts_of("gitlab.com");
    std::fs::write(&known_hosts, "[github.com]:2222 ssh-ed25519 AAAAhostkey1\n").unwrap();
    std::fs::write(&other, "gitlab.com ssh-ed25519 AAAAother\n").unwrap();

    let page = body_text(t.get("/hosts/1").await).await;
    assert!(page.contains("[github.com]:2222 ssh-ed25519 AAAAhostkey1"));
    assert!(!page.contains("AAAAother"));

    let forgotten = t.post("/hosts/1/ssh/forget-host-keys", "").await;
    assert_eq!(
        header_value(&forgotten, header::LOCATION),
        "/hosts/1?notice=host_key_forgotten"
    );
    assert!(!known_hosts.exists());
    assert_eq!(
        std::fs::read_to_string(&other).unwrap(),
        "gitlab.com ssh-ed25519 AAAAother\n"
    );
    let page = body_text(t.get("/hosts/1?notice=host_key_forgotten").await).await;
    assert!(page.contains("Host key&#39;ler unutuldu."));
    assert!(!page.contains("AAAAhostkey1"));
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
