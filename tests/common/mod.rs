//! Helpers shared by the integration tests.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, Response, StatusCode, header};
use git_archive::config::Config;
use git_archive::db;
use git_archive::i18n::Catalog;
use git_archive::web::{self, AppState};
use tempfile::TempDir;
use tower::ServiceExt;

pub const USER: &str = "admin";
pub const PASSWORD: &str = "correct horse";

pub fn test_config(data_dir: &Path) -> Config {
    let data_dir = data_dir.to_string_lossy().into_owned();
    Config::from_lookup(move |name| match name {
        "GIT_ARCHIVE_ADMIN_USERNAME" => Some(USER.into()),
        "GIT_ARCHIVE_ADMIN_PASSWORD" => Some(PASSWORD.into()),
        "GIT_ARCHIVE_SECRET_KEY" => Some("ab".repeat(32)),
        "GIT_ARCHIVE_DATA_DIR" => Some(data_dir.clone()),
        _ => None,
    })
    .unwrap()
}

/// Runs git in `dir` with a fixed identity and no user configuration.
pub fn git_in(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Creates a source repository with one commit on `main` under `parent`.
pub fn source_repo(parent: &Path, name: &str) -> PathBuf {
    let path = parent.join(name);
    std::fs::create_dir_all(&path).unwrap();
    git_in(&path, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(path.join("README.md"), "# Test\n").unwrap();
    git_in(&path, &["add", "README.md"]);
    git_in(&path, &["commit", "--quiet", "-m", "initial"]);
    path
}

/// An application with a signed-in session, backed by an in-memory database
/// and a temporary data directory.
pub struct TestApp {
    pub app: Router,
    pub state: AppState,
    pub cookie: String,
    pub csrf: String,
    pub dir: TempDir,
}

impl TestApp {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_in_memory().await.unwrap();
        let state = AppState::new(pool, test_config(dir.path()), Catalog::load().unwrap());
        let app = web::router(state.clone())
            .layer(MockConnectInfo(SocketAddr::from(([192, 0, 2, 7], 4000))));
        let mut test = Self {
            app,
            state,
            cookie: String::new(),
            csrf: String::new(),
            dir,
        };
        test.sign_in().await;
        test
    }

    async fn sign_in(&mut self) {
        let body = format!("username={USER}&password=correct+horse");
        let response = self.send(form_request("/login", &body)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let set_cookie = header_value(&response, header::SET_COOKIE);
        self.cookie = set_cookie.split(';').next().unwrap().to_owned();
        let html = body_text(self.get("/").await).await;
        let marker = "name=\"_csrf\" value=\"";
        let start = html.find(marker).expect("CSRF field") + marker.len();
        self.csrf = html[start..start + 64].to_owned();
    }

    pub async fn send(&self, request: Request<Body>) -> Response<Body> {
        send(&self.app, request).await
    }

    pub async fn get(&self, uri: &str) -> Response<Body> {
        let request = Request::get(uri)
            .header(header::COOKIE, &self.cookie)
            .body(Body::empty())
            .unwrap();
        self.send(request).await
    }

    /// Posts a signed-in form; `_csrf` is added.
    pub async fn post(&self, uri: &str, body: &str) -> Response<Body> {
        let body = if body.is_empty() {
            format!("_csrf={}", self.csrf)
        } else {
            format!("{body}&_csrf={}", self.csrf)
        };
        let mut request = form_request(uri, &body);
        request
            .headers_mut()
            .insert(header::COOKIE, self.cookie.parse().unwrap());
        self.send(request).await
    }
}

pub async fn send(app: &Router, request: Request<Body>) -> Response<Body> {
    app.clone().oneshot(request).await.unwrap()
}

pub fn form_request(uri: &str, body: &str) -> Request<Body> {
    Request::post(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

pub async fn body_text(response: Response<Body>) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

pub fn header_value(response: &Response<Body>, name: impl header::AsHeaderName) -> String {
    response
        .headers()
        .get(name)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default()
}
