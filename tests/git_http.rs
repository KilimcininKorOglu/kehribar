//! Getting an archive back: `git clone` over smart HTTP from a running
//! server, and ZIP and tar.gz downloads of a revision.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use base64::prelude::{BASE64_STANDARD, Engine as _};
use common::{
    PASSWORD, TestApp, USER, add_repo, create_token, git_in, header_value, send, source_repo,
};
use git_archive::db::repos::{self, NewRepo, RepoStatus};
use git_archive::db::unix_now;
use git_archive::web::{self, auth::MAX_FAILED_LOGINS};

const REPO_PATH: &str = "/git/local/team/project.git";

/// An archived source with two commits, a tag and a second branch.
async fn archived() -> (TestApp, PathBuf) {
    let t = TestApp::new().await;
    let source = source_repo(t.dir.path(), "source");
    std::fs::create_dir_all(source.join("src")).unwrap();
    std::fs::write(source.join("src/lib.rs"), "pub fn answer() -> u8 { 42 }\n").unwrap();
    git_in(&source, &["add", "."]);
    git_in(&source, &["commit", "--quiet", "-m", "add the library"]);
    git_in(&source, &["tag", "v1"]);
    git_in(&source, &["branch", "feature"]);
    let url = source.to_string_lossy();
    let repo = NewRepo {
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
    repos::insert(&t.state.pool, &repo, unix_now())
        .await
        .unwrap();
    sync(&t).await;
    (t, source)
}

async fn sync(t: &TestApp) {
    for handle in t.state.scheduler.tick().await.unwrap() {
        handle.await.unwrap();
    }
    let repo = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(repo.status, RepoStatus::Ok, "{:?}", repo.last_error);
}

/// Serves the application on a local TCP port and gives its address.
async fn serve(t: &TestApp) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = web::router(t.state.clone()).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// Runs git without user configuration, credential helpers or prompts.
async fn git(dir: &Path, args: &[&str]) -> Output {
    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "");
    command.output().await.unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn authed_url(addr: SocketAddr) -> String {
    let password = PASSWORD.replace(' ', "%20");
    format!("http://{USER}:{password}@{addr}{REPO_PATH}")
}

fn basic(user: &str, password: &str) -> String {
    format!(
        "Basic {}",
        BASE64_STANDARD.encode(format!("{user}:{password}"))
    )
}

fn advertise_request(auth: Option<&str>) -> Request<Body> {
    let mut request = Request::get(format!("{REPO_PATH}/info/refs?service=git-upload-pack"));
    if let Some(auth) = auth {
        request = request.header(header::AUTHORIZATION, auth);
    }
    request.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn clone_gives_back_every_commit_of_the_source() {
    let (t, source) = archived().await;
    let addr = serve(&t).await;
    let work = t.dir.path();

    let output = git(work, &["clone", "--quiet", &authed_url(addr), "copy"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let copy = work.join("copy");
    let count = |dir: &Path| git_in(dir, &["rev-list", "--all", "--count"]);
    assert_eq!(count(&copy), count(&source));
    assert_eq!(
        git_in(&copy, &["rev-parse", "HEAD"]),
        git_in(&source, &["rev-parse", "main"])
    );
    assert_eq!(git_in(&copy, &["tag", "--list"]), "v1");
    assert!(git_in(&copy, &["branch", "--remotes"]).contains("origin/feature"));
    assert_eq!(
        std::fs::read_to_string(copy.join("src/lib.rs")).unwrap(),
        "pub fn answer() -> u8 { 42 }\n"
    );

    // Protocol version 0 goes through the same backend.
    let v0 = git(
        work,
        &[
            "-c",
            "protocol.version=0",
            "clone",
            "--quiet",
            &authed_url(addr),
            "copy-v0",
        ],
    )
    .await;
    assert!(v0.status.success(), "{}", stderr(&v0));
    assert_eq!(count(&work.join("copy-v0")), count(&source));
}

#[tokio::test]
async fn mirror_clone_includes_the_rescued_history() {
    let (t, source) = archived().await;
    let old_head = git_in(&source, &["rev-parse", "main"]);
    // The source rewrites main; the archive keeps the old commit.
    git_in(
        &source,
        &["commit", "--quiet", "--amend", "-m", "rewritten"],
    );
    repos::schedule_now(&t.state.pool, 1, unix_now())
        .await
        .unwrap();
    sync(&t).await;

    let addr = serve(&t).await;
    let work = t.dir.path();
    let output = git(
        work,
        &[
            "clone",
            "--quiet",
            "--mirror",
            &authed_url(addr),
            "mirror.git",
        ],
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    let mirror = work.join("mirror.git");
    let rescued = git_in(
        &mirror,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/git-archive/",
        ],
    );
    assert_eq!(rescued, old_head);
    git_in(
        &mirror,
        &["cat-file", "-e", &format!("{old_head}^{{commit}}")],
    );
}

#[tokio::test]
async fn clone_needs_the_admin_credentials() {
    let (t, _) = archived().await;
    let addr = serve(&t).await;
    let work = t.dir.path();

    let anonymous = format!("http://{addr}{REPO_PATH}");
    let output = git(work, &["clone", "--quiet", &anonymous, "anon"]).await;
    assert!(!output.status.success());
    assert!(!work.join("anon/.git").exists());

    let wrong = format!("http://{USER}:wrong@{addr}{REPO_PATH}");
    let output = git(work, &["clone", "--quiet", &wrong, "wrong"]).await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Authentication failed"),
        "{}",
        stderr(&output)
    );

    let response = send(&t.app, advertise_request(None)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(header_value(&response, header::WWW_AUTHENTICATE).starts_with("Basic realm="));
    // A browser session is not enough.
    let mut with_cookie = advertise_request(None);
    with_cookie
        .headers_mut()
        .insert(header::COOKIE, t.cookie.parse().unwrap());
    assert_eq!(
        send(&t.app, with_cookie).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn clone_tokens_open_their_repositories_only() {
    let (t, source) = archived().await;
    let other = add_repo(&t, "other").await;
    let all = create_token(&t, "kind=clone&name=all&scope=all").await;
    let mine = create_token(&t, "kind=clone&name=mine&scope=selected&repo=1").await;
    let elsewhere = create_token(
        &t,
        &format!("kind=clone&name=elsewhere&scope=selected&repo={other}"),
    )
    .await;
    let api = create_token(&t, "kind=api&name=api&permission=write").await;

    // A real clone with the token as the password and any username.
    let addr = serve(&t).await;
    let work = t.dir.path();
    let url = format!("http://anyone:{all}@{addr}{REPO_PATH}");
    let output = git(work, &["clone", "--quiet", &url, "copy"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        git_in(&work.join("copy"), &["rev-parse", "HEAD"]),
        git_in(&source, &["rev-parse", "main"])
    );
    let used: Option<i64> =
        sqlx::query_scalar("SELECT last_used_at FROM access_tokens WHERE name = 'all'")
            .fetch_one(&t.state.pool)
            .await
            .unwrap();
    assert!(used.is_some());

    let status = |password: String| {
        let app = t.app.clone();
        async move {
            send(&app, advertise_request(Some(&basic("x", &password))))
                .await
                .status()
        }
    };
    assert_eq!(status(mine.clone()).await, StatusCode::OK);
    // Outside its scope the repository does not exist for the token.
    assert_eq!(status(elsewhere).await, StatusCode::NOT_FOUND);
    // An API token is not a clone token.
    assert_eq!(status(api).await, StatusCode::UNAUTHORIZED);

    // An expired token and a revoked one are refused.
    sqlx::query("UPDATE access_tokens SET expires_at = 1 WHERE name = 'mine'")
        .execute(&t.state.pool)
        .await
        .unwrap();
    assert_eq!(status(mine).await, StatusCode::UNAUTHORIZED);
    sqlx::query("DELETE FROM access_tokens WHERE name = 'all'")
        .execute(&t.state.pool)
        .await
        .unwrap();
    assert_eq!(status(all).await, StatusCode::UNAUTHORIZED);
    // The admin password still works.
    assert_eq!(status(PASSWORD.to_owned()).await, StatusCode::UNAUTHORIZED);
    let admin = send(&t.app, advertise_request(Some(&basic(USER, PASSWORD)))).await;
    assert_eq!(admin.status(), StatusCode::OK);
}

#[tokio::test]
async fn push_is_refused() {
    let (t, source) = archived().await;
    let addr = serve(&t).await;
    let work = t.dir.path();
    let output = git(work, &["clone", "--quiet", &authed_url(addr), "copy"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let copy = work.join("copy");
    std::fs::write(copy.join("new.txt"), "new\n").unwrap();
    git_in(&copy, &["add", "new.txt"]);
    git_in(&copy, &["commit", "--quiet", "-m", "try to push"]);
    let push = git(&copy, &["push", "--quiet", "origin", "main"]).await;
    assert!(!push.status.success());

    // The archive still holds the source's main.
    let dir =
        git_archive::repo_url::archive_dir(&t.state.config.data_dir, "local", "team", "project");
    assert_eq!(
        git_in(&dir, &["rev-parse", "refs/heads/main"]),
        git_in(&source, &["rev-parse", "main"])
    );

    let auth = basic(USER, PASSWORD);
    for (method, uri) in [
        (
            "GET",
            format!("{REPO_PATH}/info/refs?service=git-receive-pack"),
        ),
        ("POST", format!("{REPO_PATH}/git-receive-pack")),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(&uri)
            .header(header::AUTHORIZATION, &auth)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            send(&t.app, request).await.status(),
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn unknown_repositories_and_other_files_are_not_found() {
    let (t, _) = archived().await;
    let auth = basic(USER, PASSWORD);
    for uri in [
        "/git/local/team/other.git/info/refs?service=git-upload-pack",
        "/git/local/../team/project.git/info/refs?service=git-upload-pack",
        "/git/local/team/project.git/HEAD",
        "/git/local/team/project.git/objects/info/packs",
    ] {
        let request = Request::get(uri)
            .header(header::AUTHORIZATION, &auth)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            send(&t.app, request).await.status(),
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    let ok = send(&t.app, advertise_request(Some(&auth))).await;
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(
        header_value(&ok, header::CONTENT_TYPE),
        "application/x-git-upload-pack-advertisement"
    );
}

#[tokio::test]
async fn repeated_wrong_credentials_lock_the_client_out() {
    let (t, _) = archived().await;
    let wrong = basic(USER, "wrong");
    for _ in 0..MAX_FAILED_LOGINS {
        let response = send(&t.app, advertise_request(Some(&wrong))).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    // Locked now, even with the right password.
    let right = basic(USER, PASSWORD);
    let response = send(&t.app, advertise_request(Some(&right))).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

async fn download(t: &TestApp, uri: &str) -> (String, Vec<u8>) {
    let response = t.get(uri).await;
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    let disposition = header_value(&response, header::CONTENT_DISPOSITION);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (disposition, bytes.to_vec())
}

/// Unpacks an archive file with a system tool into `out`.
fn unpack(tool: &[&str], file: &Path, out: &Path) {
    std::fs::create_dir_all(out).unwrap();
    let status = Command::new(tool[0])
        .args(&tool[1..])
        .arg(file)
        .current_dir(out)
        .status()
        .unwrap();
    assert!(status.success(), "{tool:?}");
}

#[tokio::test]
async fn zip_and_tar_gz_downloads_hold_the_tree() {
    let (t, _) = archived().await;
    let work = t.dir.path();

    let (disposition, zip) = download(&t, "/repos/1/archive?format=zip").await;
    assert_eq!(disposition, "attachment; filename=\"project-main.zip\"");
    assert!(zip.starts_with(b"PK"));
    let zip_file = work.join("project.zip");
    std::fs::write(&zip_file, &zip).unwrap();
    unpack(&["unzip", "-q"], &zip_file, &work.join("zip"));
    let root = work.join("zip/project-main");
    assert_eq!(
        std::fs::read_to_string(root.join("README.md")).unwrap(),
        "# Test\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        "pub fn answer() -> u8 { 42 }\n"
    );

    let (disposition, tgz) = download(&t, "/repos/1/archive?format=tar.gz&ref=v1").await;
    assert_eq!(disposition, "attachment; filename=\"project-v1.tar.gz\"");
    assert!(tgz.starts_with(&[0x1f, 0x8b]));
    let tgz_file = work.join("project.tar.gz");
    std::fs::write(&tgz_file, &tgz).unwrap();
    unpack(&["tar", "-xzf"], &tgz_file, &work.join("tgz"));
    assert!(work.join("tgz/project-v1/src/lib.rs").is_file());

    // Unknown formats and revisions are refused; downloads need a session.
    assert_eq!(
        t.get("/repos/1/archive?format=rar").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        t.get("/repos/1/archive?ref=nope").await.status(),
        StatusCode::NOT_FOUND
    );
    let anonymous = Request::get("/repos/1/archive")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&t.app, anonymous).await.status(),
        StatusCode::SEE_OTHER
    );
}
