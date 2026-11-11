//! Mirror behaviour against local source repositories, without network.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use git_archive::git::maintenance;
use git_archive::git::mirror::{self, REWRITTEN_REF_PREFIX};
use git_archive::git::{Git, GitErrorKind};
use tempfile::TempDir;

/// A working repository that plays the source server.
struct Source {
    _root: TempDir,
    path: PathBuf,
    archive: PathBuf,
    counter: u32,
}

impl Source {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("project");
        let archive = root.path().join("archive/example.com/team/project.git");
        std::fs::create_dir(&path).unwrap();
        let source = Self {
            _root: root,
            path,
            archive,
            counter: 0,
        };
        source.git(&["init", "--quiet", "--initial-branch=main"]);
        source
    }

    fn url(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    fn git(&self, args: &[&str]) -> String {
        git_in(&self.path, args)
    }

    fn commit(&mut self, message: &str) -> String {
        self.counter += 1;
        std::fs::write(
            self.path.join("file.txt"),
            format!("{message} {}", self.counter),
        )
        .unwrap();
        self.git(&["add", "file.txt"]);
        self.git(&["commit", "--quiet", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    async fn sync(&self, now: i64) -> mirror::SyncReport {
        self.sync_excluding(now, &[]).await
    }

    async fn sync_excluding(&self, now: i64, exclude: &[&str]) -> mirror::SyncReport {
        let exclude: Vec<String> = exclude.iter().map(|p| (*p).to_owned()).collect();
        mirror::sync(&git(), &self.url(), &self.archive, None, now, &exclude)
            .await
            .unwrap()
    }

    fn archive_refs(&self) -> BTreeMap<String, String> {
        refs(&self.archive)
    }
}

fn git() -> Git {
    Git::new(Duration::from_secs(60))
}

fn git_in(dir: &Path, args: &[&str]) -> String {
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

fn refs(dir: &Path) -> BTreeMap<String, String> {
    git_in(dir, &["for-each-ref", "--format=%(refname) %(objectname)"])
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, id)| (name.to_owned(), id.to_owned()))
        .collect()
}

fn without_archive_refs(refs: BTreeMap<String, String>) -> BTreeMap<String, String> {
    refs.into_iter()
        .filter(|(name, _)| !name.starts_with("refs/git-archive/"))
        .collect()
}

#[tokio::test]
async fn git_never_runs_its_own_maintenance_in_the_archive() {
    let mut source = Source::new();
    source.commit("one");
    source.sync(100).await;
    // Background gc after a fetch would run outside the repository lock.
    assert_eq!(git_in(&source.archive, &["config", "gc.auto"]), "0");
    assert_eq!(
        git_in(&source.archive, &["config", "maintenance.auto"]),
        "false"
    );
}

#[tokio::test]
async fn first_sync_copies_every_branch_and_tag() {
    let mut source = Source::new();
    source.commit("one");
    source.git(&["tag", "v1"]);
    source.git(&["tag", "-a", "v1-annotated", "-m", "release"]);
    source.git(&["checkout", "--quiet", "-b", "feature"]);
    source.commit("two");
    source.git(&["checkout", "--quiet", "main"]);

    let report = source.sync(100).await;

    assert_eq!(source.archive_refs(), refs(&source.path));
    assert_eq!(report.refs_changed, 4);
    assert_eq!(report.refs_rescued, 0);
    assert_eq!(report.default_branch.as_deref(), Some("main"));
    assert_eq!(
        git_in(&source.archive, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(
        git_in(&source.archive, &["config", "gc.pruneExpire"]),
        "never"
    );
}

#[tokio::test]
async fn unchanged_source_reports_no_changes() {
    let mut source = Source::new();
    source.commit("one");
    source.sync(100).await;
    let report = source.sync(200).await;
    assert_eq!(report.refs_changed, 0);
    assert_eq!(report.refs_rescued, 0);
}

#[tokio::test]
async fn branch_deleted_at_the_source_stays_in_the_archive() {
    let mut source = Source::new();
    source.commit("one");
    source.git(&["branch", "doomed"]);
    source.sync(100).await;

    source.git(&["branch", "-D", "doomed"]);
    source.commit("two");
    let report = source.sync(200).await;

    assert!(source.archive_refs().contains_key("refs/heads/doomed"));
    assert_eq!(report.refs_changed, 1);
    assert_eq!(report.refs_rescued, 0);
}

#[tokio::test]
async fn a_deleted_branch_does_not_block_a_new_one_inside_its_name() {
    let mut source = Source::new();
    let first = source.commit("one");
    source.git(&["branch", "feature"]);
    source.git(&["branch", "release/1.0"]);
    source.sync(100).await;

    // `feature` becomes `feature/x`, `release/1.0` becomes `release`.
    source.git(&["branch", "-D", "feature", "release/1.0"]);
    source.git(&["branch", "feature/x"]);
    source.git(&["branch", "release"]);
    let report = source.sync(200).await;

    let refs = source.archive_refs();
    assert!(refs.contains_key("refs/heads/feature/x"), "{refs:?}");
    assert!(refs.contains_key("refs/heads/release"), "{refs:?}");
    assert!(!refs.contains_key("refs/heads/feature"));
    assert!(!refs.contains_key("refs/heads/release/1.0"));
    // The old refs are kept and reported as gone from the source.
    let kept = |name: &str| refs[&format!("{REWRITTEN_REF_PREFIX}200/heads/{name}")].clone();
    assert_eq!(
        (kept("feature"), kept("release/1.0")),
        (first.clone(), first)
    );
    let missing: Vec<&String> = report.missing_at_source().map(|(name, _)| name).collect();
    assert_eq!(missing, ["refs/heads/feature", "refs/heads/release/1.0"]);
    assert_eq!(report.refs_rescued, 2);
    // A branch deleted without a conflict stays where it is.
    source.git(&["branch", "-D", "feature/x"]);
    source.sync(300).await;
    assert!(source.archive_refs().contains_key("refs/heads/feature/x"));
}

#[tokio::test]
async fn force_push_keeps_the_old_commit_and_fast_forward_does_not() {
    let mut source = Source::new();
    source.commit("one");
    let lost = source.commit("two");
    source.sync(100).await;

    // Fast-forward: nothing to keep.
    source.commit("three");
    assert_eq!(source.sync(200).await.refs_rescued, 0);
    let before_rewrite = source.git(&["rev-parse", "HEAD"]);

    // Rewrite main so that it no longer contains the old commits.
    source.git(&["reset", "--quiet", "--hard", "HEAD~2"]);
    let rewritten = source.commit("replacement");
    let report = source.sync(300).await;
    assert_eq!(report.refs_rescued, 1);

    let refs = source.archive_refs();
    assert_eq!(refs["refs/heads/main"], rewritten);
    let kept = format!("{REWRITTEN_REF_PREFIX}300/heads/main");
    assert_eq!(refs[&kept], before_rewrite);
    // The older commit is still in the archive, reachable from the kept ref.
    git_in(
        &source.archive,
        &["cat-file", "-e", &format!("{lost}^{{commit}}")],
    );
    git_in(
        &source.archive,
        &["merge-base", "--is-ancestor", &lost, &kept],
    );
}

#[tokio::test]
async fn two_rewrites_in_the_same_second_keep_both_old_commits() {
    let mut source = Source::new();
    let first = source.commit("one");
    source.sync(100).await;

    source.git(&["commit", "--quiet", "--amend", "--allow-empty", "-m", "two"]);
    let second = source.git(&["rev-parse", "HEAD"]);
    assert_eq!(source.sync(500).await.refs_rescued, 1);
    source.git(&[
        "commit",
        "--quiet",
        "--amend",
        "--allow-empty",
        "-m",
        "three",
    ]);
    assert_eq!(source.sync(500).await.refs_rescued, 1);

    let refs = source.archive_refs();
    assert_eq!(
        refs[&format!("{REWRITTEN_REF_PREFIX}500/heads/main")],
        first
    );
    assert_eq!(
        refs[&format!("{REWRITTEN_REF_PREFIX}501/heads/main")],
        second
    );
}

#[tokio::test]
async fn a_kept_ref_below_the_name_moves_the_rescue_to_the_next_second() {
    let mut source = Source::new();
    let first = source.commit("one");
    source.sync(100).await;
    // An earlier sync of the same second kept `heads/main/old` already.
    let below = format!("{REWRITTEN_REF_PREFIX}500/heads/main/old");
    git_in(&source.archive, &["update-ref", &below, &first]);

    source.git(&["commit", "--quiet", "--amend", "--allow-empty", "-m", "two"]);
    assert_eq!(source.sync(500).await.refs_rescued, 1);
    let refs = source.archive_refs();
    assert_eq!(
        refs[&format!("{REWRITTEN_REF_PREFIX}501/heads/main")],
        first
    );
    assert_eq!(refs[&below], first);
}

#[tokio::test]
async fn a_rebuilt_merge_ref_is_not_kept_but_a_rewritten_pull_head_is() {
    let mut source = Source::new();
    let base = source.commit("one");
    let first = source.commit("two");
    source.git(&["update-ref", "refs/pull/1/merge", &first]);
    source.git(&["update-ref", "refs/merge-requests/2/merge", &first]);
    source.git(&["update-ref", "refs/pull/1/head", &first]);
    source.sync(100).await;

    // The server rebuilds the test merge on every base change; the author
    // force-pushes the pull request head.
    source.git(&["reset", "--quiet", "--hard", &base]);
    let other = source.commit("three");
    for name in [
        "refs/pull/1/merge",
        "refs/merge-requests/2/merge",
        "refs/pull/1/head",
    ] {
        source.git(&["update-ref", name, &other]);
    }
    let report = source.sync(200).await;

    let refs = source.archive_refs();
    let kept: Vec<&String> = refs
        .keys()
        .filter(|name| name.starts_with(REWRITTEN_REF_PREFIX))
        .collect();
    // main and the pull request head; never the rebuilt merge commits.
    assert_eq!(
        kept,
        [
            &format!("{REWRITTEN_REF_PREFIX}200/heads/main"),
            &format!("{REWRITTEN_REF_PREFIX}200/pull/1/head"),
        ]
    );
    assert_eq!(report.refs_rescued, 2);
    assert_eq!(refs["refs/pull/1/merge"], other);
}

#[tokio::test]
async fn moved_tag_is_kept() {
    let mut source = Source::new();
    let first = source.commit("one");
    source.git(&["tag", "release"]);
    source.sync(100).await;

    source.git(&["checkout", "--quiet", "--orphan", "other"]);
    source.commit("unrelated");
    source.git(&["tag", "--force", "release"]);
    let report = source.sync(200).await;

    assert_eq!(report.refs_rescued, 1);
    assert_eq!(
        source.archive_refs()[&format!("{REWRITTEN_REF_PREFIX}200/tags/release")],
        first
    );
}

#[tokio::test]
async fn source_cannot_write_archive_refs() {
    let mut source = Source::new();
    let commit = source.commit("one");
    source.git(&[
        "update-ref",
        "refs/git-archive/rewritten/1/heads/main",
        &commit,
    ]);
    source.sync(100).await;
    assert_eq!(
        without_archive_refs(source.archive_refs()),
        without_archive_refs(refs(&source.path))
    );
    assert!(
        !source
            .archive_refs()
            .keys()
            .any(|name| name.starts_with("refs/git-archive/"))
    );
}

#[tokio::test]
async fn excluded_refs_are_not_fetched_and_archived_ones_stay() {
    let mut source = Source::new();
    let first = source.commit("one");
    source.git(&["update-ref", "refs/pull/1/head", &first]);
    source.sync(100).await;
    assert!(source.archive_refs().contains_key("refs/pull/1/head"));

    let second = source.commit("two");
    source.git(&["update-ref", "refs/pull/2/head", &second]);
    source.git(&["update-ref", "refs/pull/1/head", &second]);
    let report = source.sync_excluding(200, &["refs/pull/*"]).await;

    let refs = source.archive_refs();
    // Branches still arrive; the pull refs neither appear nor move.
    assert_eq!(refs["refs/heads/main"], second);
    assert_eq!(report.refs_changed, 1);
    assert!(!refs.contains_key("refs/pull/2/head"));
    assert_eq!(refs["refs/pull/1/head"], first);
    assert_eq!(report.refs_rescued, 0);

    // Without the pattern the pull refs are fetched again.
    source.sync(300).await;
    let refs = source.archive_refs();
    assert_eq!(refs["refs/pull/2/head"], second);
    assert_eq!(refs["refs/pull/1/head"], second);
}

#[tokio::test]
async fn missing_source_fails_as_not_found_and_leaves_no_directory() {
    let source = Source::new();
    let missing = format!("{}-missing", source.url());
    let err = mirror::sync(&git(), &missing, &source.archive, None, 1, &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, GitErrorKind::NotFound);
    assert!(!source.archive.exists());
}

#[tokio::test]
async fn wiki_is_mirrored_when_present_and_skipped_when_absent() {
    let mut source = Source::new();
    source.commit("one");
    let wiki_url = mirror::wiki_url(&source.url(), false);
    let wiki_dir = mirror::wiki_dir(&source.archive);

    let absent = mirror::sync_wiki(&git(), &wiki_url, &wiki_dir, None, 1)
        .await
        .unwrap();
    assert_eq!(absent, None);
    assert!(!wiki_dir.exists());

    let wiki_source = PathBuf::from(&wiki_url);
    std::fs::create_dir(&wiki_source).unwrap();
    git_in(
        &wiki_source,
        &["init", "--quiet", "--initial-branch=master"],
    );
    std::fs::write(wiki_source.join("Home.md"), "# Home").unwrap();
    git_in(&wiki_source, &["add", "Home.md"]);
    git_in(&wiki_source, &["commit", "--quiet", "-m", "wiki"]);

    let present = mirror::sync_wiki(&git(), &wiki_url, &wiki_dir, None, 2)
        .await
        .unwrap()
        .expect("wiki report");
    assert_eq!(present.default_branch.as_deref(), Some("master"));
    assert_eq!(refs(&wiki_dir), refs(&wiki_source));
}

#[tokio::test]
async fn stale_lock_files_from_a_killed_sync_are_removed() {
    let mut source = Source::new();
    source.commit("one");
    source.sync(100).await;
    std::fs::write(source.archive.join("packed-refs.lock"), "").unwrap();
    std::fs::write(source.archive.join("refs/heads/main.lock"), "").unwrap();
    // A sync killed while it wrote the configuration.
    std::fs::write(source.archive.join("config.lock"), "").unwrap();

    source.commit("two");
    let report = source.sync(200).await;
    assert_eq!(report.refs_changed, 1);
    assert!(!source.archive.join("packed-refs.lock").exists());
    assert!(!source.archive.join("refs/heads/main.lock").exists());
    assert!(!source.archive.join("config.lock").exists());
}

#[tokio::test]
async fn repack_keeps_rescued_and_unreachable_commits() {
    let mut source = Source::new();
    source.commit("one");
    let lost = source.commit("two");
    source.sync(100).await;
    source.git(&["reset", "--quiet", "--hard", "HEAD~1"]);
    source.commit("replacement");
    assert_eq!(source.sync(200).await.refs_rescued, 1);

    // A commit that no ref and no reflog reaches.
    let tree = git_in(&source.archive, &["rev-parse", "HEAD^{tree}"]);
    let dangling = git_in(
        &source.archive,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit-tree",
            &tree,
            "-m",
            "dangling",
        ],
    );

    maintenance::fsck(&git(), &source.archive).await.unwrap();
    let before = mirror::disk_usage(&source.archive).await.unwrap() as i64;
    let saved = maintenance::repack(&git(), &source.archive).await.unwrap();
    let after = mirror::disk_usage(&source.archive).await.unwrap() as i64;
    assert_eq!(saved, before - after);

    for commit in [&lost, &dangling] {
        git_in(
            &source.archive,
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
        );
    }
    let counts = git_in(&source.archive, &["count-objects", "-v"]);
    assert!(counts.lines().any(|l| l == "count: 0"), "{counts}");
    maintenance::fsck(&git(), &source.archive).await.unwrap();
}

#[tokio::test]
async fn disk_usage_counts_the_archive() {
    let mut source = Source::new();
    source.commit("one");
    source.sync(100).await;
    assert!(mirror::disk_usage(&source.archive).await.unwrap() > 0);
}
