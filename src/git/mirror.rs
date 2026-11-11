//! Keeps a bare mirror of a remote repository without ever losing history.
//!
//! The fetch never prunes, so a branch deleted at the source stays in the
//! archive. When a ref moves to a commit that does not contain its old one
//! (a force push), the old commit is kept under
//! `refs/git-archive/rewritten/<unix time>/<ref>`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::command::{Credentials, Git, GitError, GitErrorKind};

/// Prefix of the refs that this archive creates. The fetch refspec excludes
/// it, so the source can neither overwrite nor delete these refs.
pub const ARCHIVE_REF_PREFIX: &str = "refs/git-archive/";
pub const REWRITTEN_REF_PREFIX: &str = "refs/git-archive/rewritten/";

/// Settings applied to every archived repository on each sync.
const REPO_CONFIG: &[(&str, &str)] = &[
    ("fetch.prune", "false"),
    ("fetch.pruneTags", "false"),
    ("gc.pruneExpire", "never"),
    ("gc.reflogExpire", "never"),
    ("gc.reflogExpireUnreachable", "never"),
    ("core.logAllRefUpdates", "always"),
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Refs created or moved by this sync.
    pub refs_changed: u64,
    /// Rewritten refs whose old commit was kept.
    pub refs_rescued: u64,
    /// Branch the source's HEAD points to, when the source has one.
    pub default_branch: Option<String>,
}

/// Fetches every ref of `url` into the bare repository at `dir`, creating
/// the repository on the first run. `now` names the rescue refs. Refs that
/// match a pattern of `exclude` are not fetched; the archive keeps the ones
/// it already has.
pub async fn sync(
    git: &Git,
    url: &str,
    dir: &Path,
    credentials: Option<&Credentials>,
    now: i64,
    exclude: &[String],
) -> Result<SyncReport, GitError> {
    // Ask the source first: a missing repository leaves no directory behind.
    let remote_head = remote_head(git, url, credentials).await?;
    prepare_repository(git, url, dir, exclude).await?;
    remove_stale_locks(dir).await?;

    let before = list_refs(git, dir).await?;
    git.run(
        Some(dir),
        ["fetch", "--quiet", "--no-write-fetch-head", "origin"],
        credentials,
    )
    .await?;
    let after = list_refs(git, dir).await?;

    let refs_changed = changed_refs(&before, &after).count() as u64;
    let refs_rescued = rescue_rewritten(git, dir, &before, &after, now).await?;
    let default_branch = point_head(git, dir, remote_head.as_deref(), &after).await?;
    Ok(SyncReport {
        refs_changed,
        refs_rescued,
        default_branch,
    })
}

/// Mirrors the wiki repository. Returns `None` when the source has no wiki.
pub async fn sync_wiki(
    git: &Git,
    wiki_url: &str,
    dir: &Path,
    credentials: Option<&Credentials>,
    now: i64,
) -> Result<Option<SyncReport>, GitError> {
    match sync(git, wiki_url, dir, credentials, now, &[]).await {
        Ok(report) => Ok(Some(report)),
        Err(err) if err.kind == GitErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// The wiki repository URL of a repository URL. Bitbucket Cloud keeps the
/// wiki at `<repo>.git/wiki`; GitHub, GitLab, Forgejo and Gitea at
/// `<repo>.wiki.git`.
pub fn wiki_url(url: &str, bitbucket: bool) -> String {
    let base = url.trim_end_matches('/');
    let base = base.strip_suffix(".git").unwrap_or(base);
    if bitbucket {
        format!("{base}.git/wiki")
    } else {
        format!("{base}.wiki.git")
    }
}

/// The bare repository that holds the wiki of the repository at `dir`.
pub fn wiki_dir(dir: &Path) -> PathBuf {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = name.strip_suffix(".git").unwrap_or(&name);
    dir.with_file_name(format!("{stem}.wiki.git"))
}

/// Reads the ref the source's HEAD points to, which also checks that the
/// source is reachable with these credentials.
async fn remote_head(
    git: &Git,
    url: &str,
    credentials: Option<&Credentials>,
) -> Result<Option<String>, GitError> {
    let out = git
        .run(
            None,
            ["ls-remote", "--symref", "--end-of-options", url, "HEAD"],
            credentials,
        )
        .await?;
    Ok(String::from_utf8_lossy(&out).lines().find_map(|line| {
        let target = line.strip_prefix("ref: ")?.split('\t').next()?;
        Some(target.to_owned())
    }))
}

async fn prepare_repository(
    git: &Git,
    url: &str,
    dir: &Path,
    exclude: &[String],
) -> Result<(), GitError> {
    if !dir.join("HEAD").is_file() {
        if let Some(parent) = dir.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(io_error)?;
        }
        git.run(
            None,
            [
                "init".as_ref(),
                "--bare".as_ref(),
                "--quiet".as_ref(),
                dir.as_os_str(),
            ],
            None,
        )
        .await?;
    }
    // Applied on every sync, so a repository created by an interrupted first
    // run, or by an older version, gets the full configuration.
    for (key, value) in REPO_CONFIG {
        git.run(Some(dir), ["config", key, value], None).await?;
    }
    configure_origin(git, url, dir, exclude).await
}

/// Points `origin` at `url` and sets its refspecs: every ref, except the
/// archive's own refs and the excluded patterns.
async fn configure_origin(
    git: &Git,
    url: &str,
    dir: &Path,
    exclude: &[String],
) -> Result<(), GitError> {
    git.run(Some(dir), ["config", "remote.origin.url", url], None)
        .await?;
    git.run(
        Some(dir),
        [
            "config",
            "--replace-all",
            "remote.origin.fetch",
            "+refs/*:refs/*",
        ],
        None,
    )
    .await?;
    // `--replace-all` above removed the negative refspecs of the last sync,
    // so a pattern deleted from the settings stops applying here.
    let archive_refs = format!("{ARCHIVE_REF_PREFIX}*");
    for pattern in std::iter::once(&archive_refs).chain(exclude) {
        let refspec = format!("^{pattern}");
        git.run(
            Some(dir),
            ["config", "--add", "remote.origin.fetch", refspec.as_str()],
            None,
        )
        .await?;
    }
    Ok(())
}

/// Removes lock files that a killed sync left behind. Only one sync runs per
/// repository at a time, so every lock found here is stale.
async fn remove_stale_locks(dir: &Path) -> Result<(), GitError> {
    let dir = dir.to_owned();
    tokio::task::spawn_blocking(move || remove_locks_in(&dir, true))
        .await
        .map_err(|err| GitError {
            kind: GitErrorKind::Other,
            exit_code: None,
            message: format!("lock cleanup failed: {err}"),
        })?
        .map_err(io_error)
}

/// Lock files live in the repository root and anywhere under `refs/`.
fn remove_locks_in(dir: &Path, is_root: bool) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        remove_lock_entry(&entry?, is_root)?;
    }
    Ok(())
}

fn remove_lock_entry(entry: &std::fs::DirEntry, in_root: bool) -> std::io::Result<()> {
    let path = entry.path();
    let file_type = entry.file_type()?;
    if file_type.is_dir() {
        // In the root only `refs/` holds locks; `objects/` is large and has none.
        if !in_root || entry.file_name() == "refs" {
            remove_locks_in(&path, false)?;
        }
        return Ok(());
    }
    if file_type.is_file() && path.extension().is_some_and(|e| e == "lock") {
        tracing::warn!(path = %path.display(), "removing stale git lock file");
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Ref name to object id, without the archive's own refs.
async fn list_refs(git: &Git, dir: &Path) -> Result<BTreeMap<String, String>, GitError> {
    let out = git
        .run(
            Some(dir),
            ["for-each-ref", "--format=%(objectname) %(refname)"],
            None,
        )
        .await?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, name)| !name.starts_with(ARCHIVE_REF_PREFIX))
        .map(|(id, name)| (name.to_owned(), id.to_owned()))
        .collect())
}

/// Refs that are new or point to another object after the fetch.
fn changed_refs<'a>(
    before: &'a BTreeMap<String, String>,
    after: &'a BTreeMap<String, String>,
) -> impl Iterator<Item = (&'a String, &'a String)> {
    after
        .iter()
        .filter(move |(name, id)| before.get(*name) != Some(*id))
}

/// Keeps the old commit of every ref whose new commit does not contain it.
async fn rescue_rewritten(
    git: &Git,
    dir: &Path,
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    now: i64,
) -> Result<u64, GitError> {
    let mut rescued = 0;
    for (name, new_id) in changed_refs(before, after) {
        let Some(old_id) = before.get(name) else {
            continue;
        };
        if is_ancestor(git, dir, old_id, new_id).await? {
            continue;
        }
        let short = name.strip_prefix("refs/").unwrap_or(name);
        let rescue = format!("{REWRITTEN_REF_PREFIX}{now}/{short}");
        git.run(
            Some(dir),
            ["update-ref", rescue.as_str(), old_id.as_str()],
            None,
        )
        .await?;
        tracing::info!(r#ref = %name, old = %old_id, kept_as = %rescue, "rewritten ref kept");
        rescued += 1;
    }
    Ok(rescued)
}

/// Whether `old` is reachable from `new`. Objects that are not commits, such
/// as a tag that moved to a tree, count as not reachable, so they are kept.
async fn is_ancestor(git: &Git, dir: &Path, old: &str, new: &str) -> Result<bool, GitError> {
    match git
        .run(Some(dir), ["merge-base", "--is-ancestor", old, new], None)
        .await
    {
        Ok(_) => Ok(true),
        Err(err) if err.kind == GitErrorKind::Timeout || err.kind == GitErrorKind::Spawn => {
            Err(err)
        }
        Err(_) => Ok(false),
    }
}

/// Points the archive's HEAD at the source's default branch and returns the
/// branch name.
async fn point_head(
    git: &Git,
    dir: &Path,
    remote_head: Option<&str>,
    refs: &BTreeMap<String, String>,
) -> Result<Option<String>, GitError> {
    let Some(target) = remote_head.filter(|target| refs.contains_key(*target)) else {
        return Ok(None);
    };
    git.run(Some(dir), ["symbolic-ref", "HEAD", target], None)
        .await?;
    Ok(Some(
        target
            .strip_prefix("refs/heads/")
            .unwrap_or(target)
            .to_owned(),
    ))
}

/// Total size in bytes of the files under `dir`.
pub async fn disk_usage(dir: &Path) -> std::io::Result<u64> {
    let dir = dir.to_owned();
    tokio::task::spawn_blocking(move || size_of(&dir))
        .await
        .map_err(std::io::Error::other)?
}

fn size_of(path: &Path) -> std::io::Result<u64> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Ok(meta.len());
    }
    let mut total = 0;
    for entry in std::fs::read_dir(path)? {
        total += size_of(&entry?.path())?;
    }
    Ok(total)
}

fn io_error(err: std::io::Error) -> GitError {
    GitError {
        kind: GitErrorKind::Other,
        exit_code: None,
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wiki_url_follows_the_provider_layout() {
        assert_eq!(
            wiki_url("https://github.com/a/b.git", false),
            "https://github.com/a/b.wiki.git"
        );
        assert_eq!(
            wiki_url("https://codeberg.org/a/b/", false),
            "https://codeberg.org/a/b.wiki.git"
        );
        assert_eq!(
            wiki_url("https://bitbucket.org/a/b", true),
            "https://bitbucket.org/a/b.git/wiki"
        );
    }

    #[test]
    fn wiki_dir_sits_next_to_the_repository() {
        assert_eq!(
            wiki_dir(Path::new("/data/repos/h/o/n.git")),
            PathBuf::from("/data/repos/h/o/n.wiki.git")
        );
    }

    #[test]
    fn changed_refs_lists_new_and_moved_refs() {
        let before: BTreeMap<_, _> = [("a", "1"), ("b", "2"), ("gone", "3")]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .into();
        let after: BTreeMap<_, _> = [("a", "1"), ("b", "9"), ("new", "4"), ("gone", "3")]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .into();
        let names: Vec<_> = changed_refs(&before, &after)
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(names, ["b", "new"]);
    }
}
