//! Browsing pages against an archived fixture repository: files, raw
//! files, history, commits, refs and blame, with hostile content.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};

use axum::http::{StatusCode, header};
use common::{TestApp, body_text, git_in, header_value};
use git_archive::db::repos::{self, NewRepo, RepoStatus};
use git_archive::db::unix_now;

const README: &str = "# Project\n\n<script>alert('readme')</script>\n\n![logo](img/logo.png) [guide](docs/guide.md)\n";
const HOSTILE_MESSAGE: &str = "say hello <script>x</script>";

/// The text as askama escapes it in a page.
fn esc(text: &str) -> String {
    text.replace('<', "&#60;")
        .replace('>', "&#62;")
        .replace('"', "&#34;")
        .replace('\'', "&#39;")
}

/// A source repository with a README, code, a binary file, an HTML file,
/// a file with markup in its name, a tag and three commits.
fn fixture(parent: &Path) -> PathBuf {
    let path = parent.join("source");
    std::fs::create_dir_all(path.join("img")).unwrap();
    std::fs::create_dir_all(path.join("docs")).unwrap();
    git_in(&path, &["init", "--quiet", "--initial-branch=main"]);
    let files: [(&str, &[u8]); 6] = [
        ("README.md", README.as_bytes()),
        ("main.rs", b"fn main() {\n    println!(\"hi\");\n}\n"),
        ("img/logo.png", &[0x89, b'P', b'N', b'G', 0, 1, 2]),
        ("page.html", b"<html><script>alert(1)</script></html>\n"),
        ("<b>x.txt", b"tag in name\n"),
        ("docs/guide.md", b"guide\n"),
    ];
    for (name, content) in files {
        std::fs::write(path.join(name), content).unwrap();
    }
    git_in(&path, &["add", "."]);
    git_in(&path, &["commit", "--quiet", "-m", "initial import"]);
    git_in(&path, &["tag", "v1"]);
    std::fs::write(
        path.join("main.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    )
    .unwrap();
    git_in(&path, &["commit", "--quiet", "-am", HOSTILE_MESSAGE]);
    std::fs::write(path.join("docs/guide.md"), "guide v2\n").unwrap();
    git_in(&path, &["commit", "--quiet", "-am", "Update the guide"]);
    path
}

/// Archives the fixture through a real sync and returns the app.
async fn archived() -> (TestApp, PathBuf) {
    let t = TestApp::new().await;
    let source = fixture(t.dir.path());
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

async fn page(t: &TestApp, uri: &str) -> String {
    let response = t.get(uri).await;
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    body_text(response).await
}

fn archive_dir(t: &TestApp) -> PathBuf {
    git_archive::repo_url::archive_dir(&t.state.config.data_dir, "local", "team", "project")
}

/// The bytes of `name` in a tar.gz archive, read with the system `tar`.
fn tar_gz_member(dir: &Path, archive: &[u8], name: &str) -> Vec<u8> {
    let file = dir.join("download.tar.gz");
    std::fs::write(&file, archive).unwrap();
    let output = std::process::Command::new("tar")
        .args(["-xzOf", file.to_str().unwrap(), name])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

async fn body_bytes(response: axum::http::Response<axum::body::Body>) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn lfs_files_show_their_content_only_with_lfs_on() {
    let t = TestApp::new().await;
    let content = "large text line\n".repeat(300);
    let source = common::lfs_source_repo(t.dir.path(), "lfs-source", content.as_bytes());
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

    // LFS off: the archive holds the pointer file, and shows it as it is.
    let pointer = page(&t, "/repos/1/blob?path=data.bin").await;
    assert!(pointer.contains("version https://git-lfs.github.com/spec/v1"));
    assert!(!archive_dir(&t).join("lfs").exists());

    t.post("/repos/1/settings", "wiki=1&lfs=1").await;
    assert!(
        repos::get(&t.state.pool, 1)
            .await
            .unwrap()
            .unwrap()
            .lfs_enabled
    );
    repos::schedule_now(&t.state.pool, 1, unix_now())
        .await
        .unwrap();
    sync(&t).await;
    let synced = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(synced.lfs_size_bytes, content.len() as i64);
    assert!(page(&t, "/repos/1").await.contains("LFS:"));

    let file = page(&t, "/repos/1/blob?path=data.bin").await;
    assert!(file.contains("large text line"), "{file}");
    assert!(!file.contains("git-lfs.github.com"));
    assert!(file.contains("300 satır"));

    let raw = t.get("/repos/1/raw?path=data.bin").await;
    assert_eq!(
        header_value(&raw, header::CONTENT_LENGTH),
        content.len().to_string()
    );
    assert_eq!(body_bytes(raw).await, content.as_bytes());

    let download = t.get("/repos/1/archive?format=tar.gz").await;
    assert_eq!(download.status(), StatusCode::OK);
    let archive = body_bytes(download).await;
    let member = tar_gz_member(t.dir.path(), &archive, "project-main/data.bin");
    assert_eq!(member, content.as_bytes());

    // A missing object is never downloaded during a request: the pointer
    // stays in the download, which still succeeds.
    std::fs::remove_dir_all(archive_dir(&t).join("lfs").join("objects")).unwrap();
    let download = t.get("/repos/1/archive?format=tar.gz").await;
    let archive = body_bytes(download).await;
    let member = tar_gz_member(t.dir.path(), &archive, "project-main/data.bin");
    assert!(String::from_utf8_lossy(&member).starts_with("version https://git-lfs"));
    let pointer = page(&t, "/repos/1/blob?path=data.bin").await;
    assert!(pointer.contains("git-lfs.github.com"));
}

#[tokio::test]
async fn an_lfs_object_the_source_lacks_fails_the_sync() {
    let t = TestApp::new().await;
    let source = common::lfs_source_repo(t.dir.path(), "lfs-source", b"content\n");
    std::fs::remove_dir_all(source.join(".git").join("lfs")).unwrap();
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
    t.post("/repos/1/settings", "lfs=1").await;
    for handle in t.state.scheduler.tick().await.unwrap() {
        handle.await.unwrap();
    }
    let failed = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(failed.status, RepoStatus::Error);
    let message = failed.last_error.unwrap();
    assert!(message.starts_with("LFS: git lfs: "), "{message}");
    // The commits themselves are archived.
    assert!(archive_dir(&t).join("HEAD").is_file());
}

#[tokio::test]
async fn tree_lists_files_and_renders_a_safe_readme() {
    let (t, _) = archived().await;
    let html = page(&t, "/repos/1/tree").await;
    assert!(html.contains("href=\"/repos/1/tree?path=docs\""), "{html}");
    assert!(html.contains("href=\"/repos/1/blob?path=main.rs\""));
    // Markup in a file name is text, never an element.
    assert!(html.contains(&esc("<b>x.txt")), "{html}");
    assert!(!html.contains("<b>x.txt"));
    // The README renders, without its script, with links into the archive.
    assert!(html.contains("<h1>Project</h1>"));
    assert!(!html.contains("alert("), "{html}");
    assert!(
        html.contains("src=\"/repos/1/raw?path=img%2Flogo.png\""),
        "{html}"
    );
    assert!(html.contains("href=\"/repos/1/blob?path=docs%2Fguide.md\""));

    let docs = page(&t, "/repos/1/tree?path=docs").await;
    assert!(docs.contains("guide.md"));
    // A file path on the tree page goes to the file page, and back.
    let to_file = t.get("/repos/1/tree?path=main.rs").await;
    assert_eq!(to_file.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        header_value(&to_file, header::LOCATION),
        "/repos/1/blob?path=main.rs"
    );
    let to_root = t.get("/repos/1/blob?path=").await;
    assert_eq!(header_value(&to_root, header::LOCATION), "/repos/1/tree");
}

#[tokio::test]
async fn file_page_highlights_code_and_handles_binary_files() {
    let (t, _) = archived().await;
    let code = page(&t, "/repos/1/blob?path=main.rs").await;
    assert!(code.contains("hl-"), "{code}");
    assert!(code.contains("3 satır"));
    assert!(code.contains("id=\"L3\""));
    assert!(code.contains("hello"));

    // The highlighter splits the tag into spans; it never emits it whole.
    let html_file = page(&t, "/repos/1/blob?path=page.html").await;
    assert!(html_file.contains("&lt;"), "{html_file}");
    assert!(!html_file.contains("<script>alert(1)"));
    assert!(!html_file.contains("<html>"));

    let image = page(&t, "/repos/1/blob?path=img%2Flogo.png").await;
    assert!(image.contains("<img src=\"/repos/1/raw?path=img%2Flogo.png\""));

    // The first commit had the old text.
    let old = page(&t, "/repos/1/blob?ref=v1&path=main.rs").await;
    assert!(old.contains("hi") && !old.contains("hello"));
}

#[tokio::test]
async fn raw_files_never_run_as_pages() {
    let (t, _) = archived().await;
    let text = t.get("/repos/1/raw?path=page.html").await;
    assert_eq!(text.status(), StatusCode::OK);
    assert_eq!(
        header_value(&text, header::CONTENT_TYPE),
        "text/plain; charset=utf-8"
    );
    assert!(header_value(&text, header::CONTENT_SECURITY_POLICY).starts_with("sandbox"));
    assert_eq!(
        header_value(&text, header::X_CONTENT_TYPE_OPTIONS),
        "nosniff"
    );
    assert_eq!(
        body_text(text).await,
        "<html><script>alert(1)</script></html>\n"
    );

    let image = t.get("/repos/1/raw?path=img/logo.png").await;
    assert_eq!(header_value(&image, header::CONTENT_TYPE), "image/png");

    let readme = t.get("/repos/1/raw?ref=v1&path=README.md").await;
    assert_eq!(body_text(readme).await, README);
}

#[tokio::test]
async fn history_search_and_commit_diff() {
    let (t, _) = archived().await;
    let list = page(&t, "/repos/1/commits").await;
    assert!(list.contains("Update the guide"));
    assert!(list.contains("initial import"));
    // The message is text: the script tag stays escaped.
    assert!(list.contains(&esc(HOSTILE_MESSAGE)), "{list}");
    assert!(!list.contains("<script>x</script>"));

    let found = page(&t, "/repos/1/commits?q=GUIDE").await;
    assert!(found.contains("Update the guide") && !found.contains("initial import"));
    // A page that is not a number shows the first page, as on other lists.
    for uri in ["/repos/1/commits?page=x", "/repos/1/tree?page=-1"] {
        assert_eq!(t.get(uri).await.status(), StatusCode::OK, "{uri}");
    }
    let none = page(&t, "/repos/1/commits?q=nothing-matches").await;
    assert!(none.contains("Eşleşen commit yok."));

    let dir = archive_dir(&t);
    let head = git_in(&dir, &["rev-parse", "main"]);
    let commit = page(&t, &format!("/repos/1/commit/{head}")).await;
    assert!(commit.contains("Update the guide"));
    assert!(commit.contains("docs/guide.md"), "{commit}");
    assert!(commit.contains("dl dl-del\">guide</span>"), "{commit}");
    assert!(commit.contains("dl dl-add\">guide v2</span>"));
    assert!(commit.contains("1 dosya değişti"));

    // The root commit shows every file as added.
    let root = git_in(&dir, &["rev-list", "--max-parents=0", "main"]);
    let first = page(&t, &format!("/repos/1/commit/{root}")).await;
    assert!(first.contains("6 dosya değişti"), "{first}");
    assert!(first.contains("Eklendi"));
}

#[tokio::test]
async fn refs_page_lists_branches_tags_and_rescued_history() {
    let (t, source) = archived().await;
    // Rewrite the last commit at the source; the next sync rescues the old one.
    git_in(
        &source,
        &["commit", "--quiet", "--amend", "-m", "Rewritten guide"],
    );
    repos::schedule_now(&t.state.pool, 1, unix_now())
        .await
        .unwrap();
    sync(&t).await;

    let html = page(&t, "/repos/1/refs").await;
    assert!(html.contains(&esc("Branch'ler")), "{html}");
    assert!(html.contains(">main</a>"));
    assert!(html.contains("Varsayılan"));
    assert!(html.contains(">v1</a>"));
    assert!(html.contains("Yeniden yazılmış geçmiş"));
    assert!(html.contains("/heads/main</a>"), "{html}");
    // The rescued commit stays listed next to the rewritten one.
    assert!(html.contains("Update the guide"));
    assert!(html.contains("Rewritten guide"));
}

#[tokio::test]
async fn blame_names_the_commit_of_each_line() {
    let (t, _) = archived().await;
    let html = page(&t, "/repos/1/blame?path=main.rs").await;
    assert!(html.contains(&esc(HOSTILE_MESSAGE)), "{html}");
    assert!(html.contains("initial import"));
    assert!(html.contains(&esc("println!(\"hello\");")));
    let binary = page(&t, "/repos/1/blame?path=img/logo.png").await;
    assert!(binary.contains("blame gösterilemez"));
}

#[tokio::test]
async fn unsafe_or_unknown_input_is_not_found() {
    let (t, _) = archived().await;
    for uri in [
        "/repos/1/tree?ref=--output%3D%2Ftmp%2Fx",
        "/repos/1/tree?ref=main..v1",
        "/repos/1/tree?ref=no-such-branch",
        "/repos/1/tree?path=..%2F..%2Fetc",
        "/repos/1/blob?path=missing.txt",
        "/repos/1/raw?path=docs",
        "/repos/1/commit/not-a-sha",
        "/repos/1/commit/0000000000000000000000000000000000000000",
        "/repos/9/tree",
    ] {
        let status = t.get(uri).await.status();
        assert!(
            status == StatusCode::NOT_FOUND
                || (uri.contains("raw") && status == StatusCode::SEE_OTHER),
            "{uri}: {status}"
        );
    }
}

#[tokio::test]
async fn the_preference_form_returns_to_the_same_file() {
    let (t, _) = archived().await;
    let html = page(&t, "/repos/1/blob?path=main.rs").await;
    assert!(
        html.contains("name=\"back\" value=\"/repos/1/blob?path=main.rs\""),
        "{html}"
    );
}

#[tokio::test]
async fn a_nul_byte_in_the_input_is_never_a_server_error() {
    let (t, _) = archived().await;
    // git takes no NUL byte in an argument.
    let search = t.get("/repos/1/commits?q=gu%00ide").await.status();
    assert_eq!(search, StatusCode::BAD_REQUEST);
    for uri in [
        "/repos/1/tree?path=docs%00x",
        "/repos/1/blob?path=main.rs%00x",
        "/repos/1/blame?path=main.rs%00x",
        "/repos/1/raw?path=main.rs%00x",
        "/repos/1/tree?ref=main%00x",
        "/repos/1/history?path=main.rs%00x",
    ] {
        let status = t.get(uri).await.status();
        assert!(!status.is_server_error(), "{uri}: {status}");
    }
}

#[tokio::test]
async fn a_repository_without_archive_shows_an_empty_state() {
    let t = TestApp::new().await;
    t.post("/repos", "url=https%3A%2F%2Fgithub.com%2Fa%2Fb")
        .await;
    for uri in ["/repos/1/tree", "/repos/1/commits", "/repos/1/refs"] {
        let html = page(&t, uri).await;
        assert!(html.contains("Arşivde henüz commit yok."), "{uri}");
    }
    let overview = page(&t, "/repos/1").await;
    assert!(overview.contains("href=\"/repos/1/tree\""));
}
