//! Export of hosts, sources and repositories to JSON, and their import.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::http::{StatusCode, header};
use common::{TestApp, body_text, header_value, urlencode};
use kehribar::db::hosts::{self, Host};
use kehribar::db::repos::{self, Repo, RepoSettings};
use kehribar::db::sources::{self, NewSource, SourceKind, SourceOptions};
use kehribar::db::unix_now;
use serde_json::Value;

const HOST_TOKEN: &str = "host-token-value";
const REPO_TOKEN: &str = "repo-token-value";
const SSH_PRIVATE: &str = "private key text";
const SSH_PUBLIC: &str = "ssh-ed25519 AAAAC3Nz test@archive";
const OTHER_KEY: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

/// An installation with a host (token, SSH key, limits), a filtered source,
/// a repository of that source with its own token, and a gist.
async fn filled_app() -> TestApp {
    let t = TestApp::new().await;
    let host_form = format!(
        "host=github.com&provider=github&username=octo&token={HOST_TOKEN}\
         &ref_exclude=refs%2Fpull%2F*&max_concurrency=2"
    );
    assert_eq!(
        t.post("/hosts", &host_form).await.status(),
        StatusCode::SEE_OTHER
    );
    let host = hosts::find_by_host(&t.state.pool, "github.com")
        .await
        .unwrap()
        .unwrap();
    let private = t.state.cipher.encrypt(SSH_PRIVATE).unwrap();
    hosts::set_ssh_key(&t.state.pool, host.id, Some((&private, SSH_PUBLIC)))
        .await
        .unwrap();
    let source = NewSource {
        host_id: host.id,
        kind: SourceKind::Org,
        name: "rust-lang",
        options: SourceOptions {
            auto_add: true,
            include_patterns: Some("rust*".into()),
            use_ssh: true,
            ..SourceOptions::default()
        },
    };
    let source_id = sources::insert(&t.state.pool, &source, unix_now())
        .await
        .unwrap()
        .unwrap();

    let repo_form = format!(
        "url={}&interval=90&wiki=1&username=bot&token={REPO_TOKEN}",
        urlencode("https://github.com/rust-lang/rust.git")
    );
    assert_eq!(
        t.post("/repos", &repo_form).await.status(),
        StatusCode::SEE_OTHER
    );
    sqlx::query("UPDATE repos SET source_id = ? WHERE id = 1")
        .bind(source_id)
        .execute(&t.state.pool)
        .await
        .unwrap();
    let settings = RepoSettings {
        interval_minutes: Some(90),
        wiki_enabled: true,
        ref_exclude: Some("refs/heads/tmp-*"),
        lfs_enabled: true,
    };
    repos::update_settings(&t.state.pool, 1, settings)
        .await
        .unwrap();
    let gist = format!(
        "url={}",
        urlencode("https://gist.github.com/octocat/abc123")
    );
    assert_eq!(
        t.post("/repos", &gist).await.status(),
        StatusCode::SEE_OTHER
    );
    t
}

async fn export_bytes(t: &TestApp) -> Vec<u8> {
    let response = t.get("/export.json").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_value(&response, header::CONTENT_TYPE),
        "application/json"
    );
    assert!(header_value(&response, header::CONTENT_DISPOSITION).starts_with("attachment"));
    assert_eq!(header_value(&response, header::CACHE_CONTROL), "no-store");
    body_text(response).await.into_bytes()
}

async fn only_host(t: &TestApp) -> Host {
    let mut list = hosts::list(&t.state.pool).await.unwrap();
    assert_eq!(list.len(), 1);
    list.remove(0)
}

async fn repo_by_host(t: &TestApp, host: &str) -> Repo {
    repos::list(&t.state.pool, None)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.host == host)
        .unwrap()
}

#[tokio::test]
async fn export_holds_every_record_with_encrypted_secrets() {
    let t = filled_app().await;
    let bytes = export_bytes(&t).await;
    let text = String::from_utf8(bytes.clone()).unwrap();
    for plain in [HOST_TOKEN, REPO_TOKEN, SSH_PRIVATE] {
        assert!(!text.contains(plain), "{plain} is in the export");
    }
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["format"], 1);
    assert_eq!(json["hosts"][0]["host"], "github.com");
    assert_eq!(json["hosts"][0]["max_concurrency"], 2);
    assert_eq!(json["hosts"][0]["ssh_public_key"], SSH_PUBLIC);
    assert!(json["hosts"][0]["token"].is_string());
    assert_eq!(json["sources"][0]["kind"], "org");
    assert_eq!(json["sources"][0]["include_patterns"], "rust*");
    let repos = json["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 2);
    // The gist goes out in the form the add form takes, with its owner.
    assert!(
        repos
            .iter()
            .any(|r| r["url"] == "https://gist.github.com/octocat/abc123")
    );
    let rust = repos.iter().find(|r| r["source"].is_object()).unwrap();
    assert_eq!(rust["source"]["name"], "rust-lang");
    assert_eq!(rust["lfs_enabled"], true);
}

#[tokio::test]
async fn import_with_the_same_key_restores_records_and_secrets() {
    let bytes = export_bytes(&filled_app().await).await;
    let t = TestApp::new().await;
    let response = t.post_file("/import", "file", &bytes, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    assert!(html.contains("4 kayıt eklendi."), "{html}");
    assert!(!html.contains("çözülemedi"));

    let host = only_host(&t).await;
    let cipher = &t.state.cipher;
    assert_eq!(
        cipher.decrypt(host.token_enc.as_ref().unwrap()).unwrap(),
        HOST_TOKEN
    );
    assert_eq!(
        cipher.decrypt(host.ssh_key_enc.as_ref().unwrap()).unwrap(),
        SSH_PRIVATE
    );
    assert_eq!(host.ssh_public_key.as_deref(), Some(SSH_PUBLIC));
    assert_eq!(host.username.as_deref(), Some("octo"));
    assert_eq!(host.ref_exclude.as_deref(), Some("refs/pull/*"));
    assert_eq!(host.max_concurrency, Some(2));

    let source = sources::list(&t.state.pool).await.unwrap().remove(0);
    assert_eq!(
        (source.kind, source.name.as_str()),
        (SourceKind::Org, "rust-lang")
    );
    assert!(source.auto_add && source.use_ssh);
    assert_eq!(source.include_patterns.as_deref(), Some("rust*"));

    let rust = repo_by_host(&t, "github.com").await;
    assert_eq!(rust.url, "https://github.com/rust-lang/rust.git");
    assert_eq!(rust.source_id, Some(source.id));
    assert_eq!(
        cipher.decrypt(rust.token_enc.as_ref().unwrap()).unwrap(),
        REPO_TOKEN
    );
    assert_eq!(rust.username.as_deref(), Some("bot"));
    assert_eq!(rust.interval_minutes, Some(90));
    assert!(rust.wiki_enabled && rust.lfs_enabled);
    assert_eq!(rust.ref_exclude.as_deref(), Some("refs/heads/tmp-*"));
    let gist = repo_by_host(&t, "gist.github.com").await;
    assert_eq!(gist.url, "https://gist.github.com/abc123.git");
    assert_eq!(
        (gist.owner.as_str(), gist.name.as_str()),
        ("octocat", "abc123")
    );

    // A second import finds everything and adds nothing.
    let again = body_text(t.post_file("/import", "file", &bytes, true).await).await;
    assert!(again.contains("0 kayıt eklendi."), "{again}");
    assert_eq!(again.matches("Zaten var").count(), 4);
    assert_eq!(repos::list(&t.state.pool, None).await.unwrap().len(), 2);
    assert_eq!(sources::list(&t.state.pool).await.unwrap().len(), 1);
}

#[tokio::test]
async fn import_with_another_key_adds_the_records_without_their_secrets() {
    let bytes = export_bytes(&filled_app().await).await;
    let t = TestApp::with_secret_key(OTHER_KEY).await;
    let html = body_text(t.post_file("/import", "file", &bytes, true).await).await;
    assert!(html.contains("4 kayıt eklendi."), "{html}");
    assert!(
        html.contains("2 kayıt token&#39;ı veya SSH anahtarı olmadan eklendi"),
        "{html}"
    );
    assert_eq!(html.matches("Secret&#39;sız eklendi").count(), 2);

    let host = only_host(&t).await;
    assert!(host.token_enc.is_none() && host.ssh_key_enc.is_none());
    assert!(host.ssh_public_key.is_none());
    assert_eq!(host.username.as_deref(), Some("octo"));
    let rust = repo_by_host(&t, "github.com").await;
    assert!(rust.token_enc.is_none());
    assert_eq!(rust.username.as_deref(), Some("bot"));
}

#[tokio::test]
async fn invalid_entries_are_reported_and_the_rest_is_added() {
    let t = TestApp::new().await;
    let file = serde_json::json!({
        "format": 1,
        "hosts": [{"host": "http://bad host", "provider": "github"}],
        "sources": [{"host": "gitlab.com", "kind": "org", "name": "group"}],
        "repos": [
            {"url": "ftp://example.com/a/b"},
            {"url": "https://codeberg.org/forgejo/forgejo", "interval_minutes": 0},
            {"url": "https://codeberg.org/forgejo/forgejo", "token": "not base64!"}
        ]
    });
    let html = body_text(
        t.post_file("/import", "file", file.to_string().as_bytes(), true)
            .await,
    )
    .await;
    assert!(html.contains("1 kayıt eklendi."), "{html}");
    assert!(
        html.contains("github.com veya gitea.local:3000 gibi"),
        "{html}"
    );
    assert!(html.contains("Kaynağın sunucusu ne arşivde ne de dosyada var."));
    assert!(html.contains("Yalnız http, https ve ssh ile başlayan"));
    assert!(html.contains("Aralık 1 ile 10080 dakika arasında"));
    // A secret that is not even base64 is dropped like any other.
    assert!(html.contains("Secret&#39;sız eklendi"));
    let added = repos::list(&t.state.pool, None).await.unwrap();
    assert_eq!(added.len(), 1);
    assert!(added[0].token_enc.is_none());
    assert!(hosts::list(&t.state.pool).await.unwrap().is_empty());
}

#[tokio::test]
async fn unreadable_files_change_nothing() {
    let t = TestApp::new().await;
    let cases: [(&[u8], &str); 3] = [
        (
            b"not json",
            "Bu dosya bir Kehribar dışa aktarma dosyası değil.",
        ),
        (
            br#"{"format": 2}"#,
            "Bu dosyanın biçim sürümü desteklenmiyor.",
        ),
        (b"", "Bir dışa aktarma dosyası seçin."),
    ];
    for (content, message) in cases {
        let response = t.post_file("/import", "file", content, true).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body_text(response).await.contains(message), "{message}");
    }
    let missing = t.post_file("/import", "other", b"{}", true).await;
    assert_eq!(missing.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn import_needs_the_csrf_token() {
    let t = TestApp::new().await;
    let file = br#"{"format": 1, "repos": [{"url": "https://github.com/a/b"}]}"#;
    let response = t.post_file("/import", "file", file, false).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(repos::list(&t.state.pool, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_failing_write_rolls_the_whole_import_back() {
    let bytes = export_bytes(&filled_app().await).await;
    let t = TestApp::new().await;
    sqlx::query(
        "CREATE TRIGGER refuse_repos BEFORE INSERT ON repos \
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .execute(&t.state.pool)
    .await
    .unwrap();
    let response = t.post_file("/import", "file", &bytes, true).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    // The host and the source came before the failing repository.
    assert!(hosts::list(&t.state.pool).await.unwrap().is_empty());
    assert!(sources::list(&t.state.pool).await.unwrap().is_empty());
}

#[tokio::test]
async fn settings_page_offers_export_and_import() {
    let t = TestApp::new().await;
    let html = body_text(t.get("/settings").await).await;
    assert!(html.contains("href=\"/export.json\""));
    assert!(html.contains("action=\"/import\" enctype=\"multipart/form-data\""));
}
