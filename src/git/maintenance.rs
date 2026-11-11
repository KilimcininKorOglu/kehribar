//! Scheduled care of an archived repository: an integrity check and a
//! repack that never drops an object.

use std::path::Path;

use super::command::{Git, GitError};
use super::mirror::disk_usage;

/// Checks every object of the repository. The error names the first
/// problems git reports. Without `--strict`, old but harmless object
/// formats (such as zero-padded file modes) stay warnings.
pub async fn fsck(git: &Git, dir: &Path) -> Result<(), GitError> {
    git.run(Some(dir), ["fsck", "--no-progress", "--no-dangling"], None)
        .await?;
    Ok(())
}

/// Packs the refs and every object into one pack and returns the bytes it
/// saved, which is negative when the repository grew (a small repository's
/// pack index can outweigh its loose objects). `--keep-unreachable` puts objects that no ref reaches, such as the
/// old commits of a force push without a kept ref, into the new pack, so the
/// repack loses nothing.
pub async fn repack(git: &Git, dir: &Path) -> Result<i64, GitError> {
    let before = measure(dir).await?;
    git.run(Some(dir), ["pack-refs", "--all"], None).await?;
    git.run(
        Some(dir),
        ["repack", "-a", "-d", "--keep-unreachable", "--quiet"],
        None,
    )
    .await?;
    let after = measure(dir).await?;
    Ok(before - after)
}

async fn measure(dir: &Path) -> Result<i64, GitError> {
    let bytes = disk_usage(dir).await.map_err(|err| {
        GitError::local(
            super::GitErrorKind::Other,
            format!("cannot measure {}: {err}", dir.display()),
        )
    })?;
    Ok(i64::try_from(bytes).unwrap_or(i64::MAX))
}
