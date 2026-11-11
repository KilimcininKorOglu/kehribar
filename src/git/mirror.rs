//! Keeps a bare mirror of a remote repository without ever losing history.
//!
//! The fetch never prunes, so a branch deleted at the source stays in the
//! archive. When a ref moves to a commit that does not contain its old one
//! (a force push), the old commit is kept under
//! `refs/git-archive/rewritten/<unix time>/<ref>`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::command::{Access, Git, GitError, GitErrorKind};
use crate::ref_filter;

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

/// How a ref changed in one sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Created,
    /// The new commit contains the old one.
    Moved,
    /// The new commit does not contain the old one, which was kept.
    Rewritten,
}

/// One ref that a sync created or moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefChange {
    pub name: String,
    pub old: Option<String>,
    pub new: String,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Refs created or moved by this sync.
    pub refs_changed: u64,
    /// Rewritten refs whose old commit was kept.
    pub refs_rescued: u64,
    /// Branch the source's HEAD points to, when the source has one.
    pub default_branch: Option<String>,
    /// The changes of the tracked refs, see [`is_tracked`].
    pub changes: Vec<RefChange>,
    /// The tracked refs of the archive after the fetch, name to object id.
    pub archive_refs: BTreeMap<String, String>,
    /// The tracked refs of the source.
    pub source_refs: BTreeMap<String, String>,
}

impl SyncReport {
    /// Tracked refs that the archive keeps but the source no longer has.
    pub fn missing_at_source(&self) -> impl Iterator<Item = (&String, &String)> {
        self.archive_refs
            .iter()
            .filter(|(name, _)| !self.source_refs.contains_key(*name))
    }
}

/// Whether the history of a ref is recorded: branches and tags that no
/// exclude pattern matches.
pub fn is_tracked(name: &str, exclude: &[String]) -> bool {
    (name.starts_with("refs/heads/") || name.starts_with("refs/tags/"))
        && !ref_filter::is_excluded(exclude, name)
}

/// Fetches every ref of `url` into the bare repository at `dir`, creating
/// the repository on the first run. `now` names the rescue refs. Refs that
/// match a pattern of `exclude` are not fetched; the archive keeps the ones
/// it already has.
pub async fn sync(
    git: &Git,
    url: &str,
    dir: &Path,
    access: Option<&Access>,
    now: i64,
    exclude: &[String],
) -> Result<SyncReport, GitError> {
    // Ask the source first: a missing repository leaves no directory behind.
    let remote = remote_refs(git, url, access).await?;
    open_repository(git, url, dir, exclude).await?;

    let before = list_refs(git, dir).await?;
    let set_aside = set_aside_blocking(git, dir, &before, &remote.refs, exclude, now).await?;
    git.run(
        Some(dir),
        ["fetch", "--quiet", "--no-write-fetch-head", "origin"],
        access,
    )
    .await?;
    let after = list_refs(git, dir).await?;

    let changes = classify_changes(git, dir, &before, &after, now).await?;
    let default_branch = point_head(git, dir, remote.head.as_deref(), &after).await?;
    let fetched = Fetched {
        changes,
        set_aside,
        after,
        source: remote.refs,
    };
    Ok(fetched.report(default_branch, exclude))
}

/// The refs of one fetch, before they become a [`SyncReport`].
struct Fetched {
    changes: Vec<RefChange>,
    set_aside: BTreeMap<String, String>,
    after: BTreeMap<String, String>,
    source: BTreeMap<String, String>,
}

impl Fetched {
    fn report(self, default_branch: Option<String>, exclude: &[String]) -> SyncReport {
        let Self {
            changes,
            set_aside,
            after,
            mut source,
        } = self;
        let tracked = |refs: BTreeMap<String, String>| -> BTreeMap<String, String> {
            refs.into_iter()
                .filter(|(name, _)| is_tracked(name, exclude))
                .collect()
        };
        // The source was listed before the fetch: a ref the fetch brought
        // that the list lacks was pushed in between, so the source has it.
        for change in &changes {
            source
                .entry(change.name.clone())
                .or_insert_with(|| change.new.clone());
        }
        let rewritten = changes
            .iter()
            .filter(|c| c.kind == ChangeKind::Rewritten)
            .count();
        // A ref set aside left the archive's branches and tags with this
        // fetch; listing it here lets the history mark it as deleted at the
        // source.
        let mut archive_refs = tracked(after);
        archive_refs.extend(tracked(set_aside.clone()));
        SyncReport {
            refs_changed: changes.len() as u64,
            refs_rescued: (rewritten + set_aside.len()) as u64,
            default_branch,
            changes: changes
                .into_iter()
                .filter(|c| is_tracked(&c.name, exclude))
                .collect(),
            archive_refs,
            source_refs: tracked(source),
        }
    }
}

/// Sets aside the archive refs that the source deleted and that block one
/// of its refs by name: git cannot store `a` next to `a/b`, so the fetch
/// would fail on every sync. Each one is kept like the old commit of a
/// rewritten ref, then removed. Returns them, name to object id.
async fn set_aside_blocking(
    git: &Git,
    dir: &Path,
    before: &BTreeMap<String, String>,
    remote: &BTreeMap<String, String>,
    exclude: &[String],
    now: i64,
) -> Result<BTreeMap<String, String>, GitError> {
    // Excluded refs are not fetched, so they need no room.
    let fetched: BTreeMap<&str, &str> = remote
        .iter()
        .filter(|(name, _)| !ref_filter::is_excluded(exclude, name))
        .map(|(name, id)| (name.as_str(), id.as_str()))
        .collect();
    let mut set_aside = BTreeMap::new();
    for (name, id) in before {
        if fetched.contains_key(name.as_str()) || !blocks(name, &fetched) {
            continue;
        }
        rescue(git, dir, name, id, now).await?;
        git.run(
            Some(dir),
            ["update-ref", "-d", name.as_str(), id.as_str()],
            None,
        )
        .await?;
        tracing::info!(r#ref = %name, "ref deleted at the source set aside for a new ref");
        set_aside.insert(name.clone(), id.clone());
    }
    Ok(set_aside)
}

/// Whether `name` and a source ref lie one inside the other, such as
/// `refs/heads/a` and `refs/heads/a/b`.
fn blocks(name: &str, fetched: &BTreeMap<&str, &str>) -> bool {
    let below = format!("{name}/");
    let has_child = fetched
        .range(below.as_str()..)
        .next()
        .is_some_and(|(other, _)| other.starts_with(&below));
    let has_parent = name
        .match_indices('/')
        .any(|(at, _)| fetched.contains_key(&name[..at]));
    has_child || has_parent
}

/// Mirrors the wiki repository. Returns `None` when the source has no wiki.
pub async fn sync_wiki(
    git: &Git,
    wiki_url: &str,
    dir: &Path,
    access: Option<&Access>,
    now: i64,
) -> Result<Option<SyncReport>, GitError> {
    match sync(git, wiki_url, dir, access, now, &[]).await {
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

/// The refs of the source and the ref its HEAD points to.
#[derive(Debug, Default, PartialEq, Eq)]
struct RemoteRefs {
    head: Option<String>,
    refs: BTreeMap<String, String>,
}

/// Lists the refs of the source, which also checks that the source is
/// reachable with these credentials.
async fn remote_refs(
    git: &Git,
    url: &str,
    access: Option<&Access>,
) -> Result<RemoteRefs, GitError> {
    let out = git
        .run(
            None,
            ["ls-remote", "--symref", "--end-of-options", url],
            access,
        )
        .await?;
    Ok(parse_ls_remote(&String::from_utf8_lossy(&out)))
}

/// Reads `git ls-remote --symref` output. The peeled lines of annotated
/// tags (`<ref>^{}`) are skipped: the archive lists the tag object too.
fn parse_ls_remote(text: &str) -> RemoteRefs {
    let mut remote = RemoteRefs::default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("ref: ") {
            if let Some((target, "HEAD")) = rest.split_once('\t') {
                remote.head = Some(target.to_owned());
            }
            continue;
        }
        let Some((id, name)) = line.split_once('\t') else {
            continue;
        };
        if name != "HEAD" && !name.ends_with("^{}") {
            remote.refs.insert(name.to_owned(), id.to_owned());
        }
    }
    remote
}

/// Readies the archive for a fetch: removes the locks a killed sync left,
/// then creates and configures the repository.
async fn open_repository(
    git: &Git,
    url: &str,
    dir: &Path,
    exclude: &[String],
) -> Result<(), GitError> {
    // Before any `git config` write: a `config.lock` left by a killed sync
    // would make the first of them fail on every later sync.
    if dir.join("HEAD").is_file() {
        remove_stale_locks(dir).await?;
    }
    prepare_repository(git, url, dir, exclude).await
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

/// Ref name to object id of the refs that match `pattern`, every ref when
/// it is `None`. A pattern matches the ref itself and the refs below it.
async fn ref_ids(
    git: &Git,
    dir: &Path,
    pattern: Option<&str>,
) -> Result<BTreeMap<String, String>, GitError> {
    let mut args = vec!["for-each-ref", "--format=%(objectname) %(refname)"];
    args.extend(pattern);
    let out = git.run(Some(dir), args, None).await?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(id, name)| (name.to_owned(), id.to_owned()))
        .collect())
}

/// Ref name to object id, without the archive's own refs.
async fn list_refs(git: &Git, dir: &Path) -> Result<BTreeMap<String, String>, GitError> {
    let mut refs = ref_ids(git, dir, None).await?;
    refs.retain(|name, _| !name.starts_with(ARCHIVE_REF_PREFIX));
    Ok(refs)
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

/// Sorts every changed ref into created, moved and rewritten, and keeps the
/// old commit of every rewritten ref.
async fn classify_changes(
    git: &Git,
    dir: &Path,
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    now: i64,
) -> Result<Vec<RefChange>, GitError> {
    let mut changes = Vec::new();
    for (name, new_id) in changed_refs(before, after) {
        let old = before.get(name);
        let kind = match old {
            None => ChangeKind::Created,
            Some(old_id) if is_ancestor(git, dir, old_id, new_id).await? => ChangeKind::Moved,
            Some(old_id) => {
                rescue(git, dir, name, old_id, now).await?;
                ChangeKind::Rewritten
            }
        };
        changes.push(RefChange {
            name: name.clone(),
            old: old.cloned(),
            new: new_id.clone(),
            kind,
        });
    }
    Ok(changes)
}

/// Seconds after the sync start a kept commit may be named with, when an
/// earlier sync of the same second kept another commit of the same ref.
const RESCUE_NAME_TRIES: i64 = 60;

/// Keeps `old_id`, the commit a rewritten ref pointed to. A name that holds
/// another commit is never overwritten; the next second's name is used.
async fn rescue(git: &Git, dir: &Path, name: &str, old_id: &str, now: i64) -> Result<(), GitError> {
    let short = name.strip_prefix("refs/").unwrap_or(name);
    for at in now..now + RESCUE_NAME_TRIES {
        let rescue = format!("{REWRITTEN_REF_PREFIX}{at}/{short}");
        match ref_ids(git, dir, Some(&rescue)).await?.remove(&rescue) {
            Some(id) if id == old_id => return Ok(()),
            Some(_) => continue,
            None => {}
        }
        // The empty old value makes the update fail if the ref exists.
        git.run(Some(dir), ["update-ref", rescue.as_str(), old_id, ""], None)
            .await?;
        tracing::info!(r#ref = %name, old = %old_id, kept_as = %rescue, "rewritten ref kept");
        return Ok(());
    }
    Err(GitError {
        kind: GitErrorKind::Other,
        exit_code: None,
        message: format!("no free name to keep the old commit of {name}"),
    })
}

/// A commit kept when its ref was rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RescuedRef {
    /// When the ref was rewritten.
    pub at: i64,
    /// The rewritten ref, such as `refs/heads/main`.
    pub name: String,
    pub id: String,
}

/// The kept commits of the tracked refs, see [`is_tracked`].
pub async fn rescued_refs(
    git: &Git,
    dir: &Path,
    exclude: &[String],
) -> Result<Vec<RescuedRef>, GitError> {
    Ok(ref_ids(git, dir, Some(REWRITTEN_REF_PREFIX))
        .await?
        .into_iter()
        .filter_map(|(full, id)| parse_rescued(&full, id))
        .filter(|r| is_tracked(&r.name, exclude))
        .collect())
}

/// Reads `refs/git-archive/rewritten/<time>/<name without refs/>`.
fn parse_rescued(full: &str, id: String) -> Option<RescuedRef> {
    let (at, short) = full.strip_prefix(REWRITTEN_REF_PREFIX)?.split_once('/')?;
    Some(RescuedRef {
        at: at.parse().ok()?,
        name: format!("refs/{short}"),
        id,
    })
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

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn a_ref_pushed_during_the_fetch_counts_as_at_the_source() {
        // ls-remote listed only main; the fetch also brought a new branch.
        let fetched = Fetched {
            changes: vec![RefChange {
                name: "refs/heads/new".into(),
                old: None,
                new: "2".into(),
                kind: ChangeKind::Created,
            }],
            set_aside: BTreeMap::new(),
            after: map(&[("refs/heads/main", "1"), ("refs/heads/new", "2")]),
            source: map(&[("refs/heads/main", "1")]),
        };
        let report = fetched.report(None, &[]);
        assert_eq!(report.missing_at_source().count(), 0);
        assert_eq!(report.source_refs["refs/heads/new"], "2");
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

    #[test]
    fn ls_remote_output_gives_head_and_refs_without_peeled_tags() {
        let text = "ref: refs/heads/main\tHEAD\n\
                    aaa\tHEAD\n\
                    aaa\trefs/heads/main\n\
                    bbb\trefs/tags/v1\n\
                    ccc\trefs/tags/v1^{}\n\
                    ddd\trefs/pull/1/head\n";
        let remote = parse_ls_remote(text);
        assert_eq!(remote.head.as_deref(), Some("refs/heads/main"));
        let names: Vec<_> = remote
            .refs
            .iter()
            .map(|(n, id)| (n.as_str(), id.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("refs/heads/main", "aaa"),
                ("refs/pull/1/head", "ddd"),
                ("refs/tags/v1", "bbb")
            ]
        );
    }

    #[test]
    fn only_branches_and_tags_outside_the_patterns_are_tracked() {
        let exclude = ["refs/heads/wip-*".to_owned()];
        assert!(is_tracked("refs/heads/main", &exclude));
        assert!(is_tracked("refs/tags/v1", &exclude));
        assert!(!is_tracked("refs/heads/wip-1", &exclude));
        assert!(!is_tracked("refs/pull/1/head", &exclude));
        assert!(!is_tracked("refs/notes/commits", &exclude));
    }

    #[test]
    fn rescued_ref_names_give_time_and_ref() {
        let id = || "abc".to_owned();
        assert_eq!(
            parse_rescued(
                "refs/git-archive/rewritten/1700000000/heads/feature/x",
                id()
            ),
            Some(RescuedRef {
                at: 1_700_000_000,
                name: "refs/heads/feature/x".into(),
                id: id()
            })
        );
        assert_eq!(parse_rescued("refs/heads/main", id()), None);
        assert_eq!(
            parse_rescued("refs/git-archive/rewritten/x/heads/a", id()),
            None
        );
    }
}
