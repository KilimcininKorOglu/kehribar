//! Syncs against a local HTTP Git server that requires Basic authentication:
//! the host token, the repository token that overrides it, and wrong tokens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::prelude::{BASE64_STANDARD, Engine as _};
use git_archive::crypto::Cipher;
use git_archive::db::hosts::{self, HostInput, Provider};
use git_archive::db::repos::{self, NewRepo, RepoStatus};
use git_archive::db::{self, unix_now};
use git_archive::sync::SyncContext;
use git_archive::sync::scheduler::Scheduler;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const HOST_USER: &str = "bot";
const HOST_TOKEN: &str = "host-token";
/// The repository token is sent with the default username.
const REPO_USER: &str = "git";
const REPO_TOKEN: &str = "repo-token";

/// Serves the repositories under `root` through `git http-backend` to
/// clients that send one of the accepted username/password pairs.
#[derive(Clone)]
struct GitServer {
    root: Arc<PathBuf>,
    accepted: Arc<Vec<String>>,
}

impl GitServer {
    fn authorized(&self, headers: &HeaderMap) -> bool {
        let Some(value) = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
        else {
            return false;
        };
        let Some(encoded) = value.strip_prefix("Basic ") else {
            return false;
        };
        let Ok(decoded) = BASE64_STANDARD.decode(encoded) else {
            return false;
        };
        let pair = String::from_utf8_lossy(&decoded);
        self.accepted.iter().any(|ok| *ok == pair)
    }
}

async fn handle(State(server): State<GitServer>, request: Request) -> Response {
    if !server.authorized(request.headers()) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"test\"")],
        )
            .into_response();
    }
    run_backend(&server.root, request).await
}

/// Runs `git http-backend` as a CGI program for one request.
async fn run_backend(root: &Path, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let header_text = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    };
    let mut child = Command::new("git")
        .arg("http-backend")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", parts.uri.path())
        .env("QUERY_STRING", parts.uri.query().unwrap_or(""))
        .env("REQUEST_METHOD", parts.method.as_str())
        .env("CONTENT_TYPE", header_text("content-type"))
        .env("HTTP_CONTENT_ENCODING", header_text("content-encoding"))
        .env("HTTP_GIT_PROTOCOL", header_text("git-protocol"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let input = to_bytes(body, usize::MAX).await.unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(&input).await.unwrap();
    drop(stdin);
    let output = child.wait_with_output().await.unwrap();
    cgi_response(&output.stdout)
}

/// Splits CGI output into status, headers and body.
fn cgi_response(output: &[u8]) -> Response {
    let split = output
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("CGI header end");
    let head = String::from_utf8_lossy(&output[..split]);
    let mut builder = Response::builder();
    for line in head.split("\r\n") {
        let (name, value) = line.split_once(": ").unwrap();
        if name.eq_ignore_ascii_case("status") {
            let code: u16 = value[..3].parse().unwrap();
            builder = builder.status(code);
        } else {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(Body::from(output[split + 4..].to_vec()))
        .unwrap()
}

/// Starts the server on a free local port and returns its address.
async fn start_server(root: &Path) -> SocketAddr {
    let server = GitServer {
        root: Arc::new(root.to_owned()),
        accepted: Arc::new(vec![
            format!("{HOST_USER}:{HOST_TOKEN}"),
            format!("{REPO_USER}:{REPO_TOKEN}"),
        ]),
    };
    let app = Router::new().fallback(handle).with_state(server);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

struct Setup {
    ctx: SyncContext,
    scheduler: Scheduler,
    host: String,
    url: String,
    _dir: tempfile::TempDir,
}

/// A served bare repository `team/project.git` with one commit on `main`.
async fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let source = common::source_repo(dir.path(), "work");
    let served = dir.path().join("served");
    std::fs::create_dir_all(served.join("team")).unwrap();
    common::git_in(
        dir.path(),
        &[
            "clone",
            "--quiet",
            "--bare",
            &source.to_string_lossy(),
            &served.join("team/project.git").to_string_lossy(),
        ],
    );
    let addr = start_server(&served).await;
    let config = common::test_config(&dir.path().join("data"));
    let ctx = SyncContext {
        pool: db::open_in_memory().await.unwrap(),
        cipher: Cipher::new(&config.secret_key),
        config: Arc::new(config),
        catalog: Arc::new(git_archive::i18n::Catalog::load().unwrap()),
        locks: Default::default(),
    };
    Setup {
        scheduler: Scheduler::new(ctx.clone()),
        ctx,
        host: addr.to_string(),
        url: format!("http://{addr}/team/project.git"),
        _dir: dir,
    }
}

impl Setup {
    async fn add_host(&self, username: &str, token: &str) {
        let token_enc = self.ctx.cipher.encrypt(token).unwrap();
        let input = HostInput {
            host: &self.host,
            provider: Provider::Generic,
            api_base_url: None,
            username: Some(username),
            token_enc: Some(&token_enc),
        };
        hosts::insert(&self.ctx.pool, &input, unix_now())
            .await
            .unwrap();
    }

    async fn add_repo(&self, token: Option<&str>) -> i64 {
        let token_enc = token.map(|t| self.ctx.cipher.encrypt(t).unwrap());
        let repo = NewRepo {
            url: &self.url,
            host: &self.host,
            owner: "team",
            name: "project",
            source_id: None,
            username: None,
            token_enc: token_enc.as_deref(),
            interval_minutes: None,
            wiki_enabled: true,
        };
        repos::insert(&self.ctx.pool, &repo, unix_now())
            .await
            .unwrap()
            .unwrap()
    }

    /// Runs one scheduler tick and returns the repository afterwards.
    async fn sync(&self, id: i64) -> repos::Repo {
        for handle in self.scheduler.tick().await.unwrap() {
            handle.await.unwrap();
        }
        repos::get(&self.ctx.pool, id).await.unwrap().unwrap()
    }
}

#[tokio::test]
async fn host_token_lets_the_sync_through() {
    let s = setup().await;
    s.add_host(HOST_USER, HOST_TOKEN).await;
    let id = s.add_repo(None).await;

    let repo = s.sync(id).await;

    assert_eq!(repo.status, RepoStatus::Ok, "{:?}", repo.last_error);
    assert_eq!(repo.default_branch.as_deref(), Some("main"));
    // The server answers 404 for the missing wiki, which is not an error.
    assert!(!repo.has_wiki);
    common::git_in(
        &s.ctx.repo_dir(&repo),
        &["rev-parse", "--verify", "refs/heads/main"],
    );
}

#[tokio::test]
async fn wrong_host_token_marks_the_repository_as_failed() {
    let s = setup().await;
    s.add_host(HOST_USER, "wrong-token").await;
    let id = s.add_repo(None).await;

    let repo = s.sync(id).await;

    assert_eq!(repo.status, RepoStatus::Error);
    let message = repo.last_error.unwrap();
    assert!(
        message.contains("401") || message.contains("Authentication failed"),
        "{message}"
    );
    assert!(!message.contains("wrong-token"), "{message}");
}

#[tokio::test]
async fn repository_token_overrides_the_host_token() {
    let s = setup().await;
    s.add_host(HOST_USER, "wrong-token").await;
    let id = s.add_repo(Some(REPO_TOKEN)).await;

    let repo = s.sync(id).await;
    assert_eq!(repo.status, RepoStatus::Ok, "{:?}", repo.last_error);
}

#[tokio::test]
async fn wrong_repository_token_is_not_rescued_by_the_host_token() {
    let s = setup().await;
    s.add_host(HOST_USER, HOST_TOKEN).await;
    let id = s.add_repo(Some("wrong-token")).await;

    let repo = s.sync(id).await;
    assert_eq!(repo.status, RepoStatus::Error);
}

#[tokio::test]
async fn without_any_token_a_protected_repository_is_unreachable() {
    let s = setup().await;
    let id = s.add_repo(None).await;

    let repo = s.sync(id).await;
    // Without credentials, an authentication request means "not available".
    assert_eq!(repo.status, RepoStatus::Unreachable);
}
