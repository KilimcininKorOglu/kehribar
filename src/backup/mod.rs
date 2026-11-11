//! Uploads a `git bundle` of every repository whose refs changed since its
//! last upload to S3-compatible storage, and keeps the newest bundles.
//!
//! Keys: `<prefix>/<host>/<owner>/<name>/<unix time>.bundle`, and
//! `<prefix>/<host>/<owner>/<name>.wiki/<unix time>.bundle` for the wiki.
//! `git clone --mirror <file>.bundle` restores a repository. With LFS on,
//! every LFS object goes to `<prefix>/<host>/<owner>/<name>/lfs/<oid>` once,
//! and every archived Markdown image to `.../<name>/assets/<sha256>` once.

pub mod s3;
pub mod settings;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::db::repos::{self, Repo};
use crate::db::{assets, backups, unix_now};
use crate::git::{Git, lfs, mirror};
use crate::notify::{self, Change, Event};
use crate::sync::SyncContext;
use s3::Store;
use settings::{LastRun, Target};

/// Longest error text kept for the settings page and the notification.
const MAX_ERROR_LEN: usize = 2000;

/// Whether a backup is configured and its interval has passed.
pub async fn is_due(ctx: &SyncContext) -> sqlx::Result<bool> {
    let interval_hours = match settings::effective(&ctx.pool, &ctx.config, &ctx.cipher).await {
        Ok(current) if current.target.is_none() => return Ok(false),
        Ok(current) => current.interval_hours,
        // A damaged stored secret: the run reports it, on the interval of
        // the environment.
        Err(_) => i64::from(ctx.config.backup.interval_hours),
    };
    let last = settings::last_run(&ctx.pool).await?;
    Ok(last.is_none_or(|run| unix_now() - run.at >= interval_hours * 3600))
}

/// Backs up every repository now. Returns `None` when no backup is
/// configured. The outcome is stored and a change is notified.
pub async fn run(ctx: &SyncContext, part_size: usize) -> sqlx::Result<Option<LastRun>> {
    let started = unix_now();
    let (uploaded, error) = match settings::effective(&ctx.pool, &ctx.config, &ctx.cipher).await {
        Ok(current) => match &current.target {
            Some(target) => back_up_all(ctx, target, current.keep, part_size).await?,
            None => return Ok(None),
        },
        Err(err) => (0, Some(err)),
    };
    let run = LastRun {
        at: started,
        uploaded,
        error: error.map(|text| shorten(&text)),
    };
    let previous = settings::last_run(&ctx.pool).await?;
    settings::record_run(&ctx.pool, &run).await?;
    match &run.error {
        Some(err) => tracing::warn!(uploaded, %err, "backup finished with errors"),
        None => tracing::info!(uploaded, "backup finished"),
    }
    report_change(
        ctx,
        previous.and_then(|p| p.error).as_deref(),
        run.error.as_deref(),
    )
    .await;
    Ok(Some(run))
}

async fn report_change(ctx: &SyncContext, previous: Option<&str>, error: Option<&str>) {
    let event = match notify::error_change(previous, error) {
        Some(Change::Failed) => Event::BackupFailed {
            message: error.unwrap_or_default(),
        },
        Some(Change::Recovered) => Event::BackupRecovered,
        None => return,
    };
    notify::send(ctx, event).await;
}

/// Writes a small object, reads it back and deletes it.
pub async fn test_target(target: &Target) -> Result<(), String> {
    let store = Store::new(target, s3::PART_SIZE)?;
    let key = store.key(".git-archive-test");
    let body = format!("git-archive {}", unix_now()).into_bytes();
    store.put_bytes(&key, body.clone()).await?;
    let read = store.get(&key).await?;
    store.delete(&key).await?;
    if read == body {
        Ok(())
    } else {
        Err("the storage returned other content than it stored".to_owned())
    }
}

/// Returns the number of uploaded repositories and the joined errors.
/// One failed repository does not stop the others.
async fn back_up_all(
    ctx: &SyncContext,
    target: &Target,
    keep: i64,
    part_size: usize,
) -> sqlx::Result<(i64, Option<String>)> {
    let store = match Store::new(target, part_size) {
        Ok(store) => store,
        Err(err) => return Ok((0, Some(err))),
    };
    let sync = crate::settings::effective(&ctx.pool, &ctx.config).await?;
    let git = Git::new(Duration::from_secs(
        u64::try_from(sync.git_timeout_seconds).unwrap_or(u64::MAX),
    ));
    let job = Job {
        ctx,
        store: &store,
        git: &git,
        keep: usize::try_from(keep).unwrap_or(1),
    };
    let mut uploaded = 0;
    let mut errors = Vec::new();
    for repo in repos::list(&ctx.pool, None).await? {
        match job.back_up(&repo).await {
            Ok(true) => uploaded += 1,
            Ok(false) => {}
            Err(err) => errors.push(format!("{}: {err}", repo.full_name())),
        }
    }
    let error = (!errors.is_empty()).then(|| errors.join("; "));
    Ok((uploaded, error))
}

struct Job<'a> {
    ctx: &'a SyncContext,
    store: &'a Store,
    git: &'a Git,
    keep: usize,
}

/// One bundle to upload and the key folder it goes to.
struct Bundle {
    folder: String,
    file: PathBuf,
}

/// Deletes the temporary bundle files when the upload ends, also on error.
struct TempFiles(Vec<PathBuf>);

impl Drop for TempFiles {
    fn drop(&mut self) {
        for file in &self.0 {
            if let Err(err) = std::fs::remove_file(file) {
                tracing::warn!(file = %file.display(), %err, "cannot remove a temporary bundle");
            }
        }
    }
}

impl Job<'_> {
    /// Uploads the bundles of the repository when its refs changed since the
    /// last upload, and its LFS objects the storage does not have yet.
    /// Returns whether it uploaded anything.
    async fn back_up(&self, repo: &Repo) -> Result<bool, String> {
        let bundled = self.back_up_refs(repo).await?;
        // Checked on every run, so LFS turned on later, or an upload that
        // failed before, catches up without a ref change.
        let objects = if repo.lfs_enabled {
            self.back_up_lfs(repo).await?
        } else {
            0
        };
        let images = self.back_up_assets(repo).await?;
        Ok(bundled || objects > 0 || images > 0)
    }

    /// Uploads every archived image of the repository missing under
    /// `<repo>/assets/<sha256>`. Like LFS objects, a stored image is named
    /// by its content and never changes. Returns the number of uploads.
    async fn back_up_assets(&self, repo: &Repo) -> Result<usize, String> {
        let rows = assets::for_repo(&self.ctx.pool, repo.id)
            .await
            .map_err(|err| err.to_string())?;
        let dir = self.ctx.config.assets_dir();
        let hashes: HashSet<String> = rows.into_iter().map(|row| row.sha256).collect();
        let files = hashes
            .into_iter()
            .map(|hash| {
                let file = dir.join(&hash);
                (hash, file)
            })
            .collect();
        self.upload_missing(&format!("{}/assets/", repo.full_name()), files)
            .await
    }

    /// Uploads every local LFS object missing under `<repo>/lfs/<oid>`. The
    /// objects are named by their content, so a stored one never changes
    /// and is never pruned. Returns the number of uploaded objects.
    async fn back_up_lfs(&self, repo: &Repo) -> Result<usize, String> {
        let dir = self.ctx.repo_dir(repo);
        let local = lfs::local_objects(&dir)
            .await
            .map_err(|err| format!("cannot list the LFS objects: {err}"))?;
        self.upload_missing(&format!("{}/lfs/", repo.full_name()), local)
            .await
    }

    /// Uploads each `(name, file)` the storage lacks under `folder`. The
    /// names are content hashes, so a stored file never needs an update.
    async fn upload_missing(
        &self,
        folder: &str,
        files: Vec<(String, PathBuf)>,
    ) -> Result<usize, String> {
        if files.is_empty() {
            return Ok(0);
        }
        let prefix = self.store.key(folder);
        let stored: HashSet<String> = self.store.list(&prefix).await?.into_iter().collect();
        let mut uploaded = 0;
        for (name, file) in files {
            let key = format!("{prefix}{name}");
            if !stored.contains(&key) {
                self.store.put_file(&key, &file).await?;
                uploaded += 1;
            }
        }
        Ok(uploaded)
    }

    async fn back_up_refs(&self, repo: &Repo) -> Result<bool, String> {
        let now = unix_now();
        let (hash, bundles) = {
            // A sync must not repack while the bundles are written.
            let _lock = self.ctx.locks.lock(repo.id).await;
            let dirs = self.dirs(repo);
            let Some(hash) = refs_hash(self.git, &dirs).await? else {
                return Ok(false);
            };
            let stored = backups::refs_hash(&self.ctx.pool, repo.id)
                .await
                .map_err(|err| err.to_string())?;
            if stored.as_deref() == Some(hash.as_str()) {
                return Ok(false);
            }
            (hash, self.write_bundles(repo, &dirs, now).await?)
        };
        let _cleanup = TempFiles(bundles.iter().map(|b| b.file.clone()).collect());
        for bundle in &bundles {
            let key = self.store.key(&format!("{}/{now}.bundle", bundle.folder));
            self.store.put_file(&key, &bundle.file).await?;
            self.prune(&bundle.folder).await?;
        }
        backups::record(&self.ctx.pool, repo.id, now, &hash)
            .await
            .map_err(|err| err.to_string())?;
        Ok(true)
    }

    /// The repository and its wiki, with the key folder of each.
    fn dirs(&self, repo: &Repo) -> Vec<(String, PathBuf)> {
        let dir = self.ctx.repo_dir(repo);
        let base = repo.full_name();
        vec![
            (format!("{base}.wiki"), mirror::wiki_dir(&dir)),
            (base, dir),
        ]
    }

    async fn write_bundles(
        &self,
        repo: &Repo,
        dirs: &[(String, PathBuf)],
        now: i64,
    ) -> Result<Vec<Bundle>, String> {
        let tmp = self.ctx.config.data_dir.join("tmp");
        tokio::fs::create_dir_all(&tmp)
            .await
            .map_err(|err| format!("cannot create {}: {err}", tmp.display()))?;
        let mut bundles = Vec::new();
        let mut cleanup = TempFiles(Vec::new());
        for (index, (folder, dir)) in dirs.iter().enumerate() {
            if ref_list(self.git, dir).await?.is_empty() {
                continue;
            }
            let file = tmp.join(format!("backup-{}-{now}-{index}.bundle", repo.id));
            cleanup.0.push(file.clone());
            self.git
                .run(
                    Some(dir),
                    [
                        "bundle".as_ref(),
                        "create".as_ref(),
                        file.as_os_str(),
                        "--all".as_ref(),
                    ],
                    None,
                )
                .await
                .map_err(|err| err.message)?;
            bundles.push(Bundle {
                folder: folder.clone(),
                file,
            });
        }
        // The caller removes the files after the upload.
        cleanup.0.clear();
        Ok(bundles)
    }

    /// Deletes the oldest bundles of `folder` beyond the number to keep.
    async fn prune(&self, folder: &str) -> Result<(), String> {
        let prefix = self.store.key(&format!("{folder}/"));
        let keys = self.store.list(&prefix).await?;
        for key in outdated(&prefix, keys, self.keep) {
            self.store.delete(&key).await?;
        }
        Ok(())
    }
}

/// The keys of `<prefix><unix time>.bundle` beyond the newest `keep`.
/// Other objects under the prefix are never touched.
fn outdated(prefix: &str, keys: Vec<String>, keep: usize) -> Vec<String> {
    let mut bundles: Vec<(i64, String)> = keys
        .into_iter()
        .filter_map(|key| {
            let stem = key.strip_prefix(prefix)?.strip_suffix(".bundle")?;
            Some((stem.parse().ok()?, key))
        })
        .collect();
    bundles.sort_by_key(|bundle| std::cmp::Reverse(bundle.0));
    bundles.into_iter().skip(keep).map(|(_, key)| key).collect()
}

/// The refs of a repository; empty when the directory holds none.
async fn ref_list(git: &Git, dir: &Path) -> Result<String, String> {
    if !dir.join("HEAD").is_file() {
        return Ok(String::new());
    }
    let out = git
        .run(
            Some(dir),
            ["for-each-ref", "--format=%(objectname) %(refname)"],
            None,
        )
        .await
        .map_err(|err| err.message)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// SHA-256 of the ref lists of `dirs`, or `None` when none has a ref.
async fn refs_hash(git: &Git, dirs: &[(String, PathBuf)]) -> Result<Option<String>, String> {
    let mut hasher = Sha256::new();
    let mut any = false;
    for (folder, dir) in dirs {
        let refs = ref_list(git, dir).await?;
        any |= !refs.is_empty();
        hasher.update(folder.as_bytes());
        hasher.update(b"\n");
        hasher.update(refs.as_bytes());
    }
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(any.then_some(hex))
}

fn shorten(text: &str) -> String {
    if text.len() <= MAX_ERROR_LEN {
        return text.to_owned();
    }
    let mut end = MAX_ERROR_LEN;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_oldest_bundles_beyond_the_limit_are_outdated() {
        let keys = [
            "p/1.bundle",
            "p/3.bundle",
            "p/2.bundle",
            "p/notes.txt",
            "p/4.bundle",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            outdated("p/", keys.clone(), 2),
            ["p/2.bundle", "p/1.bundle"]
        );
        assert!(outdated("p/", keys, 9).is_empty());
    }

    #[test]
    fn long_errors_are_shortened_on_a_char_boundary() {
        assert_eq!(shorten("short"), "short");
        let long = "ş".repeat(MAX_ERROR_LEN);
        let cut = shorten(&long);
        assert!(cut.ends_with('…') && cut.len() <= MAX_ERROR_LEN + '…'.len_utf8());
    }
}
