//! The ref history: events of real syncs from a local source, refs deleted
//! at the source, the run detail and the history page.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use common::{TestApp, body_text, git_in, source_repo};
use git_archive::db::repos::{self, NewRepo, RepoStatus};
use git_archive::db::unix_now;
use git_archive::db::usage::DAY_SECONDS;

/// A source with `main`, `feature` (one commit ahead), `gone`, `wip-1`,
/// the tag `v1` and a pull request ref, archived with `refs/heads/wip-*`
/// left out of the history.
async fn setup() -> (TestApp, PathBuf) {
    let t = TestApp::new().await;
    let source = source_repo(t.dir.path(), "source");
    git_in(&source, &["tag", "v1"]);
    for branch in ["feature", "gone", "wip-1"] {
        git_in(&source, &["branch", branch]);
    }
    commit_on(&source, "feature", "feature work");
    git_in(&source, &["update-ref", "refs/pull/1/head", "feature"]);
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
    sqlx::query("UPDATE repos SET ref_exclude = 'refs/heads/wip-*'")
        .execute(&t.state.pool)
        .await
        .unwrap();
    sync(&t).await;
    (t, source)
}

fn commit_on(source: &Path, branch: &str, message: &str) -> String {
    git_in(source, &["checkout", "--quiet", branch]);
    git_in(
        source,
        &["commit", "--quiet", "--allow-empty", "-m", message],
    );
    git_in(source, &["checkout", "--quiet", "main"]);
    git_in(source, &["rev-parse", branch])
}

async fn sync(t: &TestApp) {
    repos::schedule_now(&t.state.pool, 1, unix_now())
        .await
        .unwrap();
    for handle in t.state.scheduler.tick().await.unwrap() {
        handle.await.unwrap();
    }
    let repo = repos::get(&t.state.pool, 1).await.unwrap().unwrap();
    assert_eq!(repo.status, RepoStatus::Ok, "{:?}", repo.last_error);
}

/// `(ref, kind, old_oid, new_oid, run_id)` of an event.
type EventRow = (String, String, Option<String>, Option<String>, Option<i64>);

/// Every event, oldest first.
async fn rows(t: &TestApp) -> Vec<EventRow> {
    sqlx::query_as("SELECT ref, kind, old_oid, new_oid, run_id FROM ref_events ORDER BY id")
        .fetch_all(&t.state.pool)
        .await
        .unwrap()
}

/// The events after the first `skip`, as `(ref, kind)`.
async fn new_events(t: &TestApp, skip: usize) -> Vec<(String, String)> {
    rows(t)
        .await
        .into_iter()
        .skip(skip)
        .map(|(name, kind, ..)| (name, kind))
        .collect()
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

async fn page(t: &TestApp, uri: &str) -> String {
    let response = t.get(uri).await;
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    body_text(response).await
}

#[tokio::test]
async fn syncs_record_branch_and_tag_changes_only() {
    let (t, source) = setup().await;
    let first = rows(&t).await;
    let baseline: Vec<_> = first
        .iter()
        .map(|(n, k, _, _, r)| (n.as_str(), k.as_str(), *r))
        .collect();
    // A baseline per branch and tag of the first sync; no pull or excluded ref.
    assert_eq!(
        baseline,
        [
            ("refs/heads/feature", "baseline", Some(1)),
            ("refs/heads/gone", "baseline", Some(1)),
            ("refs/heads/main", "baseline", Some(1)),
            ("refs/tags/v1", "baseline", Some(1)),
        ]
    );

    commit_on(&source, "main", "next");
    git_in(&source, &["branch", "new"]);
    // Force-push feature: its commit is replaced by another one.
    git_in(&source, &["branch", "--force", "feature", "main~1"]);
    commit_on(&source, "feature", "redone");
    git_in(&source, &["branch", "--delete", "--force", "gone"]);
    commit_on(&source, "wip-1", "excluded work");
    git_in(&source, &["update-ref", "refs/pull/1/head", "main"]);
    sync(&t).await;
    assert_eq!(
        new_events(&t, 4).await,
        pairs(&[
            ("refs/heads/feature", "rewritten"),
            ("refs/heads/main", "moved"),
            ("refs/heads/new", "created"),
            ("refs/heads/gone", "deleted_at_source"),
        ])
    );

    let refs = page(&t, "/repos/1/refs").await;
    let gone = refs.find(">gone</a>").unwrap();
    let badge = refs.find("Kaynakta silindi").unwrap();
    assert!(badge > gone && badge - gone < 200, "{refs}");
    let detail = page(&t, "/repos/1").await;
    assert!(detail.contains("4 ref değişikliği"), "{detail}");
    assert!(detail.contains("Yeniden yazıldı"));

    // The branch comes back at the source.
    git_in(&source, &["branch", "gone", "main"]);
    sync(&t).await;
    assert_eq!(
        new_events(&t, 8).await,
        pairs(&[
            ("refs/heads/gone", "moved"),
            ("refs/heads/gone", "back_at_source"),
        ])
    );
    assert!(!page(&t, "/repos/1/refs").await.contains("Kaynakta silindi"));

    // A sync without changes records nothing.
    sync(&t).await;
    assert_eq!(rows(&t).await.len(), 10);
}

#[tokio::test]
async fn history_page_shows_a_past_day() {
    let (t, source) = setup().await;
    let old_main = git_in(&source, &["rev-parse", "main"]);
    // The baseline happened ten days ago.
    sqlx::query("UPDATE ref_events SET at = at - ?")
        .bind(10 * DAY_SECONDS)
        .execute(&t.state.pool)
        .await
        .unwrap();
    let new_main = commit_on(&source, "main", "today");
    sync(&t).await;

    let day = |days_ago: i64| git_archive::web::format::date(unix_now() - days_ago * DAY_SECONDS);
    let past = page(&t, &format!("/repos/1/history?at={}", day(5))).await;
    assert!(
        past.contains(&format!("/repos/1/commit/{old_main}")),
        "{past}"
    );
    assert!(!past.contains(&format!("/repos/1/commit/{new_main}")));
    assert!(!past.contains("alert-warn"));

    let today = page(&t, "/repos/1/history").await;
    assert!(today.contains(&format!("/repos/1/commit/{new_main}")));
    // A page past the last one shows the last page; a huge number does
    // not overflow the offset.
    let far = page(&t, "/repos/1/history?page=200000000000000000").await;
    assert!(
        far.contains(&format!("/repos/1/commit/{new_main}")),
        "{far}"
    );
    // The number stays only in the return path of the preference form; no
    // pager link names it.
    assert!(far.contains("name=\"back\" value=\"/repos/1/history?page=200000000000000000\""));
    assert_eq!(far.matches("200000000000000000").count(), 1, "{far}");
    assert!(today.contains("İlerledi"));
    assert!(today.contains(&format!("value=\"{}\"", day(0))));

    // Before the recording nothing is known and the page says so.
    let before = page(&t, &format!("/repos/1/history?at={}", day(20))).await;
    assert!(before.contains("alert-warn"), "{before}");
    assert!(before.contains("Bu tarih için bilinen branch veya tag yok."));

    let bad = t.get("/repos/1/history?at=2025-02-30").await;
    assert_eq!(bad.status(), StatusCode::NOT_FOUND);
    let missing = t.get("/repos/9/history").await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn annotated_tags_record_their_commit() {
    let (t, source) = setup().await;
    let first = git_in(&source, &["rev-parse", "main"]);
    git_in(&source, &["tag", "--annotate", "-m", "release", "v2"]);
    sync(&t).await;
    // The tag moves to a new commit: both ids are commits, not tag objects.
    let second = commit_on(&source, "main", "next");
    git_in(
        &source,
        &["tag", "--force", "--annotate", "-m", "again", "v2"],
    );
    sync(&t).await;
    // The tag is deleted at the source.
    git_in(&source, &["tag", "--delete", "v2"]);
    sync(&t).await;

    let tag: Vec<_> = rows(&t)
        .await
        .into_iter()
        .filter(|row| row.0 == "refs/tags/v2")
        .map(|(_, _, old, new, _)| (old, new))
        .collect();
    let some = |id: &str| Some(id.to_owned());
    assert_eq!(
        tag,
        [
            (None, some(&first)),
            (some(&first), some(&second)),
            (None, some(&second)),
        ]
    );
}

#[tokio::test]
async fn an_archive_without_history_starts_from_its_kept_commits() {
    let (t, source) = setup().await;
    let old_main = git_in(&source, &["rev-parse", "main"]);
    git_in(
        &source,
        &["commit", "--quiet", "--amend", "-m", "rewritten"],
    );
    sync(&t).await;
    // An archive from before the history: kept commits, no events.
    sqlx::query("DELETE FROM ref_events")
        .execute(&t.state.pool)
        .await
        .unwrap();
    sync(&t).await;

    let started = rows(&t).await;
    assert_eq!(started.len(), 5, "{started:?}");
    let (name, kind, old, new, run) = &started[0];
    assert_eq!(
        (name.as_str(), kind.as_str()),
        ("refs/heads/main", "rewritten")
    );
    assert_eq!(old.as_deref(), Some(old_main.as_str()));
    assert_eq!((new, run), (&None, &None));
    assert!(started[1..].iter().all(|r| r.1 == "baseline"));

    // The next sync does not fill it again.
    sync(&t).await;
    assert_eq!(rows(&t).await.len(), 5);

    let yesterday = git_archive::web::format::date(unix_now() - DAY_SECONDS);
    let html = page(&t, &format!("/repos/1/history?at={yesterday}")).await;
    assert!(html.contains("alert-warn"), "{html}");
    assert!(html.contains("Bu tarihten sonra yeniden yazılan ref"));
    assert!(html.contains(&format!("/repos/1/commit/{old_main}")));
}
