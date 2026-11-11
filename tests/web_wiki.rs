//! Wiki pages of an archived local source.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use axum::http::{StatusCode, header};
use common::{TestApp, archive_source, git_in, page, source_repo};
use git_archive::git::mirror;

/// A wiki with a Markdown home page, AsciiDoc, reStructuredText and
/// MediaWiki pages, and an image.
fn wiki_fixture(source: &Path) {
    let wiki = std::path::PathBuf::from(mirror::wiki_url(&source.to_string_lossy(), false));
    std::fs::create_dir(&wiki).unwrap();
    git_in(&wiki, &["init", "--quiet", "--initial-branch=master"]);
    std::fs::write(
        wiki.join("Home.md"),
        "# Welcome\n\nRead [[Getting Started]] or [[the notes|Notes]].\n\n![logo](images/logo.png)\n\n<script>alert(1)</script>\n",
    )
    .unwrap();
    std::fs::write(
        wiki.join("Getting-Started.adoc"),
        "= Getting Started\n\nInstall it *now*.\n\n++++\n<img src=x onerror=alert(2)>\n++++\n\nlink:Reference[the reference]\n",
    )
    .unwrap();
    std::fs::write(
        wiki.join("Reference.rst"),
        "Reference\n=========\n\nAll **options** are listed here.\n",
    )
    .unwrap();
    std::fs::write(
        wiki.join("Notes.mediawiki"),
        "== Notes ==\n'''bold''' <b>tag</b>\n",
    )
    .unwrap();
    std::fs::create_dir(wiki.join("images")).unwrap();
    std::fs::write(wiki.join("images/logo.png"), b"\x89PNG\r\n\x1a\n\0\0").unwrap();
    git_in(&wiki, &["add", "."]);
    git_in(&wiki, &["commit", "--quiet", "-m", "wiki"]);
}

async fn archived(with_wiki: bool) -> TestApp {
    let t = TestApp::new().await;
    let source = source_repo(t.dir.path(), "source");
    if with_wiki {
        wiki_fixture(&source);
    }
    let repo = archive_source(&t, &source, true).await;
    assert_eq!(repo.has_wiki, with_wiki);
    t
}

#[tokio::test]
async fn the_home_page_links_to_the_other_pages() {
    let t = archived(true).await;
    let (status, html) = page(&t, "/repos/1/wiki").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("<h1>Welcome</h1>"), "{html}");
    let rel = "rel=\"noopener noreferrer nofollow\"";
    assert!(
        html.contains(&format!(
            "<a href=\"/repos/1/wiki/Getting-Started\" {rel}>Getting Started</a>"
        )),
        "{html}"
    );
    assert!(html.contains(&format!(
        "<a href=\"/repos/1/wiki/Notes\" {rel}>the notes</a>"
    )));
    assert!(html.contains("src=\"/repos/1/wiki-file?path=images%2Flogo.png\""));
    let content = html.split_once("class=\"markdown\">").unwrap().1;
    let content = content.split_once("</div>").unwrap().0;
    assert!(!content.contains("<script"), "{content}");
    assert!(!content.contains("alert"), "{content}");
    // The page list names every page, the shown one marked.
    for title in ["Getting Started", "Home", "Notes", "Reference"] {
        assert!(
            html.contains(&format!(">{title}</a></li>")),
            "{title} in {html}"
        );
    }
    assert!(html.contains("href=\"/repos/1/wiki/Home\" aria-current=\"page\""));
    // The repository pages show the wiki tab.
    let (_, overview) = page(&t, "/repos/1/tree").await;
    assert!(overview.contains("href=\"/repos/1/wiki\""));
}

#[tokio::test]
async fn asciidoc_and_rst_render_and_mediawiki_shows_as_text() {
    let t = archived(true).await;
    let (status, adoc) = page(&t, "/repos/1/wiki/Getting-Started").await;
    assert_eq!(status, StatusCode::OK);
    assert!(adoc.contains("<strong>now</strong>"), "{adoc}");
    assert!(adoc.contains("href=\"/repos/1/wiki/Reference\""), "{adoc}");
    assert!(!adoc.contains("onerror"));

    // A space or another case finds the same page.
    let (status, same) = page(&t, "/repos/1/wiki/getting%20started").await;
    assert_eq!(status, StatusCode::OK);
    assert!(same.contains("<strong>now</strong>"));

    let (_, rst) = page(&t, "/repos/1/wiki/Reference").await;
    assert!(rst.contains("<strong>options</strong>"), "{rst}");

    let (_, notes) = page(&t, "/repos/1/wiki/Notes").await;
    assert!(
        notes
            .contains("== Notes ==\n&#39;&#39;&#39;bold&#39;&#39;&#39; &#60;b&#62;tag&#60;/b&#62;"),
        "{notes}"
    );
}

#[tokio::test]
async fn files_and_missing_pages() {
    let t = archived(true).await;
    let response = t.get("/repos/1/wiki-file?path=images%2Flogo.png").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
    assert!(
        response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .starts_with("sandbox")
    );

    let (status, _) = page(&t, "/repos/1/wiki/Missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = page(&t, "/repos/1/wiki-file?path=..%2Fsecret").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = page(&t, "/repos/1/wiki-file?path=nope.png").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_repository_without_a_wiki_has_no_wiki_pages() {
    let t = archived(false).await;
    let (_, tree) = page(&t, "/repos/1/tree").await;
    assert!(!tree.contains("/repos/1/wiki"), "{tree}");
    let (status, _) = page(&t, "/repos/1/wiki").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = page(&t, "/repos/1/wiki-file?path=Home.md").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
