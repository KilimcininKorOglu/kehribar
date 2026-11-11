//! Notifications: state changes reach the webhook and Telegram once, and
//! the settings page saves and tests the channels.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::http::{StatusCode, header};
use common::{TestApp, body_text, header_value, urlencode};
use git_archive::config::Config;
use git_archive::crypto::Cipher;
use git_archive::db::repos::{self, NewRepo, RepoStatus};
use git_archive::db::{self, unix_now};
use git_archive::i18n::Catalog;
use git_archive::notify::{self, Event};
use git_archive::sync::SyncContext;
use git_archive::sync::scheduler::Scheduler;
use serde_json::Value;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn context(config: Config, pool: sqlx::SqlitePool) -> SyncContext {
    SyncContext {
        pool,
        cipher: Cipher::new(&config.secret_key),
        config: Arc::new(config),
        catalog: Arc::new(Catalog::load().unwrap()),
        locks: Default::default(),
        guard: Default::default(),
    }
}

async fn webhook_bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

#[tokio::test]
async fn a_repository_is_reported_when_it_fails_and_when_it_recovers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut config = common::test_config(&dir.path().join("data"));
    config.notify.webhook_url = Some(format!("{}/hook", server.uri()));
    config.public_url = Some("https://archive.example".into());
    let ctx = context(config, db::open_in_memory().await.unwrap());
    let scheduler = Scheduler::new(ctx.clone());

    let source = common::source_repo(dir.path(), "project");
    let url = source.to_string_lossy().into_owned();
    let new = NewRepo {
        url: &url,
        host: "local",
        owner: "team",
        name: "project",
        source_id: None,
        username: None,
        token_enc: None,
        interval_minutes: None,
        wiki_enabled: false,
    };
    let id = repos::insert(&ctx.pool, &new, unix_now())
        .await
        .unwrap()
        .unwrap();
    let sync = async || {
        repos::schedule_now(&ctx.pool, id, unix_now())
            .await
            .unwrap();
        for handle in scheduler.tick().await.unwrap() {
            handle.await.unwrap();
        }
    };

    // A first sync that works is no news.
    sync().await;
    assert!(webhook_bodies(&server).await.is_empty());

    let moved = dir.path().join("moved");
    std::fs::rename(&source, &moved).unwrap();
    sync().await;
    sync().await;
    let bodies = webhook_bodies(&server).await;
    assert_eq!(bodies.len(), 1, "a lasting error is sent once: {bodies:?}");
    let failed = &bodies[0];
    assert_eq!(failed["event"], "repo_failed");
    assert_eq!(failed["status"], "unreachable");
    assert_eq!(failed["previous_status"], "ok");
    assert_eq!(failed["repo"]["name"], "local/team/project");
    assert_eq!(
        failed["link"],
        format!("https://archive.example/repos/{id}")
    );
    let text = failed["text"].as_str().unwrap();
    assert!(
        text.starts_with(
            "git-archive: local/team/project güncellenemedi. Durum: Kaynağa ulaşılamıyor."
        ),
        "{text}"
    );
    assert_eq!(failed["content"], failed["text"]);

    std::fs::rename(&moved, &source).unwrap();
    sync().await;
    let bodies = webhook_bodies(&server).await;
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[1]["event"], "repo_recovered");
    assert_eq!(bodies[1]["status"], "ok");
    let repo = repos::get(&ctx.pool, id).await.unwrap().unwrap();
    assert_eq!(repo.status, RepoStatus::Ok);
}

#[tokio::test]
async fn telegram_gets_the_text_and_errors_never_show_the_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/bot123:secret/sendMessage"))
        .and(body_partial_json(serde_json::json!({ "chat_id": "42" })))
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"ok\":true}"))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut config = common::test_config(dir.path());
    config.notify.telegram_api_url = server.uri();
    config.notify.telegram_bot_token = Some("123:secret".into());
    config.notify.telegram_chat_id = Some("42".into());
    config.notify.lang = git_archive::i18n::Lang::En;
    let ctx = context(config.clone(), db::open_in_memory().await.unwrap());

    notify::deliver(&ctx, &Event::Test).await.unwrap();
    let request = &server.received_requests().await.unwrap()[0];
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(
        body["text"],
        "git-archive: This is a test notification. The notification settings work."
    );

    // A refused token: the error names the cause, not the token.
    let mut wrong = config;
    wrong.notify.telegram_bot_token = Some("999:wrong".into());
    Mock::given(method("POST"))
        .and(path("/bot999:wrong/sendMessage"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("{\"ok\":false,\"description\":\"Unauthorized\"}"),
        )
        .mount(&server)
        .await;
    let ctx = context(wrong, db::open_in_memory().await.unwrap());
    let err = notify::deliver(&ctx, &Event::Test).await.unwrap_err();
    assert_eq!(err, "Telegram: HTTP 401: Unauthorized");
}

#[tokio::test]
async fn a_token_saved_under_another_key_does_not_stop_the_webhook() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut config = common::test_config(dir.path());
    config.notify.webhook_url = Some(format!("{}/hook", server.uri()));
    config.notify.telegram_chat_id = Some("42".into());
    let ctx = context(config, db::open_in_memory().await.unwrap());
    // The secret key changed after the token was saved.
    let old_key = Cipher::new(&[7u8; 32]);
    let token = notify::settings::TokenChange::Set(old_key.encrypt("123:secret").unwrap());
    notify::settings::save(&ctx.pool, None, None, None, &token)
        .await
        .unwrap();

    let err = notify::deliver(&ctx, &Event::Test).await.unwrap_err();
    assert!(err.starts_with("Telegram: "), "{err}");
    assert!(!err.contains("secret"), "{err}");
    assert_eq!(webhook_bodies(&server).await.len(), 1);
}

#[tokio::test]
async fn settings_page_saves_and_tests_the_channels() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ok"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/broken"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    let t = TestApp::new().await;

    let page = body_text(t.get("/settings").await).await;
    assert!(page.contains("Hiçbir kanal ayarlı değil"));
    let none = t.post("/settings/notifications/test", "").await;
    assert_eq!(none.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body_text(none)
            .await
            .contains("Deneme için önce bir webhook URL")
    );

    let bad = t
        .post("/settings/notifications", "webhook_url=ftp%3A%2F%2Fx")
        .await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body_text(bad)
            .await
            .contains("http:// veya https:// ile başlamalı")
    );
    let bad_token = t
        .post("/settings/notifications", "telegram_token=no+colon")
        .await;
    assert_eq!(bad_token.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let hook = format!("{}/ok", server.uri());
    let form = format!(
        "webhook_url={}&telegram_token=1%3Asecret&telegram_chat_id=7&lang=en",
        urlencode(&hook)
    );
    let saved = t.post("/settings/notifications", &form).await;
    assert_eq!(
        header_value(&saved, header::LOCATION),
        "/settings?notice=notify_saved"
    );
    let page = body_text(t.get("/settings?notice=notify_saved").await).await;
    assert!(page.contains("Bildirim ayarları kaydedildi."));
    assert!(page.contains("Etkin kanallar: Webhook, Telegram"));
    assert!(page.contains("Kayıtlı bir bot token&#39;ı var."));
    assert!(!page.contains("1:secret"));

    // Only the webhook: the stored Telegram token is cleared.
    let form = format!("webhook_url={}&clear_telegram_token=1", urlencode(&hook));
    t.post("/settings/notifications", &form).await;
    let sent = t.post("/settings/notifications/test", "").await;
    assert_eq!(sent.status(), StatusCode::OK);
    assert!(
        body_text(sent)
            .await
            .contains("Deneme bildirimi gönderildi.")
    );

    let broken = format!(
        "webhook_url={}",
        urlencode(&format!("{}/broken", server.uri()))
    );
    t.post("/settings/notifications", &broken).await;
    let failed = t.post("/settings/notifications/test", "").await;
    assert_eq!(failed.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_text(failed).await.contains("webhook: HTTP 500: boom"));
    // The error stays on the page until a delivery works.
    let page = body_text(t.get("/settings").await).await;
    assert!(page.contains("Son bildirim hatası"));
}
