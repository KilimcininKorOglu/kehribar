//! Moves the archives whose path an earlier version made without the `_git`
//! segment on a host that is not Azure DevOps (see
//! [`crate::repo_url::for_provider`]).

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use crate::db::repos::{self, Repo};
use crate::git::mirror;
use crate::repo_url;

/// Gives each repository whose URL holds `/_git/` the archive path its
/// host's provider calls for, moving its archive and wiki directories.
/// A repository whose new path is taken stays where it is, with a warning.
/// Call it at startup, before a sync runs. Gives the number moved.
pub async fn git_segment_paths(pool: &SqlitePool, data_dir: &Path) -> sqlx::Result<usize> {
    let mut moved = 0;
    for repo in repos::list(pool, Some("/_git/")).await? {
        let location = match repo_url::locate(pool, &repo.url).await? {
            Ok(location) => location,
            Err(err) => {
                tracing::warn!(url = %repo.url, %err, "archive path not checked");
                continue;
            }
        };
        if location.owner.eq_ignore_ascii_case(&repo.owner) {
            continue;
        }
        if move_repo(pool, data_dir, &repo, &location.owner).await? {
            moved += 1;
        }
    }
    Ok(moved)
}

/// Moves `repo` to the owner path `owner`. Gives `false` when the new
/// path is taken.
async fn move_repo(
    pool: &SqlitePool,
    data_dir: &Path,
    repo: &Repo,
    owner: &str,
) -> sqlx::Result<bool> {
    let taken = repos::find_by_path(pool, &repo.host, owner, &repo.name).await?;
    let from = repo_url::archive_dir(data_dir, &repo.host, &repo.owner, &repo.name);
    let to = repo_url::archive_dir(data_dir, &repo.host, owner, &repo.name);
    if taken.is_some() || to.exists() || mirror::wiki_dir(&to).exists() {
        tracing::warn!(url = %repo.url, to = %to.display(), "archive path taken, not moved");
        return Ok(false);
    }
    let pairs = [
        (from.clone(), to.clone()),
        (mirror::wiki_dir(&from), mirror::wiki_dir(&to)),
    ];
    let done = rename_all(&pairs).map_err(sqlx::Error::Io)?;
    if let Err(err) = repos::set_owner(pool, repo.id, owner).await {
        undo(&done);
        return Err(err);
    }
    tracing::info!(url = %repo.url, to = %to.display(), "archive moved to its path");
    Ok(true)
}

/// Renames each existing `from` to its `to`. On an error the renames done
/// so far are undone. Gives the renames done.
fn rename_all(pairs: &[(PathBuf, PathBuf)]) -> std::io::Result<Vec<(PathBuf, PathBuf)>> {
    let mut done = Vec::new();
    for (from, to) in pairs {
        if !from.exists() {
            continue;
        }
        let renamed = to
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::rename(from, to));
        if let Err(err) = renamed {
            undo(&done);
            return Err(err);
        }
        done.push((from.clone(), to.clone()));
    }
    Ok(done)
}

/// Moves each renamed directory back, the last one first.
fn undo(done: &[(PathBuf, PathBuf)]) {
    for (from, to) in done.iter().rev() {
        if let Err(err) = std::fs::rename(to, from) {
            tracing::error!(from = %to.display(), to = %from.display(), %err, "cannot move an archive back");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;
    use crate::db::repos::NewRepo;

    async fn add(pool: &SqlitePool, url: &str, host: &str, owner: &str, name: &str) -> i64 {
        let repo = NewRepo {
            url,
            host,
            owner,
            name,
            source_id: None,
            username: None,
            token_enc: None,
            interval_minutes: None,
            wiki_enabled: true,
        };
        repos::insert(pool, &repo, 1).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn archives_without_the_git_group_move_to_their_path() {
        let pool = open_in_memory().await.unwrap();
        let data = tempfile::tempdir().unwrap();
        let host = "gitlab.example.com";
        // Archived by an earlier version, which left `_git` out everywhere.
        let gitlab = add(
            &pool,
            "https://gitlab.example.com/team/_git/app",
            host,
            "team",
            "app",
        )
        .await;
        let old = repo_url::archive_dir(data.path(), host, "team", "app");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(mirror::wiki_dir(&old)).unwrap();
        // Azure DevOps keeps its path without `_git`.
        let azure = add(
            &pool,
            "https://dev.azure.com/o/p/_git/r",
            "dev.azure.com",
            "o/p",
            "r",
        )
        .await;
        // The new path of this one is taken by another repository.
        add(
            &pool,
            "https://git.example/x/_git/y",
            "git.example",
            "x",
            "y",
        )
        .await;
        add(
            &pool,
            "https://git.example/x/_git/y.git",
            "git.example",
            "x/_git",
            "y",
        )
        .await;

        assert_eq!(git_segment_paths(&pool, data.path()).await.unwrap(), 1);
        let owner = |id| {
            let pool = pool.clone();
            async move { repos::get(&pool, id).await.unwrap().unwrap().owner }
        };
        assert_eq!(owner(gitlab).await, "team/_git");
        assert_eq!(owner(azure).await, "o/p");
        let new = repo_url::archive_dir(data.path(), host, "team/_git", "app");
        assert!(new.join("HEAD").is_file());
        assert!(mirror::wiki_dir(&new).is_dir());
        assert!(!old.exists() && !mirror::wiki_dir(&old).exists());
        // A second start finds nothing to move.
        assert_eq!(git_segment_paths(&pool, data.path()).await.unwrap(), 0);
    }
}
