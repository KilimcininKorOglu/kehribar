//! Code search against an archived local source.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use common::{TestApp, archive_source, git_in, page, source_repo};
use git_archive::web::search::MAX_MATCHES;

/// A source with mixed-case text, a binary file, an option-like line and
/// a file with more matching lines than the page shows. The `old` branch
/// keeps the first version of `src/main.rs`.
fn fixture(parent: &Path) -> PathBuf {
    let source = source_repo(parent, "source");
    std::fs::create_dir(source.join("src")).unwrap();
    std::fs::write(
        source.join("src/main.rs"),
        "fn main() {\n    println!(\"Hello World\");\n}\n",
    )
    .unwrap();
    git_in(&source, &["add", "src/main.rs"]);
    git_in(&source, &["commit", "--quiet", "-m", "Add main"]);
    git_in(&source, &["branch", "old"]);
    std::fs::write(
        source.join("src/main.rs"),
        "fn main() {\n    println!(\"Hello World\");\n    println!(\"hello world\");\n    let flag = \"--foo\";\n    let year = 2026;\n}\n",
    )
    .unwrap();
    let mut binary = b"Hello World\0".to_vec();
    binary.extend_from_slice(&[0, 159, 146, 150]);
    std::fs::write(source.join("logo.bin"), binary).unwrap();
    std::fs::write(source.join("many.txt"), "needle\n".repeat(MAX_MATCHES + 20)).unwrap();
    git_in(&source, &["add", "src/main.rs", "logo.bin", "many.txt"]);
    git_in(&source, &["commit", "--quiet", "-m", "Grow"]);
    source
}

async fn archived() -> TestApp {
    let t = TestApp::new().await;
    let source = fixture(t.dir.path());
    archive_source(&t, &source, false).await;
    t
}

#[tokio::test]
async fn fixed_text_matches_any_case_and_links_to_the_line() {
    let t = archived().await;
    let (status, html) = page(&t, "/repos/1/search?q=hello+world").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("1 dosya içinde 2 eşleşen satır."), "{html}");
    assert!(html.contains("<strong>src/main.rs</strong>"));
    assert!(html.contains(
        "href=\"/repos/1/blob?path=src%2Fmain.rs#L2\"><span class=\"search-number\">2</span>"
    ));
    assert!(html.contains("search-number\">3</span>"));
    // The binary file holds the text too, but is skipped.
    assert!(!html.contains("logo.bin"));

    let (_, exact) = page(&t, "/repos/1/search?q=hello+world&case=1").await;
    assert!(exact.contains("1 dosya içinde 1 eşleşen satır."), "{exact}");
    assert!(exact.contains("search-number\">3</span>"));
    assert!(!exact.contains("search-number\">2</span>"));
}

#[tokio::test]
async fn a_revision_and_a_regular_expression_narrow_the_search() {
    let t = archived().await;
    let (_, regex) = page(&t, "/repos/1/search?q=year+%3D+%5B0-9%5D%7B4%7D&regex=1").await;
    assert!(regex.contains("search-number\">5</span>"), "{regex}");

    // Without the regex option the same text is matched literally.
    let (_, literal) = page(&t, "/repos/1/search?q=year+%3D+%5B0-9%5D%7B4%7D").await;
    assert!(literal.contains("Eşleşen satır yok."), "{literal}");
    assert!(!literal.contains("0 eşleşen satır"));

    let (status, old) = page(&t, "/repos/1/search?q=hello&ref=old").await;
    assert_eq!(status, StatusCode::OK);
    assert!(old.contains("1 dosya içinde 1 eşleşen satır."), "{old}");
    assert!(old.contains("href=\"/repos/1/blob?ref=old&#38;path=src%2Fmain.rs#L2\""));
}

#[tokio::test]
async fn long_results_are_cut() {
    let t = archived().await;
    let (_, html) = page(&t, "/repos/1/search?q=needle").await;
    assert!(
        html.contains(&format!("1 dosya içinde {MAX_MATCHES} eşleşen satır.")),
        "{html}"
    );
    assert!(html.contains(&format!(
        "Yalnız ilk {MAX_MATCHES} eşleşen satır gösteriliyor."
    )));
    assert_eq!(html.matches("class=\"search-line\"").count(), MAX_MATCHES);
}

#[tokio::test]
async fn option_like_queries_and_bad_input_are_safe() {
    let t = archived().await;
    // A query that looks like an option is searched as text.
    let (status, html) = page(&t, "/repos/1/search?q=--foo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("search-number\">4</span>"), "{html}");

    // A revision that looks like an option is never passed to git.
    let target = t.dir.path().join("written");
    let option = format!("--output={}", target.display());
    let uri = format!("/repos/1/search?q=hello&ref={}", common::urlencode(&option));
    let (status, html) = page(&t, &uri).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(html.contains("Arşivde bu adla bir commit yok."), "{html}");
    assert!(!target.exists());

    let (status, html) = page(&t, "/repos/1/search?q=hello&ref=nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(html.contains("Arşivde bu adla bir commit yok."), "{html}");

    let (status, html) = page(&t, "/repos/1/search?q=%28open&regex=1").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("Düzenli ifade geçersiz."), "{html}");

    // git takes no NUL byte in an argument: a client error, not a crash.
    let (status, html) = page(&t, "/repos/1/search?q=hel%00lo").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{html}");
    assert!(
        html.contains("Arama metni NUL karakteri içeremez."),
        "{html}"
    );

    // A one-character query does not run.
    let (status, html) = page(&t, "/repos/1/search?q=h").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("En az 2 karakter yazın."), "{html}");
    assert!(!html.contains("search-summary"));

    // The empty form runs nothing and warns about nothing.
    let (_, form) = page(&t, "/repos/1/search").await;
    assert!(!form.contains("alert"), "{form}");
    assert!(form.contains("placeholder=\"main\""));

    let missing = t.get("/repos/9/search?q=hello").await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}
