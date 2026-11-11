//! The compare page against an archived local source.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use axum::http::StatusCode;
use common::{TestApp, archive_source, git_in, page, source_repo};
use git_archive::web::compare::MAX_COMMITS;

/// A source whose `feature` branch adds two commits to `main`, whose
/// `orphan` branch shares no history, and whose `long` branch adds
/// `MAX_COMMITS + 5` commits.
fn fixture(parent: &Path) -> PathBuf {
    let source = source_repo(parent, "source");
    git_in(&source, &["checkout", "--quiet", "-b", "feature"]);
    std::fs::write(source.join("notes.txt"), "first line\n").unwrap();
    git_in(&source, &["add", "notes.txt"]);
    git_in(&source, &["commit", "--quiet", "-m", "Add the notes"]);
    std::fs::write(source.join("README.md"), "# Test\n\nMore text.\n").unwrap();
    git_in(&source, &["commit", "--quiet", "-am", "Extend the readme"]);
    git_in(&source, &["checkout", "--quiet", "--orphan", "orphan"]);
    git_in(&source, &["commit", "--quiet", "-m", "Unrelated start"]);
    git_in(&source, &["checkout", "--quiet", "main"]);
    import_commits(&source, "refs/heads/long", MAX_COMMITS + 5);
    // A diff larger than the page shows.
    git_in(&source, &["checkout", "--quiet", "-b", "big"]);
    let line = "a line of a large generated file\n";
    std::fs::write(source.join("big.txt"), line.repeat(100_000)).unwrap();
    git_in(&source, &["add", "big.txt"]);
    git_in(&source, &["commit", "--quiet", "-m", "Add a large file"]);
    git_in(&source, &["checkout", "--quiet", "main"]);
    source
}

/// Adds `count` commits on top of `main` as `branch`, in one fast-import.
fn import_commits(source: &Path, branch: &str, count: usize) {
    let mut stream = String::new();
    for i in 0..count {
        let parent = if i == 0 {
            "from refs/heads/main\n".to_owned()
        } else {
            String::new()
        };
        let message = format!("step {i}");
        stream.push_str(&format!(
            "commit {branch}\ncommitter T <t@example.com> {} +0000\ndata {}\n{message}\n{parent}M 644 inline step.txt\ndata {}\n{i}\n\n",
            1_700_000_000 + i,
            message.len(),
            i.to_string().len() + 1,
        ));
    }
    let mut child = Command::new("git")
        .current_dir(source)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stream.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

async fn archived() -> TestApp {
    let t = TestApp::new().await;
    let source = fixture(t.dir.path());
    archive_source(&t, &source, false).await;
    t
}

#[tokio::test]
async fn a_branch_is_compared_with_the_default_branch() {
    let t = archived().await;
    // Without a second revision only the form shows, with the names to pick.
    let (status, form) = page(&t, "/repos/1/compare").await;
    assert_eq!(status, StatusCode::OK);
    assert!(form.contains("name=\"base\" value=\"main\""), "{form}");
    assert!(form.contains("<option value=\"feature\">"));
    assert!(!form.contains("class=\"diff\""));

    let (status, html) = page(&t, "/repos/1/compare?base=main&head=feature").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("2 commit"), "{html}");
    assert!(html.contains("Add the notes"));
    assert!(html.contains("Extend the readme"));
    assert!(html.contains("2 dosya değişti"));
    assert!(html.contains("<strong>notes.txt</strong>"));
    assert!(html.contains("dl-add\">More text."));
    assert!(html.contains("href=\"/repos/1/compare?base=feature&#38;head=main\""));

    // The other way round main adds nothing.
    let (_, back) = page(&t, "/repos/1/compare?base=feature&head=main").await;
    assert!(
        back.contains("İkinci revizyonda birincide olmayan commit yok."),
        "{back}"
    );
    assert!(back.contains("0 dosya değişti"));
}

#[tokio::test]
async fn the_same_commit_twice_has_no_difference() {
    let t = archived().await;
    let main = git_in(&t.dir.path().join("source"), &["rev-parse", "main"]);
    let (status, html) = page(&t, &format!("/repos/1/compare?base=main&head={main}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Fark yok."), "{html}");
}

#[tokio::test]
async fn unknown_or_unsafe_revisions_are_reported() {
    let t = archived().await;
    let (status, html) = page(&t, "/repos/1/compare?base=main&head=nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        html.contains("Arşivde bu adla bir commit yok: nope"),
        "{html}"
    );

    // An option as a revision is never passed to git.
    let target = t.dir.path().join("written");
    let option = format!("--output={}", target.display());
    let uri = format!(
        "/repos/1/compare?base={}&head=main..feature",
        common::urlencode(&option)
    );
    let (status, html) = page(&t, &uri).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(html.contains(&format!("{option}, main..feature")), "{html}");
    assert!(!target.exists());

    let missing = t.get("/repos/9/compare").await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn long_lists_are_cut_and_unrelated_histories_still_compare() {
    let t = archived().await;
    let (_, html) = page(&t, "/repos/1/compare?base=main&head=long").await;
    let total = MAX_COMMITS + 5;
    assert!(html.contains(&format!("{total} commit")), "{html}");
    assert!(html.contains(&format!(
        "Yalnız en yeni {MAX_COMMITS} commit listeleniyor."
    )));
    assert_eq!(
        html.matches("class=\"commit-subject\"").count(),
        MAX_COMMITS
    );
    assert!(html.contains(&format!("step {}", total - 1)));
    assert!(!html.contains("step 4<"));

    let (status, big) = page(&t, "/repos/1/compare?base=main&head=big").await;
    assert_eq!(status, StatusCode::OK);
    assert!(big.contains("Diff çok büyük olduğu için yalnız bir kısmı gösteriliyor."));
    assert!(big.len() < 4 * 1024 * 1024, "{}", big.len());

    // No merge base: the diff runs against the first revision itself.
    let (status, html) = page(&t, "/repos/1/compare?base=main&head=orphan").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Unrelated start"), "{html}");
    assert!(html.contains("<strong>README.md</strong>"));
}
